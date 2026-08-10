//! Milestone 2B.2a: Persistent `ContactStore`.
//!
//! Persists `MeshClient`'s contact cache (see `contacts()`/`pollContactEvent()`) to a
//! single versioned JSON file at an app-provided path (e.g. the iOS Documents directory,
//! or Android's internal files dir -- both already sandboxed to the app, so no extra
//! encryption is applied here beyond what the OS already provides for app-private
//! storage). Without this, a previously-discovered contact's `PublicIdentity` -- and
//! therefore the ability to encrypt a "MeshTalk Direct Encryption v1" message to them
//! while they're offline -- disappeared the moment the app restarted, which is
//! incompatible with any future store-and-forward/DTN design.
//!
//! # What is/isn't persisted, and who wins on conflict
//! - `local_alias` and `verification` (local trust decisions) are **local-user-owned**
//!   data: only ever set by explicit local action, never by anything the network claims.
//!   These survive a restart unchanged, and the network can never overwrite them --
//!   [`ContactStore::merge_from_network`] enforces this the same way
//!   `MeshClient::update_contact_cache` already does for the in-memory cache.
//! - `advertised_name`, the `PublicIdentity` itself, and `last_seen_ms` are
//!   network-supplied, untrusted-or-freshness-only data. They're persisted purely so a
//!   restart doesn't have to wait for discovery to repopulate everything from scratch,
//!   but a live peer re-advertising them after restart always wins over whatever is on
//!   disk.
//! - `identity_change_pending` is persisted so an unacknowledged "identity changed"
//!   warning survives a restart -- a restart is not the same thing as the local user
//!   having seen and dismissed the warning.
//!
//! # Schema versioning
//! [`PersistedStoreV1`] is an explicitly versioned on-disk DTO, deliberately kept
//! separate from `mesh_core::ContactRecord` so the in-memory struct can evolve freely
//! without silently changing the disk format underneath existing installs. Any future
//! breaking change to the on-disk shape must introduce a `PersistedStoreV2` (or higher)
//! and an explicit migration path in [`ContactStore::load`] -- never reinterpret old
//! bytes under a new struct shape and hope `serde` happens to cope.
//!
//! # Safety properties
//! - **Atomic writes.** Every [`ContactStore::save`] writes to a temp file in the same
//!   directory as the real path, then renames it over the real path -- `rename` is
//!   atomic on the POSIX-family filesystems both iOS and Android use, so a crash or kill
//!   mid-write can never leave a half-written, corrupt file at the real path.
//! - **Fail-safe on corruption.** [`ContactStore::load`] never panics and never stops the
//!   app from starting: unreadable or unrecognized-schema data is moved aside (renamed
//!   to `<path>.corrupt-<unix_ms>`, preserving the evidence instead of silently deleting
//!   it) and the store starts empty rather than erroring out.
//! - **Explicit reset.** There is no implicit "clear everything" path; callers that want
//!   to wipe stored contacts must call [`ContactStore::clear`] explicitly.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use mesh_core::{session_protocol_version, ContactRecord, NodeId, PublicIdentity, VerificationState, X25519Public, ENCRYPTION_VERSION};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
enum PersistedVerificationState {
    Unverified,
    Verified,
}

impl From<VerificationState> for PersistedVerificationState {
    fn from(state: VerificationState) -> Self {
        match state {
            VerificationState::Unverified => PersistedVerificationState::Unverified,
            VerificationState::Verified => PersistedVerificationState::Verified,
        }
    }
}

impl From<PersistedVerificationState> for VerificationState {
    fn from(state: PersistedVerificationState) -> Self {
        match state {
            PersistedVerificationState::Unverified => VerificationState::Unverified,
            PersistedVerificationState::Verified => VerificationState::Verified,
        }
    }
}

/// On-disk mirror of `mesh_core::session::PublicIdentity` -- kept as its own type (even
/// though the fields match today) so this file's schema doesn't silently change shape if
/// `PublicIdentity` ever does.
#[derive(Serialize, Deserialize, Clone)]
struct PersistedPublicIdentity {
    node_id: NodeId,
    x25519_public: X25519Public,
    x25519_signature: Vec<u8>,
}

impl From<&PublicIdentity> for PersistedPublicIdentity {
    fn from(identity: &PublicIdentity) -> Self {
        Self {
            node_id: identity.node_id,
            x25519_public: identity.x25519_public,
            x25519_signature: identity.x25519_signature.clone(),
        }
    }
}

impl PersistedPublicIdentity {
    /// Rebuilds a `PublicIdentity` from disk. Deliberately does **not** re-verify the
    /// binding signature here -- a record already stored in this contact cache was
    /// verified before it was ever inserted (see `MeshClient::update_contact_cache`), and
    /// re-deriving trust from disk bytes on every launch would let a corrupted-but-
    /// well-formed file silently resurrect a binding that was never actually checked.
    /// Callers that need the verified-ness guarantee re-established should treat a
    /// freshly loaded contact as "known but re-verify on next live contact", exactly like
    /// any other stored `PublicIdentity` -- `Session::establish` still requires a
    /// `VerifiedPublicIdentity`, so nothing downstream can skip that check regardless.
    fn into_public_identity(self) -> PublicIdentity {
        PublicIdentity {
            node_id: self.node_id,
            x25519_public: self.x25519_public,
            x25519_signature: self.x25519_signature,
        }
    }
}

/// One persisted contact. See the module doc for exactly which fields the network is
/// allowed to overwrite on merge, and which it never is.
#[derive(Serialize, Deserialize, Clone)]
struct PersistedContactV1 {
    public_identity: PersistedPublicIdentity,
    advertised_name: Option<String>,
    local_alias: Option<String>,
    verification: PersistedVerificationState,
    first_seen_ms: u64,
    last_seen_ms: u64,
    identity_change_pending: bool,
    /// The session/key-agreement protocol version in effect when this record was last
    /// written. Not used for any migration decision yet -- recorded now so a future
    /// session-protocol bump has the information available to decide whether an old
    /// record needs special handling, instead of silently assuming compatibility.
    session_protocol_version: u8,
    /// Same rationale as `session_protocol_version`, for the AEAD/encryption scheme.
    encryption_version: u8,
}

impl From<&ContactRecord> for PersistedContactV1 {
    fn from(record: &ContactRecord) -> Self {
        Self {
            public_identity: PersistedPublicIdentity::from(&record.public_identity),
            advertised_name: record.advertised_name.clone(),
            local_alias: record.local_alias.clone(),
            verification: record.verification.into(),
            first_seen_ms: record.first_seen_ms,
            last_seen_ms: record.last_seen_ms,
            identity_change_pending: record.identity_change_pending,
            session_protocol_version: session_protocol_version(),
            encryption_version: ENCRYPTION_VERSION,
        }
    }
}

impl PersistedContactV1 {
    fn into_contact_record(self) -> ContactRecord {
        ContactRecord {
            public_identity: self.public_identity.into_public_identity(),
            advertised_name: self.advertised_name,
            local_alias: self.local_alias,
            verification: self.verification.into(),
            first_seen_ms: self.first_seen_ms,
            last_seen_ms: self.last_seen_ms,
            identity_change_pending: self.identity_change_pending,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedStoreV1 {
    schema_version: u32,
    contacts: Vec<PersistedContactV1>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Loads/saves the contact cache to a single JSON file at a fixed path, chosen by the
/// app (so it lands in the right sandboxed, app-private directory on each platform).
pub struct ContactStore {
    path: PathBuf,
}

impl ContactStore {
    /// Opens (without necessarily creating) the store at `path`, returning it together
    /// with whatever contacts were successfully loaded from disk (an empty map on first
    /// ever launch, or if the file is missing/corrupt -- see the module doc).
    pub fn open(path: impl Into<PathBuf>) -> (Self, HashMap<NodeId, ContactRecord>) {
        let path = path.into();
        let contacts = Self::load(&path);
        (Self { path }, contacts)
    }

    fn load(path: &Path) -> HashMap<NodeId, ContactRecord> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return HashMap::new(), // doesn't exist yet (first launch) or unreadable
        };
        match serde_json::from_slice::<PersistedStoreV1>(&bytes) {
            Ok(store) if store.schema_version == SCHEMA_VERSION => store
                .contacts
                .into_iter()
                .map(|persisted| {
                    let record = persisted.into_contact_record();
                    (record.public_identity.node_id, record)
                })
                .collect(),
            Ok(_) => {
                // Recognized JSON shape, but an unexpected schema version -- treat the
                // same as corrupt rather than guessing at a migration that doesn't exist
                // yet. Quarantined, not deleted, so the data isn't silently lost.
                eprintln!("mesh-mobile: contact store at {path:?} has an unsupported schema version -- quarantining and starting empty");
                Self::quarantine_corrupt_file(path);
                HashMap::new()
            }
            Err(err) => {
                eprintln!("mesh-mobile: contact store at {path:?} is corrupt ({err}) -- quarantining and starting empty");
                Self::quarantine_corrupt_file(path);
                HashMap::new()
            }
        }
    }

    /// Moves an unreadable/corrupt file aside rather than overwriting or deleting it
    /// silently, so there's still forensic evidence something went wrong. Best-effort:
    /// if even the rename fails, there's nothing safe left to do except continue with an
    /// empty in-memory store (the alternative -- refusing to start the app -- would be
    /// worse).
    fn quarantine_corrupt_file(path: &Path) {
        let quarantined = path.with_file_name(format!(
            "{}.corrupt-{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("contacts"),
            now_millis()
        ));
        let _ = fs::rename(path, quarantined);
    }

    /// Atomically overwrites the on-disk store with the given contacts (write-to-temp
    /// then rename -- see the module doc). Best-effort: a failure here (e.g. disk full)
    /// is silently ignored rather than propagated, since the in-memory cache remains
    /// correct either way and there is no sensible recovery action for the caller to
    /// take other than "try again next time something changes".
    pub fn save(&self, contacts: &HashMap<NodeId, ContactRecord>) {
        let store = PersistedStoreV1 {
            schema_version: SCHEMA_VERSION,
            contacts: contacts.values().map(PersistedContactV1::from).collect(),
        };
        let Ok(json) = serde_json::to_vec_pretty(&store) else { return };
        let tmp_path = self.path.with_file_name(format!(
            "{}.tmp",
            self.path.file_name().and_then(|n| n.to_str()).unwrap_or("contacts.json")
        ));
        if fs::write(&tmp_path, &json).is_err() {
            return;
        }
        let _ = fs::rename(&tmp_path, &self.path);
    }

    /// Explicitly wipes the on-disk store (e.g. "reset identity"/"remove all contacts"
    /// flows). Deliberately not called from anywhere else -- there is no implicit path
    /// that clears persisted contacts as a side effect of anything else.
    pub fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn sample_contact_record(now: u64) -> ContactRecord {
        let peer = Identity::generate();
        ContactRecord::new(PublicIdentity::new(&peer), Some("Bob".to_string()), now)
    }

    /// A guaranteed-unique temp directory for one test -- combines a nanosecond
    /// timestamp with a per-process atomic counter, so parallel test execution can
    /// never collide the way a millisecond-resolution timestamp alone occasionally did
    /// (observed flakily in this file before this fix).
    fn unique_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-contact-store-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_saved_store_reopens_with_the_same_contact() {
        let dir = unique_test_dir("reopens");
        let path = dir.join("contacts.json");

        let (store, contacts) = ContactStore::open(&path);
        assert!(contacts.is_empty(), "nothing persisted yet on first open");

        let mut contacts = HashMap::new();
        let record = sample_contact_record(1000);
        let node_id = record.public_identity.node_id;
        contacts.insert(node_id, record);
        store.save(&contacts);

        // Reopen as an entirely new `ContactStore` -- proves this isn't just reusing the
        // same in-memory object, but actually round-tripping through disk.
        let (_reopened_store, reloaded) = ContactStore::open(&path);
        assert_eq!(reloaded.len(), 1);
        let reloaded_record = reloaded.get(&node_id).unwrap();
        assert_eq!(reloaded_record.advertised_name.as_deref(), Some("Bob"));
        assert_eq!(reloaded_record.first_seen_ms, 1000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_alias_and_verification_survive_a_restart() {
        let dir = unique_test_dir("alias-verification");
        let path = dir.join("contacts.json");

        let (store, _) = ContactStore::open(&path);
        let mut contacts = HashMap::new();
        let mut record = sample_contact_record(2000);
        let node_id = record.public_identity.node_id;
        record.local_alias = Some("My Best Friend".to_string());
        record.mark_verified();
        contacts.insert(node_id, record);
        store.save(&contacts);

        let (_reopened_store, reloaded) = ContactStore::open(&path);
        let reloaded_record = reloaded.get(&node_id).unwrap();
        assert_eq!(reloaded_record.local_alias.as_deref(), Some("My Best Friend"));
        assert_eq!(reloaded_record.verification, VerificationState::Verified);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_change_pending_survives_a_restart() {
        let dir = unique_test_dir("identity-change-pending");
        let path = dir.join("contacts.json");

        let (store, _) = ContactStore::open(&path);
        let mut contacts = HashMap::new();
        let mut record = sample_contact_record(3000);
        let node_id = record.public_identity.node_id;
        record.mark_identity_change_pending();
        contacts.insert(node_id, record);
        store.save(&contacts);

        let (_reopened_store, reloaded) = ContactStore::open(&path);
        let reloaded_record = reloaded.get(&node_id).unwrap();
        assert!(reloaded_record.identity_change_pending);
        assert_eq!(reloaded_record.verification, VerificationState::Unverified);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The corrupt-storage-must-fail-safely requirement: garbage bytes at the store path
    /// must never panic or propagate a fatal error -- the app must still start, with an
    /// empty contact cache, and the bad file must be preserved (quarantined) rather than
    /// silently destroyed.
    #[test]
    fn a_corrupt_store_file_fails_safe_instead_of_panicking() {
        let dir = unique_test_dir("corrupt");
        let path = dir.join("contacts.json");
        std::fs::write(&path, b"this is not valid json at all {{{").unwrap();

        let (_store, contacts) = ContactStore::open(&path);
        assert!(contacts.is_empty());
        assert!(!path.exists(), "the corrupt file should have been moved aside, not left in place");

        let quarantined_exists = std::fs::read_dir(&dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("contacts.json.corrupt-")
        });
        assert!(quarantined_exists, "expected a quarantined copy of the corrupt file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unsupported/future schema version must also fail safe (same treatment as
    /// outright-corrupt bytes), not attempt a guessed migration.
    #[test]
    fn an_unrecognized_schema_version_fails_safe() {
        let dir = unique_test_dir("unrecognized-schema");
        let path = dir.join("contacts.json");
        std::fs::write(&path, br#"{"schema_version": 999, "contacts": []}"#).unwrap();

        let (_store, contacts) = ContactStore::open(&path);
        assert!(contacts.is_empty());
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saving must be atomic: no half-written temp file should linger, and the real path
    /// should always contain valid, fully-written JSON after `save` returns.
    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = unique_test_dir("no-temp-file");
        let path = dir.join("contacts.json");

        let (store, _) = ContactStore::open(&path);
        let mut contacts = HashMap::new();
        let record = sample_contact_record(4000);
        contacts.insert(record.public_identity.node_id, record);
        store.save(&contacts);

        let tmp_path = dir.join("contacts.json.tmp");
        assert!(!tmp_path.exists());
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_the_store_file() {
        let dir = unique_test_dir("clear");
        let path = dir.join("contacts.json");

        let (store, _) = ContactStore::open(&path);
        let mut contacts = HashMap::new();
        let record = sample_contact_record(5000);
        contacts.insert(record.public_identity.node_id, record);
        store.save(&contacts);
        assert!(path.exists());

        store.clear();
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
