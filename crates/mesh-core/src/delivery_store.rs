//! Milestone 3A: durable outgoing delivery state for DirectV1 chat text messages.
//!
//! `ReplayStore` (Milestone 2D/2D.1) answers "have I already authenticated/handled this
//! exact incoming packet" -- a *security* question. `DeliveryStore` answers a completely
//! different, *reliability* question: "has this message I originated actually been
//! durably accepted by its recipient yet, and if not, when should I try transmitting it
//! again?" Conflating these two concerns into one `seen`/`not seen` boolean is exactly
//! the mistake Milestone 2D.1 backed away from for incoming packets -- this module exists
//! so outgoing delivery reliability doesn't repeat it.
//!
//! # Scope (deliberately narrow for this milestone)
//! Only single-chunk DirectV1 text messages go through this reliable path today (see
//! `MeshNode::send_reliable_text`) -- multi-chunk transfers (attachments, long text) and
//! `ChannelV1` traffic (broadcast, call signaling/frames) are unaffected and keep their
//! existing best-effort behavior. Extending reliable delivery to attachments is real
//! future work (resumable files), not something to bolt on here.
//!
//! # State machine
//! ```text
//! enqueue()              record_attempt()           acknowledge()
//! (persisted BEFORE  ──▶  QUEUED  ──────────────▶  SENT  ──────────────▶  ACKNOWLEDGED
//!  the first send                                    │                    (terminal)
//!  attempt, so a                                      │ expire_overdue()
//!  crash before the                                   ▼
//!  first send still                                EXPIRED
//!  has something to                                (terminal)
//!  retry after restart)
//! ```
//! There is deliberately no persisted `TRANSMITTING` state: an in-flight send attempt is
//! purely a transient, in-memory thing (a single `flood()` call) -- if the process
//! crashes mid-send, the durable state on disk is still whatever it was before that
//! attempt started (`QUEUED` or `SENT`), which is exactly what should be retried on
//! restart. Persisting a "currently transmitting" state that could never be trusted
//! across a restart anyway would only add complexity for no benefit.
//!
//! # Retrying the *same* logical message
//! `enqueue` persists the already-fully-encrypted `Envelope` bytes once; every retry
//! retransmits those exact bytes verbatim (same `message_id`, same ciphertext, same
//! nonce) rather than re-encrypting or minting a new id. This is what lets the recipient
//! deduplicate retries as "the same message" instead of seeing an unbounded stream of
//! distinct-looking resends, and avoids ever reusing an AEAD nonce under a different
//! plaintext.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rand::RngCore;
use rusqlite::Connection;

use crate::identity::NodeId;

pub const DEFAULT_DELIVERY_STORE_CAPACITY: usize = 10_000;

/// How long to wait before the *first* retry, and the base of the exponential backoff
/// applied to every subsequent one -- see `next_retry_delay_ms`.
const RETRY_BASE_MS: u64 = 1_000;
/// Backoff never waits longer than this between attempts, however many attempts have
/// already happened -- otherwise a message that's been failing for a long time would
/// wait for hours before its next (still probably doomed, given `expires_at`) attempt.
const RETRY_MAX_DELAY_MS: u64 = 60_000;

/// Where one outbound message currently stands. See the module doc's state diagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundState {
    /// Persisted, but no transmission attempt has been recorded as completed yet.
    Queued,
    /// At least one transmission attempt has been made; still waiting for an ack.
    Sent,
    /// A valid `DeliveryAck` was received -- terminal, no further retries.
    Acknowledged,
    /// `expires_at` passed before an ack arrived -- terminal, no further retries.
    Expired,
}

impl OutboundState {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(OutboundState::Queued),
            "sent" => Some(OutboundState::Sent),
            "acknowledged" => Some(OutboundState::Acknowledged),
            "expired" => Some(OutboundState::Expired),
            _ => None,
        }
    }
}

/// One message that's due to be (re)transmitted -- returned by `due_for_attempt`.
pub struct DueMessage {
    pub message_id: [u8; 16],
    pub recipient: NodeId,
    pub envelope_bytes: Vec<u8>,
    pub attempts: u32,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS outbound_messages (
        message_id BLOB PRIMARY KEY,
        recipient BLOB NOT NULL,
        envelope_bytes BLOB NOT NULL,
        state TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        next_attempt_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS outbound_messages_next_attempt ON outbound_messages (state, next_attempt_at);
";

pub struct DeliveryStore {
    conn: Mutex<Connection>,
    capacity: usize,
}

impl DeliveryStore {
    pub fn in_memory() -> Self {
        Self::in_memory_with_capacity(DEFAULT_DELIVERY_STORE_CAPACITY)
    }

    pub fn in_memory_with_capacity(capacity: usize) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite connection must always succeed");
        Self::from_connection(conn, capacity)
    }

    /// Persistent, file-backed store at `path` -- survives a process restart, the same
    /// corruption-handling contract as `ReplayStore::open`/`ContactStore::open`: a
    /// missing file is created, a corrupt one is quarantined (renamed aside, never
    /// silently deleted) and replaced with a fresh store, and this is logged/reported
    /// back via the returned `was_reset` flag rather than silently swallowed.
    pub fn open(path: impl Into<PathBuf>) -> (Self, bool) {
        Self::open_with_capacity(path, DEFAULT_DELIVERY_STORE_CAPACITY)
    }

    pub fn open_with_capacity(path: impl Into<PathBuf>, capacity: usize) -> (Self, bool) {
        let path = path.into();
        match Self::try_open_and_verify(&path, capacity) {
            Ok(store) => (store, false),
            Err(err) => {
                log::warn!(
                    "mesh-core: DELIVERY STATE RESET -- delivery store at {path:?} could not be opened/verified ({err}); quarantining it and starting empty. Any not-yet-acknowledged outgoing messages recorded before this reset will not be retried automatically."
                );
                Self::quarantine_corrupt_file(&path);
                let conn = Connection::open(&path).expect("creating a fresh sqlite file at this path must succeed");
                (Self::from_connection(conn, capacity), true)
            }
        }
    }

    fn try_open_and_verify(path: &Path, capacity: usize) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn), capacity })
    }

    fn quarantine_corrupt_file(path: &Path) {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let quarantined = path.with_file_name(format!(
            "{}.corrupt-{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("delivery-store.sqlite"),
            now_ms
        ));
        let _ = std::fs::rename(path, quarantined);
    }

    fn from_connection(conn: Connection, capacity: usize) -> Self {
        conn.execute_batch(SCHEMA).expect("schema creation must succeed on a fresh connection");
        Self { conn: Mutex::new(conn), capacity }
    }

    /// Persists a new outbound message as `Queued`, due for its first transmission
    /// attempt immediately (`next_attempt_at = now_ms`). Must be called -- and the
    /// caller must wait for it to return -- *before* the first transmission attempt, so
    /// a crash between persisting and actually sending still leaves something to retry
    /// after a restart.
    pub fn enqueue(&self, message_id: &[u8; 16], recipient: &NodeId, envelope_bytes: &[u8], now_ms: u64, expires_at_ms: u64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO outbound_messages (message_id, recipient, envelope_bytes, state, attempts, next_attempt_at, expires_at, created_at)
             VALUES (?1, ?2, ?3, 'queued', 0, ?4, ?5, ?4)",
            rusqlite::params![&message_id[..], &recipient[..], envelope_bytes, now_ms as i64, expires_at_ms as i64],
        );
        Self::enforce_capacity_locked(&conn, self.capacity);
    }

    /// Every not-yet-terminal message whose `next_attempt_at` has passed and whose
    /// `expires_at` has not -- i.e. everything due for a (re)transmission attempt right
    /// now. Does not itself change any state -- pair with `record_attempt` once the
    /// caller has actually made the attempt.
    pub fn due_for_attempt(&self, now_ms: u64) -> Vec<DueMessage> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT message_id, recipient, envelope_bytes, attempts FROM outbound_messages
                 WHERE state IN ('queued', 'sent') AND next_attempt_at <= ?1 AND expires_at > ?1",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![now_ms as i64], |row| {
                let message_id: Vec<u8> = row.get(0)?;
                let recipient: Vec<u8> = row.get(1)?;
                let envelope_bytes: Vec<u8> = row.get(2)?;
                let attempts: i64 = row.get(3)?;
                Ok((message_id, recipient, envelope_bytes, attempts))
            })
            .unwrap();

        rows.filter_map(|r| r.ok())
            .filter_map(|(message_id, recipient, envelope_bytes, attempts)| {
                let message_id: [u8; 16] = message_id.try_into().ok()?;
                let recipient: NodeId = recipient.try_into().ok()?;
                Some(DueMessage { message_id, recipient, envelope_bytes, attempts: attempts as u32 })
            })
            .collect()
    }

    /// Records that a transmission attempt for `message_id` was just made -- transitions
    /// `Queued` -> `Sent` (or keeps it `Sent`), increments the attempt counter, and
    /// schedules the next attempt using exponential backoff with jitter (see
    /// `next_retry_delay_ms`). A no-op if the message is already in a terminal state
    /// (`Acknowledged`/`Expired`) -- e.g. an ack race with an in-flight retry.
    pub fn record_attempt(&self, message_id: &[u8; 16], now_ms: u64) {
        let conn = self.conn.lock().unwrap();
        let attempts: Option<i64> = conn
            .query_row(
                "SELECT attempts FROM outbound_messages WHERE message_id = ?1 AND state IN ('queued', 'sent')",
                rusqlite::params![&message_id[..]],
                |row| row.get(0),
            )
            .ok();
        let Some(attempts) = attempts else { return };
        let new_attempts = attempts as u32 + 1;
        let next_attempt_at = now_ms + next_retry_delay_ms(new_attempts);
        let _ = conn.execute(
            "UPDATE outbound_messages SET state = 'sent', attempts = ?2, next_attempt_at = ?3 WHERE message_id = ?1",
            rusqlite::params![&message_id[..], new_attempts as i64, next_attempt_at as i64],
        );
    }

    /// Marks `message_id` as durably acknowledged -- but **only** if `acking_node`
    /// matches the `recipient` this message was originally enqueued for. This is what
    /// makes a forged or misattributed ack harmless: a validly-signed `DeliveryAck` from
    /// some other node, or one referencing a `message_id` addressed to someone else, is
    /// silently ignored (`false`) rather than prematurely stopping retries meant for the
    /// real recipient. Idempotent -- a duplicate, correctly-attributed ack is a harmless
    /// no-op that still returns `true`. Returns `false` if `message_id` isn't known at
    /// all, or is known but `acking_node` doesn't match its recorded recipient.
    pub fn acknowledge_from(&self, message_id: &[u8; 16], acking_node: &NodeId) -> bool {
        let conn = self.conn.lock().unwrap();
        let updated = conn
            .execute(
                "UPDATE outbound_messages SET state = 'acknowledged' WHERE message_id = ?1 AND recipient = ?2",
                rusqlite::params![&message_id[..], &acking_node[..]],
            )
            .unwrap_or(0);
        updated > 0
    }

    /// Transitions anything still `Queued`/`Sent` whose `expires_at` has passed into the
    /// terminal `Expired` state, so it stops being returned by `due_for_attempt`.
    pub fn expire_overdue(&self, now_ms: u64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE outbound_messages SET state = 'expired' WHERE state IN ('queued', 'sent') AND expires_at <= ?1",
            rusqlite::params![now_ms as i64],
        );
    }

    fn enforce_capacity_locked(conn: &Connection, capacity: usize) {
        // Oldest-first-created rows are evicted once the table exceeds `capacity` --
        // same backstop rationale as `ReplayStore`'s: bounds storage growth from an
        // attacker (or a bug) generating far more outgoing messages than could ever
        // legitimately be pending at once. Prefers evicting terminal (acknowledged/
        // expired) rows first, since those carry no further obligation.
        let _ = conn.execute(
            "DELETE FROM outbound_messages WHERE rowid IN (
                SELECT rowid FROM outbound_messages
                ORDER BY (state IN ('acknowledged', 'expired')) DESC, created_at ASC
                LIMIT MAX(0, (SELECT COUNT(*) FROM outbound_messages) - ?1)
            )",
            rusqlite::params![capacity as i64],
        );
    }

    /// Current state of `message_id`, or `None` if it was never enqueued (or has since
    /// been evicted by the capacity backstop). Test/diagnostic use.
    pub fn state_of(&self, message_id: &[u8; 16]) -> Option<OutboundState> {
        let conn = self.conn.lock().unwrap();
        let state: Option<String> = conn
            .query_row("SELECT state FROM outbound_messages WHERE message_id = ?1", rusqlite::params![&message_id[..]], |row| row.get(0))
            .ok();
        state.and_then(|s| OutboundState::from_str(&s))
    }

    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM outbound_messages", [], |row| row.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Exponential backoff with jitter: `attempts` doubles the base delay each time (capped),
/// plus up to 25% random jitter so many nodes retrying at once don't all synchronize and
/// hammer the mesh simultaneously (the "thundering herd" problem).
fn next_retry_delay_ms(attempts: u32) -> u64 {
    let exponential = RETRY_BASE_MS.saturating_mul(1u64 << attempts.min(6));
    let capped = exponential.min(RETRY_MAX_DELAY_MS);
    let mut jitter_bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut jitter_bytes);
    let jitter_fraction = u64::from_le_bytes(jitter_bytes) % (capped / 4 + 1);
    capped + jitter_fraction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn now_millis() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-delivery-store-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enqueued_message_is_immediately_due() {
        let store = DeliveryStore::in_memory();
        let recipient = Identity::generate().node_id();
        let message_id = [1u8; 16];
        let now = now_millis();

        store.enqueue(&message_id, &recipient, b"envelope-bytes", now, now + 60_000);
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Queued));

        let due = store.due_for_attempt(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message_id, message_id);
        assert_eq!(due[0].envelope_bytes, b"envelope-bytes");
    }

    #[test]
    fn record_attempt_transitions_to_sent_and_schedules_a_later_retry() {
        let store = DeliveryStore::in_memory();
        let recipient = Identity::generate().node_id();
        let message_id = [2u8; 16];
        let now = now_millis();
        // Long expiry so this test's "check later" assertion below isn't itself past
        // the message's own expiry (which `due_for_attempt` also excludes).
        store.enqueue(&message_id, &recipient, b"bytes", now, now + 600_000);

        store.record_attempt(&message_id, now);
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Sent));
        // Not due again immediately -- backoff pushed the next attempt into the future.
        assert!(store.due_for_attempt(now).is_empty());
        assert!(!store.due_for_attempt(now + 120_000).is_empty());
    }

    #[test]
    fn acknowledge_stops_future_retries() {
        let store = DeliveryStore::in_memory();
        let recipient = Identity::generate().node_id();
        let message_id = [3u8; 16];
        let now = now_millis();
        store.enqueue(&message_id, &recipient, b"bytes", now, now + 60_000);
        store.record_attempt(&message_id, now);

        assert!(store.acknowledge_from(&message_id, &recipient));
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Acknowledged));
        assert!(store.due_for_attempt(now + 999_999).is_empty());
    }

    #[test]
    fn acknowledge_is_idempotent_and_harmless_for_unknown_ids() {
        let store = DeliveryStore::in_memory();
        // Never enqueued at all.
        assert!(!store.acknowledge_from(&[9u8; 16], &Identity::generate().node_id()));
        assert!(store.is_empty());

        let recipient = Identity::generate().node_id();
        let message_id = [4u8; 16];
        let now = now_millis();
        store.enqueue(&message_id, &recipient, b"bytes", now, now + 60_000);
        assert!(store.acknowledge_from(&message_id, &recipient));
        assert!(store.acknowledge_from(&message_id, &recipient)); // duplicate ack
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Acknowledged));
    }

    /// A `DeliveryAck` claiming a `message_id` this store enqueued for a *different*
    /// recipient must be rejected -- otherwise a forged or misattributed ack could
    /// prematurely stop retries actually meant for the real recipient.
    #[test]
    fn acknowledge_from_the_wrong_node_is_rejected() {
        let store = DeliveryStore::in_memory();
        let real_recipient = Identity::generate().node_id();
        let mallory = Identity::generate().node_id();
        let message_id = [10u8; 16];
        let now = now_millis();
        store.enqueue(&message_id, &real_recipient, b"bytes", now, now + 60_000);

        assert!(!store.acknowledge_from(&message_id, &mallory), "an ack from the wrong node must not be accepted");
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Queued), "state must be unaffected by a mismatched ack");
        assert!(!store.due_for_attempt(now).is_empty(), "the message must remain retryable");

        assert!(store.acknowledge_from(&message_id, &real_recipient), "the real recipient's ack must still work");
    }

    #[test]
    fn expire_overdue_transitions_past_expiry_messages_and_stops_retries() {
        let store = DeliveryStore::in_memory();
        let recipient = Identity::generate().node_id();
        let message_id = [5u8; 16];
        store.enqueue(&message_id, &recipient, b"bytes", 1_000, 2_000);

        store.expire_overdue(3_000); // past expires_at
        assert_eq!(store.state_of(&message_id), Some(OutboundState::Expired));
        assert!(store.due_for_attempt(3_000).is_empty());
    }

    #[test]
    fn store_survives_being_reopened_at_the_same_path() {
        let dir = unique_test_dir("reopen");
        let path = dir.join("delivery.sqlite");
        let recipient = Identity::generate().node_id();
        let message_id = [6u8; 16];
        let now = now_millis();

        {
            let (store, was_reset) = DeliveryStore::open(&path);
            assert!(!was_reset);
            store.enqueue(&message_id, &recipient, b"envelope-bytes", now, now + 60_000);
            store.record_attempt(&message_id, now);
        }

        let (reopened, was_reset) = DeliveryStore::open(&path);
        assert!(!was_reset);
        assert_eq!(reopened.state_of(&message_id), Some(OutboundState::Sent));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_store_file_fails_safe_instead_of_panicking() {
        let dir = unique_test_dir("corrupt");
        let path = dir.join("delivery.sqlite");
        std::fs::write(&path, b"not a sqlite database, just garbage").unwrap();

        let (store, was_reset) = DeliveryStore::open(&path);
        assert!(was_reset);
        let recipient = Identity::generate().node_id();
        let now = now_millis();
        store.enqueue(&[7u8; 16], &recipient, b"bytes", now, now + 60_000);
        assert_eq!(store.state_of(&[7u8; 16]), Some(OutboundState::Queued));

        let quarantined_exists = std::fs::read_dir(&dir)
            .unwrap()
            .any(|entry| entry.unwrap().file_name().to_string_lossy().contains("delivery.sqlite.corrupt-"));
        assert!(quarantined_exists);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_size_is_bounded_by_capacity() {
        let store = DeliveryStore::in_memory_with_capacity(5);
        let now = now_millis();
        for i in 0..20u8 {
            let recipient = Identity::generate().node_id();
            store.enqueue(&[i; 16], &recipient, b"bytes", now, now + 999_999_999);
        }
        assert!(store.len() <= 5, "store should never grow past its configured capacity, got {}", store.len());
    }

    #[test]
    fn retry_delay_increases_with_attempts_but_is_capped() {
        let first = next_retry_delay_ms(1);
        let later = next_retry_delay_ms(5);
        let capped = next_retry_delay_ms(50);
        assert!(first < later, "backoff should grow with more attempts");
        assert!(capped <= RETRY_MAX_DELAY_MS + RETRY_MAX_DELAY_MS / 4, "backoff must stay bounded however many attempts have happened");
    }
}
