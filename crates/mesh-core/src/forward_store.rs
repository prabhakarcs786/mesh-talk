//! Milestone 3B: durable per-neighbor forwarding state for relayed packets.
//!
//! # The problem this fixes
//! Before this module existed, a relay's `flood()` call considered forwarding a message
//! successful the moment *any* neighbor accepted it, and durably marked the whole
//! `(sender, message_id)` pair as "handled" in `ReplayStore` on that basis alone. For a
//! relay `R` flooding to three neighbors:
//!
//! ```text
//!           B
//!         /
//! Alice → R → C
//!         \
//!           D
//! ```
//!
//! if the send to `B` succeeded but the sends to `C` and `D` failed (a temporarily
//! unreachable peer, a transient link error, etc.), the old logic still recorded the
//! message as durably forwarded. A later resend of the exact same packet -- the only
//! mechanism that could have gotten it to `C` or `D` -- would then be dropped as an
//! already-handled duplicate before ever reaching them, even though they never actually
//! got it. "Successfully sent to one neighbor" is not the same thing as "this message no
//! longer needs forwarding."
//!
//! # What this module tracks instead
//! One row per `(message_id, peer)` pair a relay has ever attempted (or still needs) to
//! forward to -- independent, per-neighbor state, instead of one collapsed boolean for
//! the whole message. `MeshNode::handle_incoming` marks the message durably `mark_seen`
//! in `ReplayStore` only once every tracked neighbor has actually received it (or
//! permanently given up after `expires_at`) -- see `all_peers_resolved`. Neighbors that
//! failed stay `Pending` and are retried with backoff by `retry_pending_forwards`,
//! independent of whether any *other* neighbor already succeeded, and this state
//! survives a relay restart the same way `DeliveryStore`'s outbound state does.
//!
//! This is deliberately scoped to *known-at-receipt-time* neighbors only: a peer that
//! joins the mesh *after* a message has already been fully forwarded to everyone known
//! at the time never retroactively receives it. Making late-joining peers eventually
//! receive already-circulated messages is real future work (store-and-forward/DTN), not
//! something this module attempts.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rand::RngCore;
use rusqlite::Connection;

use crate::identity::NodeId;

pub const DEFAULT_FORWARD_STORE_CAPACITY: usize = 20_000;

/// Same backoff shape as `delivery_store.rs`'s outbound retry -- see its doc for the
/// thundering-herd rationale. Kept as an independent constant (not shared) since relay
/// forwarding and endpoint delivery are different concerns that happen to want similar
/// tuning today, not values that must always move together.
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_DELAY_MS: u64 = 60_000;

/// Where one `(message_id, peer)` forwarding obligation currently stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardState {
    /// Not yet successfully forwarded to this peer -- still due for a (re)attempt.
    Pending,
    /// This peer successfully received this message -- terminal, nothing more to do.
    Forwarded,
    /// `expires_at` passed before this peer ever successfully received it -- terminal;
    /// counts as "resolved" for `all_peers_resolved` purposes (the relay gives up, it
    /// does not retry forever), but is distinct from `Forwarded` for diagnostics/tests.
    Expired,
}

impl ForwardState {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ForwardState::Pending),
            "forwarded" => Some(ForwardState::Forwarded),
            "expired" => Some(ForwardState::Expired),
            _ => None,
        }
    }
}

/// One pending forward attempt due right now -- returned by `due_for_attempt`.
pub struct DueForward {
    pub message_id: [u8; 16],
    pub peer: String,
    pub envelope_bytes: Vec<u8>,
    pub attempts: u32,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS forward_attempts (
        message_id BLOB NOT NULL,
        sender BLOB NOT NULL,
        peer TEXT NOT NULL,
        envelope_bytes BLOB NOT NULL,
        state TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        next_attempt_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (message_id, peer)
    );
    CREATE INDEX IF NOT EXISTS forward_attempts_next_attempt ON forward_attempts (state, next_attempt_at);
    CREATE INDEX IF NOT EXISTS forward_attempts_message_id ON forward_attempts (message_id);
";

pub struct ForwardStore {
    conn: Mutex<Connection>,
    capacity: usize,
}

impl ForwardStore {
    pub fn in_memory() -> Self {
        Self::in_memory_with_capacity(DEFAULT_FORWARD_STORE_CAPACITY)
    }

    pub fn in_memory_with_capacity(capacity: usize) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite connection must always succeed");
        Self::from_connection(conn, capacity)
    }

    /// Persistent, file-backed store at `path` -- same corruption-handling contract as
    /// `DeliveryStore::open`/`ReplayStore::open`: a missing file is created, a corrupt
    /// one is quarantined (renamed aside, never silently deleted) and replaced with a
    /// fresh store, reported back via the returned `was_reset` flag.
    pub fn open(path: impl Into<PathBuf>) -> (Self, bool) {
        Self::open_with_capacity(path, DEFAULT_FORWARD_STORE_CAPACITY)
    }

    pub fn open_with_capacity(path: impl Into<PathBuf>, capacity: usize) -> (Self, bool) {
        let path = path.into();
        match Self::try_open_and_verify(&path, capacity) {
            Ok(store) => (store, false),
            Err(err) => {
                log::warn!(
                    "mesh-core: FORWARD STATE RESET -- forward store at {path:?} could not be opened/verified ({err}); quarantining it and starting empty. Any not-yet-fully-forwarded relayed messages recorded before this reset will not be retried automatically to peers that hadn't received them yet."
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
            path.file_name().and_then(|n| n.to_str()).unwrap_or("forward-store.sqlite"),
            now_ms
        ));
        let _ = std::fs::rename(path, quarantined);
    }

    fn from_connection(conn: Connection, capacity: usize) -> Self {
        conn.execute_batch(SCHEMA).expect("schema creation must succeed on a fresh connection");
        Self { conn: Mutex::new(conn), capacity }
    }

    /// Records that `message_id` (originally from `sender`) needs to be forwarded to
    /// every peer in `peers`, starting `Pending` and due for an attempt immediately. A
    /// no-op (via `INSERT OR IGNORE`) for any peer already tracked for this
    /// `message_id` -- safe to call again for a resend of the same packet, or a
    /// redundant call within the same `handle_incoming` invocation. Must be called
    /// *before* the first send attempt to each peer, so a crash mid-flood still leaves
    /// untried peers retryable.
    pub fn enqueue_pending(&self, message_id: &[u8; 16], sender: &NodeId, peers: &[String], envelope_bytes: &[u8], now_ms: u64, expires_at_ms: u64) {
        let conn = self.conn.lock().unwrap();
        for peer in peers {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO forward_attempts (message_id, sender, peer, envelope_bytes, state, attempts, next_attempt_at, expires_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6, ?5)",
                rusqlite::params![&message_id[..], &sender[..], peer, envelope_bytes, now_ms as i64, expires_at_ms as i64],
            );
        }
        Self::enforce_capacity_locked(&conn, self.capacity);
    }

    /// Every `Pending` `(message_id, peer)` row whose `next_attempt_at` has passed and
    /// whose `expires_at` has not -- due for a (re)attempt right now.
    pub fn due_for_attempt(&self, now_ms: u64) -> Vec<DueForward> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT message_id, peer, envelope_bytes, attempts FROM forward_attempts
                 WHERE state = 'pending' AND next_attempt_at <= ?1 AND expires_at > ?1",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![now_ms as i64], |row| {
                let message_id: Vec<u8> = row.get(0)?;
                let peer: String = row.get(1)?;
                let envelope_bytes: Vec<u8> = row.get(2)?;
                let attempts: i64 = row.get(3)?;
                Ok((message_id, peer, envelope_bytes, attempts))
            })
            .unwrap();

        rows.filter_map(|r| r.ok())
            .filter_map(|(message_id, peer, envelope_bytes, attempts)| {
                let message_id: [u8; 16] = message_id.try_into().ok()?;
                Some(DueForward { message_id, peer, envelope_bytes, attempts: attempts as u32 })
            })
            .collect()
    }

    /// Records the outcome of a just-made forward attempt to `peer` for `message_id`:
    /// `Ok` transitions it to the terminal `Forwarded` state; `Err` increments the
    /// attempt counter and schedules a later retry with exponential backoff and jitter.
    /// A no-op if the row is already terminal (`Forwarded`/`Expired`).
    pub fn record_attempt_result(&self, message_id: &[u8; 16], peer: &str, succeeded: bool, now_ms: u64) {
        let conn = self.conn.lock().unwrap();
        if succeeded {
            let _ = conn.execute(
                "UPDATE forward_attempts SET state = 'forwarded' WHERE message_id = ?1 AND peer = ?2 AND state = 'pending'",
                rusqlite::params![&message_id[..], peer],
            );
            return;
        }
        let attempts: Option<i64> = conn
            .query_row(
                "SELECT attempts FROM forward_attempts WHERE message_id = ?1 AND peer = ?2 AND state = 'pending'",
                rusqlite::params![&message_id[..], peer],
                |row| row.get(0),
            )
            .ok();
        let Some(attempts) = attempts else { return };
        let new_attempts = attempts as u32 + 1;
        let next_attempt_at = now_ms + next_retry_delay_ms(new_attempts);
        let _ = conn.execute(
            "UPDATE forward_attempts SET attempts = ?3, next_attempt_at = ?4 WHERE message_id = ?1 AND peer = ?2",
            rusqlite::params![&message_id[..], peer, new_attempts as i64, next_attempt_at as i64],
        );
    }

    /// Transitions any `Pending` row whose `expires_at` has passed into the terminal
    /// `Expired` state, so it stops being returned by `due_for_attempt` and stops
    /// blocking `all_peers_resolved`.
    pub fn expire_overdue(&self, now_ms: u64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE forward_attempts SET state = 'expired' WHERE state = 'pending' AND expires_at <= ?1",
            rusqlite::params![now_ms as i64],
        );
    }

    /// True once every peer this relay ever recorded an obligation for regarding
    /// `message_id` is in a terminal state (`Forwarded` or `Expired`) -- i.e. nothing
    /// is still `Pending`. This is the gate `handle_incoming` uses to decide whether
    /// it's finally safe to durably mark the message `seen` in `ReplayStore`: while any
    /// peer is still pending, the message must remain retryable. Vacuously `true` if no
    /// peer was ever tracked for this `message_id` at all (nothing to forward to, e.g.
    /// no known peers at receipt time), matching the pre-Milestone-3B "no peers to
    /// forward to" behavior.
    pub fn all_peers_resolved(&self, message_id: &[u8; 16]) -> bool {
        let conn = self.conn.lock().unwrap();
        let pending_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM forward_attempts WHERE message_id = ?1 AND state = 'pending'",
                rusqlite::params![&message_id[..]],
                |row| row.get(0),
            )
            .unwrap_or(0);
        pending_count == 0
    }

    /// The `(sender, expires_at)` originally recorded for `message_id` (identical across
    /// every peer row for the same message), or `None` if this `message_id` was never
    /// tracked at all. Used by `MeshNode::retry_pending_forwards` to durably mark a
    /// message `seen` in `ReplayStore` once `all_peers_resolved` finally becomes true as
    /// a result of a later retry (as opposed to becoming true immediately within the
    /// same `handle_incoming` call, which already has the envelope's own sender/expiry
    /// on hand).
    pub fn sender_and_expiry_of(&self, message_id: &[u8; 16]) -> Option<(NodeId, u64)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT sender, expires_at FROM forward_attempts WHERE message_id = ?1 LIMIT 1",
            rusqlite::params![&message_id[..]],
            |row| {
                let sender: Vec<u8> = row.get(0)?;
                let expires_at: i64 = row.get(1)?;
                Ok((sender, expires_at))
            },
        )
        .ok()
        .and_then(|(sender, expires_at)| {
            let sender: NodeId = sender.try_into().ok()?;
            Some((sender, expires_at as u64))
        })
    }

    /// Current state of one `(message_id, peer)` pair, or `None` if it was never
    /// tracked (or has since been evicted by the capacity backstop). Test/diagnostic
    /// use.
    pub fn state_of(&self, message_id: &[u8; 16], peer: &str) -> Option<ForwardState> {
        let conn = self.conn.lock().unwrap();
        let state: Option<String> = conn
            .query_row(
                "SELECT state FROM forward_attempts WHERE message_id = ?1 AND peer = ?2",
                rusqlite::params![&message_id[..], peer],
                |row| row.get(0),
            )
            .ok();
        state.and_then(|s| ForwardState::from_str(&s))
    }

    fn enforce_capacity_locked(conn: &Connection, capacity: usize) {
        // Same backstop rationale as `DeliveryStore`'s: bounds storage growth from an
        // attacker (or a bug) generating far more forwarding obligations than could
        // ever legitimately be outstanding at once. Prefers evicting terminal
        // (forwarded/expired) rows first, since those carry no further obligation.
        let _ = conn.execute(
            "DELETE FROM forward_attempts WHERE rowid IN (
                SELECT rowid FROM forward_attempts
                ORDER BY (state IN ('forwarded', 'expired')) DESC, created_at ASC
                LIMIT MAX(0, (SELECT COUNT(*) FROM forward_attempts) - ?1)
            )",
            rusqlite::params![capacity as i64],
        );
    }

    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM forward_attempts", [], |row| row.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Exponential backoff with jitter -- see `delivery_store.rs`'s copy of this same shape
/// for the full thundering-herd rationale.
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

    fn test_sender() -> NodeId {
        Identity::generate().node_id()
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-forward-store-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_tracked_peers_is_vacuously_resolved() {
        let store = ForwardStore::in_memory();
        assert!(store.all_peers_resolved(&[1u8; 16]));
    }

    #[test]
    fn message_id_with_no_peers_tracked_is_vacuously_resolved_matching_pre_3b_behavior() {
        let store = ForwardStore::in_memory();
        // Enqueuing with an empty peer list must behave like "no peers to forward to".
        store.enqueue_pending(&[7u8; 16], &test_sender(), &[], b"bytes", now_millis(), now_millis() + 60_000);
        assert!(store.all_peers_resolved(&[7u8; 16]));
        assert!(store.is_empty());
    }

    #[test]
    fn partial_success_leaves_only_the_failed_peers_pending_and_unresolved() {
        let store = ForwardStore::in_memory();
        let message_id = [2u8; 16];
        let now = now_millis();
        let peers = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        store.enqueue_pending(&message_id, &test_sender(), &peers, b"bytes", now, now + 600_000);

        // B succeeds, C and D fail.
        store.record_attempt_result(&message_id, "b", true, now);
        store.record_attempt_result(&message_id, "c", false, now);
        store.record_attempt_result(&message_id, "d", false, now);

        assert_eq!(store.state_of(&message_id, "b"), Some(ForwardState::Forwarded));
        assert_eq!(store.state_of(&message_id, "c"), Some(ForwardState::Pending));
        assert_eq!(store.state_of(&message_id, "d"), Some(ForwardState::Pending));
        assert!(
            !store.all_peers_resolved(&message_id),
            "a partially-successful flood must not be considered fully forwarded while any peer is still pending"
        );

        // Only C and D are due for a retry, not B (already forwarded). Checked well
        // within the message's own (long) expiry window, not past it.
        let due: Vec<String> = store.due_for_attempt(now + 10_000).into_iter().map(|d| d.peer).collect();
        assert!(due.contains(&"c".to_string()));
        assert!(due.contains(&"d".to_string()));
        assert!(!due.contains(&"b".to_string()));
    }

    #[test]
    fn resolved_once_every_peer_eventually_succeeds() {
        let store = ForwardStore::in_memory();
        let message_id = [3u8; 16];
        let now = now_millis();
        let peers = vec!["b".to_string(), "c".to_string()];
        store.enqueue_pending(&message_id, &test_sender(), &peers, b"bytes", now, now + 60_000);

        store.record_attempt_result(&message_id, "b", true, now);
        assert!(!store.all_peers_resolved(&message_id));
        store.record_attempt_result(&message_id, "c", true, now);
        assert!(store.all_peers_resolved(&message_id));
    }

    #[test]
    fn expired_pending_peer_counts_as_resolved_but_not_forwarded() {
        let store = ForwardStore::in_memory();
        let message_id = [4u8; 16];
        let now = now_millis();
        store.enqueue_pending(&message_id, &test_sender(), &["c".to_string()], b"bytes", now, now + 100);

        store.expire_overdue(now + 200);
        assert_eq!(store.state_of(&message_id, "c"), Some(ForwardState::Expired));
        assert!(store.all_peers_resolved(&message_id));
        assert!(store.due_for_attempt(now + 999_999).is_empty());
    }

    #[test]
    fn resending_the_same_message_id_does_not_reset_an_already_forwarded_peer() {
        let store = ForwardStore::in_memory();
        let message_id = [5u8; 16];
        let now = now_millis();
        let sender = test_sender();
        store.enqueue_pending(&message_id, &sender, &["b".to_string()], b"bytes", now, now + 60_000);
        store.record_attempt_result(&message_id, "b", true, now);

        // Same packet resent -- re-enqueuing must be a harmless no-op (INSERT OR IGNORE),
        // not reset an already-forwarded peer back to pending.
        store.enqueue_pending(&message_id, &sender, &["b".to_string()], b"bytes", now + 1_000, now + 60_000);
        assert_eq!(store.state_of(&message_id, "b"), Some(ForwardState::Forwarded));
    }

    #[test]
    fn sender_and_expiry_are_recoverable_after_resolution_for_later_replay_marking() {
        let store = ForwardStore::in_memory();
        let message_id = [8u8; 16];
        let sender = test_sender();
        let now = now_millis();
        assert!(store.sender_and_expiry_of(&message_id).is_none());

        store.enqueue_pending(&message_id, &sender, &["b".to_string()], b"bytes", now, now + 60_000);
        let (recovered_sender, recovered_expiry) = store.sender_and_expiry_of(&message_id).unwrap();
        assert_eq!(recovered_sender, sender);
        assert_eq!(recovered_expiry, now + 60_000);
    }

    #[test]
    fn persists_across_reopen_at_the_same_path() {
        let dir = unique_test_dir("reopen");
        let path = dir.join("forward.sqlite");
        let message_id = [6u8; 16];
        let now = now_millis();

        {
            let (store, was_reset) = ForwardStore::open(&path);
            assert!(!was_reset);
            store.enqueue_pending(&message_id, &test_sender(), &["b".to_string(), "c".to_string()], b"bytes", now, now + 600_000);
            store.record_attempt_result(&message_id, "b", true, now);
            store.record_attempt_result(&message_id, "c", false, now);
        }

        let (reopened, was_reset) = ForwardStore::open(&path);
        assert!(!was_reset);
        assert_eq!(reopened.state_of(&message_id, "b"), Some(ForwardState::Forwarded));
        assert_eq!(reopened.state_of(&message_id, "c"), Some(ForwardState::Pending));
        assert!(!reopened.all_peers_resolved(&message_id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_replaced_with_a_fresh_empty_store() {
        let dir = unique_test_dir("corrupt");
        let path = dir.join("forward.sqlite");
        std::fs::write(&path, b"not a sqlite file").unwrap();

        let (store, was_reset) = ForwardStore::open(&path);
        assert!(was_reset);
        assert!(store.is_empty());
        let quarantined_exists = std::fs::read_dir(&dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("forward.sqlite.corrupt-")
        });
        assert!(quarantined_exists, "the corrupt original file must be quarantined (renamed aside), not silently deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
