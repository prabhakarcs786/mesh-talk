//! Milestone 2D: durable, restart-surviving replay protection.
//!
//! Before this module, `MeshNode` deduplicated incoming packets with an in-memory-only
//! `SeenCache` (see `store.rs`): fine within one process lifetime, but a legitimately
//! authenticated packet, captured and replayed by an attacker (or simply redelivered by
//! a relay holding a copy) *after the recipient restarts*, was accepted and delivered
//! again -- there was nothing that remembered "already processed" across a restart.
//! `ReplayStore` closes that hole with a durable (SQLite-backed) table of every
//! `(sender, message_id)` pair already accepted, so a restart doesn't erase that memory.
//!
//! # Why `(sender, message_id)`, not just `message_id`
//! `message_id` is a 16-byte value the *sender* chooses (randomly, but nothing stops a
//! second sender from picking the same bytes, whether by coincidence or by a crafted
//! packet). Keying only on `message_id` would let one sender's message collide with --
//! and potentially suppress -- an unrelated message from a different sender. Keying on
//! the pair makes each sender's own id-space independent of every other sender's.
//!
//! # Why this is safe to insert into
//! `ReplayStore::check_and_insert` must only ever be called *after* `handle_incoming`
//! has already verified the envelope's signature (see `node.rs`) -- inserting an
//! unauthenticated sender/message_id pair would let an attacker "poison" the store with
//! ids that were never legitimately sent, potentially causing a later genuine message
//! reusing (or colliding with) one of those ids to be silently dropped. This ordering is
//! enforced by `node.rs`'s call site, not by this module -- this module has no way to
//! check a signature itself (it doesn't have access to `Identity`/verification), so it's
//! on every caller to only call this after verification. Do not add a new call site to
//! this store without re-reading `node.rs::handle_incoming`'s ordering comment first.
//!
//! # Bounded retention
//! Every entry carries `retain_until` (derived from the envelope's own `expires_at`,
//! plus the same clock-skew tolerance `Envelope::is_expired` already uses) -- once
//! that's passed, the entry is eligible for purging, which happens opportunistically on
//! every new (non-duplicate) insert. A hard row-count cap (independent of expiry) is
//! also enforced as a backstop against an attacker flooding many distinct, individually
//! short-lived but rapidly-produced authenticated messages within their validity window
//! -- oldest-first-seen entries are evicted once the cap is exceeded.
//!
//! # Corruption handling
//! A missing file is simply created. A file that exists but isn't a valid, readable
//! SQLite database (corrupted, truncated, or not a database at all) is quarantined
//! (renamed aside, `<name>.corrupt-<unix_ms>`, never silently deleted) and a fresh store
//! is created in its place -- this must never prevent a node from starting.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::identity::NodeId;

/// Default hard cap on the number of `(sender, message_id)` rows retained, independent
/// of expiry -- a backstop against unbounded storage growth from a flood of distinct,
/// individually-valid authenticated messages. Generous for realistic chat/mesh traffic
/// (this is not a cap on message *rate*, only on how many not-yet-expired ids are
/// remembered at once).
pub const DEFAULT_REPLAY_STORE_CAPACITY: usize = 50_000;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS seen_messages (
        sender BLOB NOT NULL,
        message_id BLOB NOT NULL,
        first_seen_at INTEGER NOT NULL,
        retain_until INTEGER NOT NULL,
        PRIMARY KEY (sender, message_id)
    );
    CREATE INDEX IF NOT EXISTS seen_messages_retain_until ON seen_messages (retain_until);
    CREATE INDEX IF NOT EXISTS seen_messages_first_seen_at ON seen_messages (first_seen_at);
";

/// Durable `(sender, message_id)` de-duplication, used by every `MeshNode` for both
/// "is this addressed-to-me message one I've already processed" AND "is this a packet
/// I've already relayed" -- both checks share the same table, since `handle_incoming`
/// runs this check before either path (see its doc).
pub struct ReplayStore {
    conn: Mutex<Connection>,
    capacity: usize,
}

impl ReplayStore {
    /// In-memory only (not persisted across a restart) -- what `MeshNode::new` uses by
    /// default. Still exercises the exact same SQL-backed logic as the persistent path;
    /// only durability differs. Suitable for tests, `mesh-cli`'s short-lived demo runs,
    /// or any caller that doesn't (yet) need restart-surviving replay protection.
    pub fn in_memory() -> Self {
        Self::in_memory_with_capacity(DEFAULT_REPLAY_STORE_CAPACITY)
    }

    pub fn in_memory_with_capacity(capacity: usize) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite connection must always succeed");
        Self::from_connection(conn, capacity)
    }

    /// Persistent, file-backed store at `path` -- survives a process restart. Creates
    /// the file if missing. If the file exists but is corrupt/unreadable as a SQLite
    /// database, it is quarantined (renamed aside, never silently deleted or
    /// overwritten) and a fresh store is created in its place, so a bad file can never
    /// stop a node from starting -- see the module doc. Returns `(store, was_reset)`:
    /// `was_reset` is `true` only when an existing file was found corrupt and had to be
    /// replaced (never true for a brand-new, never-existed-before path) -- callers
    /// should surface this to the user/app ("replay protection history reset") rather
    /// than silently continuing as if nothing happened, since any `(sender,
    /// message_id)` pairs recorded before the reset are no longer remembered.
    pub fn open(path: impl Into<PathBuf>) -> (Self, bool) {
        Self::open_with_capacity(path, DEFAULT_REPLAY_STORE_CAPACITY)
    }

    pub fn open_with_capacity(path: impl Into<PathBuf>, capacity: usize) -> (Self, bool) {
        let path = path.into();
        match Self::try_open_and_verify(&path, capacity) {
            Ok(store) => (store, false),
            Err(err) => {
                log::warn!(
                    "mesh-core: REPLAY PROTECTION HISTORY RESET -- replay store at {path:?} could not be opened/verified ({err}); quarantining it and starting with an empty replay history. Any (sender, message_id) pairs recorded before this reset are no longer remembered."
                );
                Self::quarantine_corrupt_file(&path);
                let conn = Connection::open(&path).expect("creating a fresh sqlite file at this path must succeed");
                (Self::from_connection(conn, capacity), true)
            }
        }
    }

    fn try_open_and_verify(path: &Path, capacity: usize) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // `Connection::open` alone doesn't prove the file is actually a valid SQLite
        // database (SQLite opens lazily) -- running the schema statement is what
        // actually touches the file's contents and surfaces corruption as an `Err`.
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn), capacity })
    }

    fn quarantine_corrupt_file(path: &Path) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let quarantined = path.with_file_name(format!(
            "{}.corrupt-{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("replay-store.sqlite"),
            now_ms
        ));
        let _ = std::fs::rename(path, quarantined);
    }

    fn from_connection(conn: Connection, capacity: usize) -> Self {
        conn.execute_batch(SCHEMA).expect("schema creation must succeed on a fresh connection");
        Self { conn: Mutex::new(conn), capacity }
    }

    /// Read-only: has `(sender, message_id)` already been durably recorded? Does **not**
    /// mutate anything -- see `mark_seen` for actually recording one. Splitting these
    /// two apart (Milestone 2D.1) is what lets a caller check for a prior replay *before*
    /// doing expensive work (decryption, forwarding) without prematurely treating a
    /// merely-received-and-authenticated packet as "fully processed" -- see `node.rs`'s
    /// Milestone 2D.1 doc section for exactly why that distinction matters (a relay
    /// whose forward attempt fails, or a crash between steps, must not have
    /// permanently -- and incorrectly -- recorded success).
    pub fn contains(&self, sender: &NodeId, message_id: &[u8; 16]) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM seen_messages WHERE sender = ?1 AND message_id = ?2",
            rusqlite::params![&sender[..], &message_id[..]],
            |_| Ok(()),
        )
        .optional()
        .unwrap_or(None)
        .is_some()
    }

    /// Durably records `(sender, message_id)` as processed -- idempotent (calling this
    /// again for the same pair is a harmless no-op). Call this **only once whatever this
    /// store is protecting has actually, successfully happened** -- e.g. only after a
    /// message was successfully decrypted and accepted (for an endpoint), or only after
    /// a relay's forward attempt actually succeeded (for a relay) -- never merely because
    /// a signature verified. See the module doc and `node.rs`'s Milestone 2D.1 section.
    ///
    /// `now_ms`/`expires_at_ms` are passed in (rather than read from the system clock
    /// internally) so callers -- and tests -- have full, deterministic control over
    /// time, matching `Envelope::is_expired`'s own signature.
    pub fn mark_seen(&self, sender: &NodeId, message_id: &[u8; 16], now_ms: u64, expires_at_ms: u64) {
        let conn = self.conn.lock().unwrap();
        let retain_until = expires_at_ms.max(now_ms) + crate::message::CLOCK_SKEW_TOLERANCE_MS;
        let _ = conn.execute(
            "INSERT OR IGNORE INTO seen_messages (sender, message_id, first_seen_at, retain_until) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&sender[..], &message_id[..], now_ms as i64, retain_until as i64],
        );
        Self::purge_expired_locked(&conn, now_ms);
        Self::enforce_capacity_locked(&conn, self.capacity);
    }

    /// Convenience wrapper combining `contains` + `mark_seen` atomically-ish (single
    /// lock acquisition) for callers that don't need the two-phase split -- e.g. marking
    /// this node's own freshly-originated outgoing message as seen immediately (we
    /// authored it, so there's no "did processing actually succeed" question to defer).
    /// Returns `true` if this was newly recorded, `false` if it was already present.
    pub fn check_and_insert(&self, sender: &NodeId, message_id: &[u8; 16], now_ms: u64, expires_at_ms: u64) -> bool {
        let conn = self.conn.lock().unwrap();
        let retain_until = expires_at_ms.max(now_ms) + crate::message::CLOCK_SKEW_TOLERANCE_MS;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO seen_messages (sender, message_id, first_seen_at, retain_until) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![&sender[..], &message_id[..], now_ms as i64, retain_until as i64],
            )
            .unwrap_or(0);
        if inserted == 0 {
            return false; // already present -- a replay
        }
        Self::purge_expired_locked(&conn, now_ms);
        Self::enforce_capacity_locked(&conn, self.capacity);
        true
    }

    fn purge_expired_locked(conn: &Connection, now_ms: u64) {
        let _ = conn.execute("DELETE FROM seen_messages WHERE retain_until < ?1", rusqlite::params![now_ms as i64]);
    }

    fn enforce_capacity_locked(conn: &Connection, capacity: usize) {
        // Oldest-first-seen rows are evicted once the table exceeds `capacity` -- a
        // backstop independent of expiry (see the module doc).
        let _ = conn.execute(
            "DELETE FROM seen_messages WHERE rowid IN (
                SELECT rowid FROM seen_messages ORDER BY first_seen_at ASC
                LIMIT MAX(0, (SELECT COUNT(*) FROM seen_messages) - ?1)
            )",
            rusqlite::params![capacity as i64],
        );
    }

    /// Number of currently-retained `(sender, message_id)` entries. Test/diagnostic use.
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM seen_messages", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// A guaranteed-unique temp directory for one test -- combines a nanosecond
    /// timestamp with a per-process atomic counter, so parallel test execution can
    /// never collide two tests onto the same path the way a millisecond-resolution
    /// timestamp alone occasionally could.
    fn test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-replay-store-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_insert_is_new_second_is_a_replay() {
        let store = ReplayStore::in_memory();
        let sender = Identity::generate().node_id();
        let message_id = [1u8; 16];
        let now = now_millis();

        assert!(store.check_and_insert(&sender, &message_id, now, now + 60_000));
        assert!(!store.check_and_insert(&sender, &message_id, now, now + 60_000));
    }

    #[test]
    fn same_message_id_from_different_senders_does_not_collide() {
        let store = ReplayStore::in_memory();
        let alice = Identity::generate().node_id();
        let bob = Identity::generate().node_id();
        let shared_message_id = [7u8; 16];
        let now = now_millis();

        assert!(store.check_and_insert(&alice, &shared_message_id, now, now + 60_000));
        // Bob using the exact same message_id bytes must NOT be treated as a replay of
        // Alice's message -- they're independent senders.
        assert!(store.check_and_insert(&bob, &shared_message_id, now, now + 60_000));
        // But a genuine repeat from either individual sender still is.
        assert!(!store.check_and_insert(&alice, &shared_message_id, now, now + 60_000));
        assert!(!store.check_and_insert(&bob, &shared_message_id, now, now + 60_000));
    }

    #[test]
    fn store_survives_being_reopened_at_the_same_path() {
        let dir = test_dir("reopen");
        let path = dir.join("replay.sqlite");
        let sender = Identity::generate().node_id();
        let message_id = [2u8; 16];
        let now = now_millis();

        {
            let (store, was_reset) = ReplayStore::open(&path);
            assert!(!was_reset, "a brand-new path should never report a reset");
            assert!(store.check_and_insert(&sender, &message_id, now, now + 60_000));
        }
        // A brand new `ReplayStore`, not the same in-memory object -- proves this
        // actually round-trips through disk, not just an in-process cache.
        let (reopened, was_reset) = ReplayStore::open(&path);
        assert!(!was_reset, "reopening a healthy, previously-written file must not report a reset");
        assert!(
            !reopened.check_and_insert(&sender, &message_id, now, now + 60_000),
            "the same (sender, message_id) must still be recognized as already-seen after reopening"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_entries_are_purged_on_a_later_insert() {
        let store = ReplayStore::in_memory();
        let sender = Identity::generate().node_id();
        let old_message_id = [3u8; 16];
        let new_message_id = [4u8; 16];

        // Inserted with an expiry already in the past relative to `now` used below.
        assert!(store.check_and_insert(&sender, &old_message_id, 1_000, 1_000));
        assert_eq!(store.len(), 1);

        // A much later insert triggers purging of anything whose retain_until (expiry +
        // skew tolerance) has already passed.
        let far_future = 1_000 + crate::message::CLOCK_SKEW_TOLERANCE_MS * 10;
        assert!(store.check_and_insert(&sender, &new_message_id, far_future, far_future + 60_000));
        assert_eq!(store.len(), 1, "the expired entry should have been purged, leaving only the new one");
    }

    #[test]
    fn a_corrupt_store_file_fails_safe_instead_of_panicking() {
        let dir = test_dir("corrupt");
        let path = dir.join("replay.sqlite");
        std::fs::write(&path, b"not a sqlite database at all, just garbage bytes").unwrap();

        let (store, was_reset) = ReplayStore::open(&path);
        assert!(was_reset, "opening a corrupt file must report that a reset happened, not silently continue");
        // Still fully usable afterward.
        let sender = Identity::generate().node_id();
        let now = now_millis();
        assert!(store.check_and_insert(&sender, &[5u8; 16], now, now + 60_000));

        let quarantined_exists = std::fs::read_dir(&dir)
            .unwrap()
            .any(|entry| entry.unwrap().file_name().to_string_lossy().contains("replay.sqlite.corrupt-"));
        assert!(quarantined_exists, "expected the corrupt file to be quarantined, not silently discarded");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Milestone 2D.1: `contains` must never mutate anything -- checking whether a pair
    /// has been seen must not itself count as recording it as seen.
    #[test]
    fn contains_does_not_insert() {
        let store = ReplayStore::in_memory();
        let sender = Identity::generate().node_id();
        let message_id = [6u8; 16];

        assert!(!store.contains(&sender, &message_id));
        assert!(!store.contains(&sender, &message_id), "merely checking must not have inserted anything");
        assert_eq!(store.len(), 0);

        let now = now_millis();
        store.mark_seen(&sender, &message_id, now, now + 60_000);
        assert!(store.contains(&sender, &message_id));
        assert_eq!(store.len(), 1);
    }

    /// `mark_seen` is idempotent -- calling it twice for the same pair must not error or
    /// create a second row.
    #[test]
    fn mark_seen_is_idempotent() {
        let store = ReplayStore::in_memory();
        let sender = Identity::generate().node_id();
        let message_id = [7u8; 16];
        let now = now_millis();

        store.mark_seen(&sender, &message_id, now, now + 60_000);
        store.mark_seen(&sender, &message_id, now, now + 60_000);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_size_is_bounded_by_capacity() {
        let store = ReplayStore::in_memory_with_capacity(5);
        let now = now_millis();
        // Far-future expiry so none of these are purged by the expiry check -- only the
        // capacity backstop should limit growth.
        for i in 0..20u8 {
            let sender = Identity::generate().node_id();
            store.check_and_insert(&sender, &[i; 16], now, now + 999_999_999);
        }
        assert!(store.len() <= 5, "store should never grow past its configured capacity, got {}", store.len());
    }
}
