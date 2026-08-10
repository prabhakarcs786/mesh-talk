//! Milestone 3C: durable inbound message store -- the authority for "has the final
//! recipient durably accepted this specific message", replacing the app-callback-based
//! contract Milestone 3A's `acknowledge_content` relied on.
//!
//! # Why that contract was semantically premature
//! Milestone 3A deferred durable replay-marking for a `Chat` message until the
//! application layer called `acknowledge_content`, on the theory that decryption
//! succeeding is not the same thing as the application durably accepting the content.
//! That reasoning was correct -- but in practice, nothing durable actually backed the
//! app's side of that contract: `mesh-mobile` called it right after appending the
//! message to an in-memory `Vec`/array. So the sequence was:
//!
//! ```text
//! decrypt X ✅ → messages.append(X) (RAM only) → ACK sent → app crashes → RAM gone
//! ```
//!
//! Alice would believe Bob durably accepted the message; Bob actually lost it. The ACK
//! meant "Bob's app briefly held this in memory," not "the recipient durably accepted
//! the message" -- exactly the distinction Milestone 3A was supposed to establish.
//!
//! # The fix: mesh-core owns durable persistence, not just a callback contract
//! Rather than trust every calling application to wire up its own durable chat-history
//! store correctly (and keep it perfectly ordered with the ack), `mesh-core` now *is*
//! the durable inbox: `MeshNode::handle_incoming` persists a fully-reassembled `Chat`
//! message here, in the same synchronous step that decides whether to ack, before
//! returning control to the caller at all. There is no longer an app-facing
//! "acknowledge once you've saved it yourself" API -- there is nothing left for the
//! app to durably save that mesh-core doesn't already durably have.
//!
//! # Ordering `handle_incoming` now follows for a completed `Chat` transfer
//! ```text
//! authenticate → decrypt → reassemble → INSERT OR IGNORE into inbound_messages
//!   → if newly inserted: mark_seen + ACK + surface IncomingEvent::Content
//!   → if already present (duplicate): re-ACK only, do not resurface to the app
//!   → if the durable insert itself failed: NO ack at all -- the sender's own retry
//!     loop (see `delivery_store.rs`) will simply try again later.
//! ```
//!
//! # Why the inbox -- not `ReplayStore` -- is the true dedup authority here
//! `ReplayStore::mark_seen` for this `(sender, message_id)` pair happens right after a
//! successful insert, as a fast early-exit for `handle_incoming`'s next call. But even
//! if a crash happens between the insert committing and that `mark_seen` call (so a
//! restart's `ReplayStore` doesn't yet know about it), a resent packet still decrypts,
//! reassembles, and lands back in `insert_if_absent`, which reports `AlreadyPresent`
//! (its `PRIMARY KEY (sender, message_id)` already has this exact pair) -- so the
//! application layer is never shown the content twice, and a fresh ACK still goes out.
//! `ReplayStore` is an optimization on top of this; `InboxStore` is the actual
//! guarantee.
//!
//! # Milestone 3C.1: content is encrypted at rest
//! `DirectV1` already protects a message in transit; without this, the exact same
//! plaintext this store durably persists would then sit in a plain SQLite file --
//! recoverable via a device backup, a rooted/jailbroken filesystem read, or any other
//! path that reaches app-private storage without going through the app itself. That's
//! a real gap this crate just created by making chat history durable at all, so
//! `content_ciphertext` is never written to disk in plaintext: every row's payload is
//! sealed with XChaCha20-Poly1305 under a caller-supplied `storage_key` before the
//! `INSERT`, with `(sender, message_id, schema_version)` bound as associated data so a
//! ciphertext can't be silently replayed under a different identity/id/format than the
//! one it was actually sealed for. `mesh-core` never generates or stores this key
//! itself -- the caller (see `mesh-mobile`) is expected to hold it in a platform
//! keystore (iOS Keychain / Android Keystore-backed storage), the same way
//! `Identity::seed` already is. A wrong key (or tampered ciphertext) fails closed:
//! `all_messages`/`messages_for_peer` simply skip the row rather than ever returning
//! garbage or panicking.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use rusqlite::Connection;

use crate::identity::NodeId;
use crate::payload::ReceivedContent;

pub const DEFAULT_INBOX_STORE_CAPACITY: usize = 5_000;

/// Bumped only if the encrypted-at-rest encoding itself ever changes shape -- bound as
/// associated data so a ciphertext produced under one version can never be
/// misinterpreted as another.
const STORAGE_SCHEMA_VERSION: u8 = 1;

/// What a durable-insert attempt found.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Genuinely new -- durably persisted just now. Safe to ACK and surface to the app.
    Inserted,
    /// Exact `(sender, message_id)` pair was already durably present. Safe (and
    /// necessary, in case the original ACK never reached the sender) to ACK again, but
    /// must NOT be resurfaced to the application layer as a new message.
    AlreadyPresent,
}

/// One durably-accepted inbound message, as returned by `all_messages`/
/// `messages_for_peer` -- everything a UI needs to hydrate its chat history from disk
/// instead of starting empty.
pub struct InboxMessage {
    pub sender: NodeId,
    pub message_id: [u8; 16],
    pub received_at: u64,
    pub content: ReceivedContent,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS inbound_messages (
        sender BLOB NOT NULL,
        message_id BLOB NOT NULL,
        received_at INTEGER NOT NULL,
        nonce BLOB NOT NULL,
        content_ciphertext BLOB NOT NULL,
        PRIMARY KEY (sender, message_id)
    );
    CREATE INDEX IF NOT EXISTS inbound_messages_received_at ON inbound_messages (received_at);
    CREATE INDEX IF NOT EXISTS inbound_messages_sender_received_at ON inbound_messages (sender, received_at);
";

pub struct InboxStore {
    conn: Mutex<Connection>,
    capacity: usize,
    cipher: XChaCha20Poly1305,
}

impl InboxStore {
    /// In-memory only (never touches disk at all, so at-rest encryption is moot for
    /// this variant) -- a fresh random key is generated internally, since there is
    /// nothing durable for a caller to ever need to reproduce. Fine for tests, or an
    /// app session that hasn't been given a real `inbox_store_path`/storage key yet.
    pub fn in_memory() -> Self {
        Self::in_memory_with_capacity(DEFAULT_INBOX_STORE_CAPACITY)
    }

    pub fn in_memory_with_capacity(capacity: usize) -> Self {
        let mut random_key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut random_key);
        let conn = Connection::open_in_memory().expect("in-memory sqlite connection must always succeed");
        Self::from_connection(conn, capacity, random_key)
    }

    /// Persistent, file-backed store at `path`, with content encrypted at rest under
    /// `storage_key` (see the module doc's "Milestone 3C.1" section) -- same
    /// corruption-handling contract as `DeliveryStore`/`ForwardStore`/
    /// `ReplayStore::open`: a missing file is created, a corrupt one is quarantined
    /// (renamed aside, never silently deleted) and replaced with a fresh store,
    /// reported back via the returned `was_reset` flag. The caller is responsible for
    /// keeping `storage_key` itself durable (e.g. in the iOS Keychain / Android
    /// Keystore-backed storage) and passing back the *same* key on every reopen --
    /// this store has no way to detect "wrong key" versus "no data yet" up front; a
    /// wrong key just makes every row fail to decrypt (see `all_messages`'s doc).
    pub fn open(path: impl Into<PathBuf>, storage_key: [u8; 32]) -> (Self, bool) {
        Self::open_with_capacity(path, storage_key, DEFAULT_INBOX_STORE_CAPACITY)
    }

    pub fn open_with_capacity(path: impl Into<PathBuf>, storage_key: [u8; 32], capacity: usize) -> (Self, bool) {
        let path = path.into();
        match Self::try_open_and_verify(&path, storage_key, capacity) {
            Ok(store) => (store, false),
            Err(err) => {
                log::warn!(
                    "mesh-core: INBOX STATE RESET -- inbox store at {path:?} could not be opened/verified ({err}); quarantining it and starting empty. Chat history recorded before this reset is not recoverable from this file."
                );
                Self::quarantine_corrupt_file(&path);
                let conn = Connection::open(&path).expect("creating a fresh sqlite file at this path must succeed");
                (Self::from_connection(conn, capacity, storage_key), true)
            }
        }
    }

    fn try_open_and_verify(path: &Path, storage_key: [u8; 32], capacity: usize) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self::build(conn, capacity, storage_key))
    }

    fn quarantine_corrupt_file(path: &Path) {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let quarantined = path.with_file_name(format!(
            "{}.corrupt-{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("inbox-store.sqlite"),
            now_ms
        ));
        let _ = std::fs::rename(path, quarantined);
    }

    fn from_connection(conn: Connection, capacity: usize, storage_key: [u8; 32]) -> Self {
        conn.execute_batch(SCHEMA).expect("schema creation must succeed on a fresh connection");
        Self::build(conn, capacity, storage_key)
    }

    fn build(conn: Connection, capacity: usize, storage_key: [u8; 32]) -> Self {
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&storage_key));
        Self { conn: Mutex::new(conn), capacity, cipher }
    }

    /// Associated data binding a stored ciphertext to exactly the `(sender,
    /// message_id)` pair (and encoding version) it was sealed for -- so a ciphertext
    /// can never be silently accepted under a different identity/id than the one
    /// recorded alongside it, even if someone tampered with the plaintext columns
    /// directly in the database file.
    fn associated_data(sender: &NodeId, message_id: &[u8; 16]) -> Vec<u8> {
        let mut aad = Vec::with_capacity(32 + 16 + 1);
        aad.extend_from_slice(sender);
        aad.extend_from_slice(message_id);
        aad.push(STORAGE_SCHEMA_VERSION);
        aad
    }

    fn encrypt_content(&self, sender: &NodeId, message_id: &[u8; 16], content: &ReceivedContent) -> anyhow::Result<([u8; 24], Vec<u8>)> {
        let plaintext = bincode::serialize(content)?;
        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let aad = Self::associated_data(sender, message_id);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce_bytes), Payload { msg: &plaintext, aad: &aad })
            .map_err(|_| anyhow::anyhow!("failed to encrypt message content for storage"))?;
        Ok((nonce_bytes, ciphertext))
    }

    /// `None` on any failure -- wrong `storage_key`, tampered ciphertext/AAD, or
    /// malformed inner content -- never a panic. This is the fail-closed behavior the
    /// module doc promises: a wrong key just makes rows silently unreadable, not
    /// garbage data or a crash.
    fn decrypt_content(&self, sender: &NodeId, message_id: &[u8; 16], nonce_bytes: &[u8], ciphertext: &[u8]) -> Option<ReceivedContent> {
        let nonce_bytes: [u8; 24] = nonce_bytes.try_into().ok()?;
        let aad = Self::associated_data(sender, message_id);
        let plaintext = self.cipher.decrypt(XNonce::from_slice(&nonce_bytes), Payload { msg: ciphertext, aad: &aad }).ok()?;
        bincode::deserialize(&plaintext).ok()
    }

    /// Durably persists `content` as having arrived from `sender` with `message_id`,
    /// unless that exact pair is already present. See the module doc for exactly how
    /// `MeshNode::handle_incoming` uses the three possible outcomes -- in particular,
    /// callers MUST NOT send a `DeliveryAck` when this returns `Err`.
    pub fn insert_if_absent(&self, sender: &NodeId, message_id: &[u8; 16], received_at: u64, content: &ReceivedContent) -> anyhow::Result<InsertOutcome> {
        let (nonce, ciphertext) = self.encrypt_content(sender, message_id, content)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO inbound_messages (sender, message_id, received_at, nonce, content_ciphertext) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![&sender[..], &message_id[..], received_at as i64, &nonce[..], ciphertext],
        )?;
        Self::enforce_capacity_locked(&conn, self.capacity);
        if changed > 0 {
            Ok(InsertOutcome::Inserted)
        } else {
            Ok(InsertOutcome::AlreadyPresent)
        }
    }

    /// Whether `(sender, message_id)` has already been durably accepted. Test/diagnostic
    /// use -- `handle_incoming` itself relies on `insert_if_absent`'s return value
    /// rather than a separate contains-then-insert (which would be a TOCTOU race).
    pub fn contains(&self, sender: &NodeId, message_id: &[u8; 16]) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM inbound_messages WHERE sender = ?1 AND message_id = ?2",
            rusqlite::params![&sender[..], &message_id[..]],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Every durably-accepted message, oldest first -- for a UI to hydrate its chat
    /// history from disk on launch (Milestone 3C) instead of starting from an empty
    /// in-memory list. Silently skips any row that fails to decrypt/deserialize --
    /// either the wrong `storage_key` was supplied, the ciphertext/AAD was tampered
    /// with, or (should never happen for data this store itself wrote) the content is
    /// otherwise malformed -- fails safe rather than ever panicking or returning
    /// garbage, but note a wrong key means this silently returns an empty (or
    /// partial) history rather than an explicit error; callers that need to
    /// distinguish "no history" from "wrong key" should track that separately (e.g.
    /// compare `len()` against how many rows decrypted).
    ///
    /// Loads the entire table -- fine for the message volumes this app deals with
    /// today, but callers with a large history should prefer `messages_for_peer` for
    /// a single conversation's paginated view instead of hydrating everything up
    /// front (see that method's doc).
    pub fn all_messages(&self) -> Vec<InboxMessage> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT sender, message_id, received_at, nonce, content_ciphertext FROM inbound_messages ORDER BY received_at ASC")
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                let sender: Vec<u8> = row.get(0)?;
                let message_id: Vec<u8> = row.get(1)?;
                let received_at: i64 = row.get(2)?;
                let nonce: Vec<u8> = row.get(3)?;
                let ciphertext: Vec<u8> = row.get(4)?;
                Ok((sender, message_id, received_at, nonce, ciphertext))
            })
            .unwrap();

        rows.filter_map(|r| r.ok())
            .filter_map(|(sender, message_id, received_at, nonce, ciphertext)| {
                let sender: NodeId = sender.try_into().ok()?;
                let message_id: [u8; 16] = message_id.try_into().ok()?;
                let content = self.decrypt_content(&sender, &message_id, &nonce, &ciphertext)?;
                Some(InboxMessage { sender, message_id, received_at: received_at as u64, content })
            })
            .collect()
    }

    /// A single conversation's messages, newest first, bounded to `limit` -- the
    /// paginated alternative to `all_messages` a UI should prefer once a
    /// conversation's history grows large (Milestone 3C.1: don't hydrate the entire
    /// database up front just to show the latest screenful of one thread).
    ///
    /// Call with `before_received_at: None` for the most recent page; pass the
    /// `received_at` of the oldest message from the previous page to fetch the next
    /// (older) page, e.g.:
    ///
    /// ```text
    /// let page1 = store.messages_for_peer(&bob, None, 50);
    /// let cursor = page1.last().map(|m| m.received_at);
    /// let page2 = store.messages_for_peer(&bob, cursor, 50);
    /// ```
    ///
    /// Same fail-safe decrypt behavior as `all_messages` -- an unreadable row is
    /// skipped, never surfaced as an error or garbage.
    pub fn messages_for_peer(&self, peer: &NodeId, before_received_at: Option<u64>, limit: usize) -> Vec<InboxMessage> {
        let conn = self.conn.lock().unwrap();
        let cursor = before_received_at.map(|v| v as i64).unwrap_or(i64::MAX);
        let mut stmt = conn
            .prepare(
                "SELECT sender, message_id, received_at, nonce, content_ciphertext FROM inbound_messages
                 WHERE sender = ?1 AND received_at < ?2 ORDER BY received_at DESC LIMIT ?3",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![&peer[..], cursor, limit as i64], |row| {
                let sender: Vec<u8> = row.get(0)?;
                let message_id: Vec<u8> = row.get(1)?;
                let received_at: i64 = row.get(2)?;
                let nonce: Vec<u8> = row.get(3)?;
                let ciphertext: Vec<u8> = row.get(4)?;
                Ok((sender, message_id, received_at, nonce, ciphertext))
            })
            .unwrap();

        rows.filter_map(|r| r.ok())
            .filter_map(|(sender, message_id, received_at, nonce, ciphertext)| {
                let sender: NodeId = sender.try_into().ok()?;
                let message_id: [u8; 16] = message_id.try_into().ok()?;
                let content = self.decrypt_content(&sender, &message_id, &nonce, &ciphertext)?;
                Some(InboxMessage { sender, message_id, received_at: received_at as u64, content })
            })
            .collect()
    }

    fn enforce_capacity_locked(conn: &Connection, capacity: usize) {
        // Oldest-first eviction, same backstop rationale as the other stores -- bounds
        // storage growth from an attacker (or a bug) generating far more inbound
        // messages than could ever legitimately need to be kept. Unlike the other
        // stores, every row here is equally "important" (real chat history), so there's
        // no terminal-state-first preference -- just oldest first.
        let _ = conn.execute(
            "DELETE FROM inbound_messages WHERE rowid IN (
                SELECT rowid FROM inbound_messages ORDER BY received_at ASC
                LIMIT MAX(0, (SELECT COUNT(*) FROM inbound_messages) - ?1)
            )",
            rusqlite::params![capacity as i64],
        );
    }

    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM inbound_messages", [], |row| row.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test-only hook to simulate a durable-write failure (e.g. a corrupted/unwritable
    /// database) without needing exotic filesystem tricks -- drops the underlying table
    /// out from under this connection, so the next `insert_if_absent` call fails with a
    /// genuine `rusqlite` error, exactly like a real I/O or corruption failure would.
    #[cfg(test)]
    pub fn break_for_test(&self) {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("DROP TABLE inbound_messages").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn now_millis() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
    }

    fn test_sender() -> NodeId {
        Identity::generate().node_id()
    }

    fn test_key(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-inbox-store-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_insert_is_new_second_is_a_duplicate() {
        let store = InboxStore::in_memory();
        let sender = test_sender();
        let message_id = [1u8; 16];
        let content = ReceivedContent::Text("hello".to_string());

        assert_eq!(store.insert_if_absent(&sender, &message_id, now_millis(), &content).unwrap(), InsertOutcome::Inserted);
        assert_eq!(store.insert_if_absent(&sender, &message_id, now_millis(), &content).unwrap(), InsertOutcome::AlreadyPresent);
        assert_eq!(store.len(), 1, "a duplicate insert must not create a second row");
    }

    #[test]
    fn same_message_id_from_different_senders_does_not_collide() {
        let store = InboxStore::in_memory();
        let (alice, bob) = (test_sender(), test_sender());
        let message_id = [2u8; 16];
        let content = ReceivedContent::Text("hi".to_string());

        assert_eq!(store.insert_if_absent(&alice, &message_id, now_millis(), &content).unwrap(), InsertOutcome::Inserted);
        assert_eq!(store.insert_if_absent(&bob, &message_id, now_millis(), &content).unwrap(), InsertOutcome::Inserted);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn all_messages_round_trips_content_in_received_order() {
        let store = InboxStore::in_memory();
        let sender = test_sender();
        let now = now_millis();
        store.insert_if_absent(&sender, &[3u8; 16], now, &ReceivedContent::Text("first".to_string())).unwrap();
        store.insert_if_absent(&sender, &[4u8; 16], now + 1, &ReceivedContent::Text("second".to_string())).unwrap();

        let all = store.all_messages();
        assert_eq!(all.len(), 2);
        assert!(matches!(&all[0].content, ReceivedContent::Text(t) if t == "first"));
        assert!(matches!(&all[1].content, ReceivedContent::Text(t) if t == "second"));
    }

    #[test]
    fn broken_store_fails_the_insert_instead_of_silently_succeeding() {
        let store = InboxStore::in_memory();
        store.break_for_test();
        let result = store.insert_if_absent(&test_sender(), &[5u8; 16], now_millis(), &ReceivedContent::Text("x".to_string()));
        assert!(result.is_err(), "a durable-write failure must be reported as an error, never silently treated as success");
    }

    #[test]
    fn persists_across_reopen_at_the_same_path() {
        let dir = unique_test_dir("reopen");
        let path = dir.join("inbox.sqlite");
        let sender = test_sender();
        let message_id = [6u8; 16];
        let key = test_key(7);

        {
            let (store, was_reset) = InboxStore::open(&path, key);
            assert!(!was_reset);
            store.insert_if_absent(&sender, &message_id, now_millis(), &ReceivedContent::Text("persisted".to_string())).unwrap();
        }

        let (reopened, was_reset) = InboxStore::open(&path, key);
        assert!(!was_reset);
        assert!(reopened.contains(&sender, &message_id));
        assert_eq!(reopened.all_messages().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_replaced_with_a_fresh_empty_store() {
        let dir = unique_test_dir("corrupt");
        let path = dir.join("inbox.sqlite");
        std::fs::write(&path, b"not a sqlite file").unwrap();

        let (store, was_reset) = InboxStore::open(&path, test_key(1));
        assert!(was_reset);
        assert!(store.is_empty());
        let quarantined_exists = std::fs::read_dir(&dir).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().contains("inbox.sqlite.corrupt-"));
        assert!(quarantined_exists, "the corrupt original file must be quarantined (renamed aside), not silently deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Milestone 3C.1: the raw bytes on disk must never contain the plaintext message
    /// text -- otherwise "durable chat history" would just mean "plaintext chat
    /// history sitting in a file," defeating the point of encrypting anything in
    /// transit at all.
    #[test]
    fn raw_file_does_not_contain_plaintext_marker() {
        let dir = unique_test_dir("plaintext-marker");
        let path = dir.join("inbox.sqlite");
        let marker = "TOTALLY-SECRET-MARKER-STRING-12345";

        {
            let (store, _) = InboxStore::open(&path, test_key(9));
            store.insert_if_absent(&test_sender(), &[8u8; 16], now_millis(), &ReceivedContent::Text(marker.to_string())).unwrap();
        }

        let raw = std::fs::read(&path).unwrap();
        let haystack = String::from_utf8_lossy(&raw);
        assert!(!haystack.contains(marker), "message plaintext must never appear in the raw database file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reopening with a *different* key than the data was written under must fail
    /// closed -- the row simply becomes unreadable, never garbage content and never a
    /// panic.
    #[test]
    fn wrong_storage_key_fails_closed() {
        let dir = unique_test_dir("wrong-key");
        let path = dir.join("inbox.sqlite");
        let sender = test_sender();

        {
            let (store, _) = InboxStore::open(&path, test_key(1));
            store.insert_if_absent(&sender, &[10u8; 16], now_millis(), &ReceivedContent::Text("hello".to_string())).unwrap();
        }

        let (reopened_wrong_key, _) = InboxStore::open(&path, test_key(2));
        assert!(reopened_wrong_key.all_messages().is_empty(), "a wrong storage key must not decrypt any content");
        // The row still physically exists (dedup/contains still work on the plaintext
        // sender/message_id columns) -- only the *content* is unreadable.
        assert!(reopened_wrong_key.contains(&sender, &[10u8; 16]));

        let (reopened_right_key, _) = InboxStore::open(&path, test_key(1));
        assert_eq!(reopened_right_key.all_messages().len(), 1, "the correct key must still decrypt it afterward");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stored ciphertext tampered with directly in the database must be rejected
    /// (AEAD authentication failure), not silently accepted as different content.
    #[test]
    fn modified_stored_ciphertext_is_rejected() {
        let dir = unique_test_dir("tampered-ciphertext");
        let path = dir.join("inbox.sqlite");
        let sender = test_sender();
        let key = test_key(3);

        {
            let (store, _) = InboxStore::open(&path, key);
            store.insert_if_absent(&sender, &[11u8; 16], now_millis(), &ReceivedContent::Text("hello".to_string())).unwrap();
        }

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE inbound_messages SET content_ciphertext = content_ciphertext || X'00'", []).unwrap();
        }

        let (reopened, _) = InboxStore::open(&path, key);
        assert!(reopened.all_messages().is_empty(), "tampered ciphertext must fail AEAD authentication, not be silently accepted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn messages_for_peer_paginates_newest_first() {
        let store = InboxStore::in_memory();
        let (alice, bob) = (test_sender(), test_sender());
        let now = now_millis();
        for i in 0..5u64 {
            store.insert_if_absent(&alice, &[i as u8; 16], now + i, &ReceivedContent::Text(format!("alice-{i}"))).unwrap();
        }
        store.insert_if_absent(&bob, &[99u8; 16], now, &ReceivedContent::Text("bob-only".to_string())).unwrap();

        let page1 = store.messages_for_peer(&alice, None, 2);
        assert_eq!(page1.len(), 2);
        assert!(matches!(&page1[0].content, ReceivedContent::Text(t) if t == "alice-4"));
        assert!(matches!(&page1[1].content, ReceivedContent::Text(t) if t == "alice-3"));

        let page2 = store.messages_for_peer(&alice, Some(page1[1].received_at), 2);
        assert_eq!(page2.len(), 2);
        assert!(matches!(&page2[0].content, ReceivedContent::Text(t) if t == "alice-2"));
        assert!(matches!(&page2[1].content, ReceivedContent::Text(t) if t == "alice-1"));

        // Bob's message never leaks into Alice's paginated view.
        assert!(store.messages_for_peer(&alice, None, 100).iter().all(|m| m.sender == alice));
    }
}
