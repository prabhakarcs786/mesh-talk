//! UniFFI bindings exposing `mesh-core` to Swift (iOS) and Kotlin (Android).
//!
//! Uses the UDP transport today (works over any local Wi-Fi/hotspot, no internet
//! required, just like the `mesh-cli` demo) since it's the only `Transport` that's fully
//! two-way working right now. Swapping in Bluetooth LE once its peripheral/advertising
//! side lands (see the repo issue tracker) means changing the `UdpTransport` type used
//! below to `BleCentralTransport` -- the rest of this file, and everything on the
//! Swift/Kotlin side, stays the same, because `mesh-core`'s routing/crypto logic never
//! depends on which `Transport` is plugged in.

use std::collections::VecDeque;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mesh_core::{short_id, ChannelKey, ContactRecord, Identity, MeshNode, VerificationState};
use mesh_transport_udp::{LanDiscovery, UdpTransport};
use rand::RngCore;

mod contact_store;
use contact_store::ContactStore;

/// Minimal diagnostic logger -- writes to stderr, which shows up in the Xcode console
/// for iOS out of the box. Android visibility is more limited (native `stderr` isn't
/// automatically routed to `logcat` the way JVM `System.out`/`System.err` are) --
/// wiring up a proper `android_logger`-style bridge is a reasonable future follow-up,
/// not done here to keep this addition minimal. Never logs plaintext, session keys, or
/// identity seeds -- see the `log::debug!`/`log::warn!` call sites in `mesh-core`/
/// `mesh-transport-udp`/this file for exactly what is and isn't logged.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER_INIT: std::sync::Once = std::sync::Once::new();

/// Installs `StderrLogger` as the global logger, exactly once -- safe to call from every
/// `MeshClient::new` (e.g. across multiple app-level restarts, or multiple clients in a
/// test), since `log::set_boxed_logger` only accepts being called once per process and
/// would otherwise return an error on the second call.
fn init_logging() {
    LOGGER_INIT.call_once(|| {
        let _ = log::set_boxed_logger(Box::new(StderrLogger));
        log::set_max_level(log::LevelFilter::Debug);
    });
}

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MeshError {
    #[error("network error: {0}")]
    Network(String),
}

/// What kind of attachment a message carries -- mirrors `mesh_core::ContentKind`, kept as
/// a separate type here since UniFFI's derive macros need to be applied where a type is
/// defined, not on a type imported from another crate.
#[derive(uniffi::Enum)]
pub enum AttachmentKind {
    Image,
    Video,
    Voice,
    File,
}

impl From<AttachmentKind> for mesh_core::ContentKind {
    fn from(kind: AttachmentKind) -> Self {
        match kind {
            AttachmentKind::Image => mesh_core::ContentKind::Image,
            AttachmentKind::Video => mesh_core::ContentKind::Video,
            AttachmentKind::Voice => mesh_core::ContentKind::Voice,
            AttachmentKind::File => mesh_core::ContentKind::File,
        }
    }
}

impl From<mesh_core::ContentKind> for AttachmentKind {
    fn from(kind: mesh_core::ContentKind) -> Self {
        match kind {
            mesh_core::ContentKind::Image => AttachmentKind::Image,
            mesh_core::ContentKind::Video => AttachmentKind::Video,
            mesh_core::ContentKind::Voice => AttachmentKind::Voice,
            // Plain text never reaches here (see ReceivedContent::Text handling below);
            // anything else generic falls back to File.
            _ => AttachmentKind::File,
        }
    }
}

/// A file attachment (image, video, voice note, or generic file) carried by a message.
#[derive(uniffi::Record)]
pub struct FileAttachment {
    /// Matches the `transferId` on the `TransferProgressUpdate`s seen while this
    /// attachment was arriving, so the UI can remove the right progress bar once this
    /// shows up in `pollMessage()`.
    pub transfer_id: String,
    pub kind: AttachmentKind,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// A single message received from the mesh, ready to show in a UI. Exactly one of
/// `text`/`attachment` is populated. Milestone 3C: by the time this is returned from
/// `pollMessage()`, `mesh-core` has already durably persisted it (see
/// `MeshClient::chatHistory`) and, for a `DirectV1` message, already acknowledged it to
/// the sender -- there is nothing left for the app to do to make this durable.
#[derive(uniffi::Record)]
pub struct ReceivedMessage {
    /// Full hex-encoded id of the conversation partner this message belongs to (the
    /// sender, since every message the mobile apps send/receive now is a direct message
    /// to/from one specific peer) -- lets the UI group messages into per-contact chat
    /// threads instead of one shared feed.
    pub peer_id: String,
    /// Short hex prefix of the sender's public key -- stable per-device identity.
    pub sender_id: String,
    /// Hex-encoded id of the underlying envelope this message arrived in (for a
    /// multi-chunk attachment, the *last* chunk's id) -- stable across `chatHistory()`
    /// and `pollMessage()`, useful as a UI list key.
    pub message_id: String,
    pub text: Option<String>,
    pub attachment: Option<FileAttachment>,
}

/// Whether a `TransferProgressUpdate` describes a file this device is sending out, or one
/// it's in the middle of receiving.
#[derive(uniffi::Enum)]
pub enum TransferDirection {
    Sending,
    Receiving,
}

/// A live progress snapshot for one in-flight attachment transfer -- poll this
/// periodically (like `pollMessage`) to drive a progress bar instead of the attachment
/// only appearing once it's 100% there. `doneChunks == totalChunks` marks completion (for
/// a received attachment, the fully reassembled `ReceivedMessage` also arrives via
/// `pollMessage` around the same time; for a sent one, this is the only signal you get).
#[derive(uniffi::Record)]
pub struct TransferProgressUpdate {
    /// Stable per-transfer id (hex-encoded), so a UI can track multiple in-flight
    /// transfers -- e.g. sending one photo while still receiving another -- separately.
    pub transfer_id: String,
    pub kind: AttachmentKind,
    pub direction: TransferDirection,
    pub done_chunks: u32,
    pub total_chunks: u32,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Bound on `MeshClient::contact_events`, same rationale as the other bounded queues in
/// this file -- a UI that falls behind on polling shouldn't grow this unboundedly.
const CONTACT_EVENT_CAPACITY: usize = 200;

/// The outcome of `MeshClient::send`. Milestone 3C: deliberately never silently
/// downgrades a long message to a weaker, best-effort delivery guarantee -- `send`
/// either gives ordinary chat text durable ACK/retry delivery, or reports exactly why
/// it couldn't, so the app can tell the user rather than unknowingly getting a weaker
/// guarantee for a message that happened to be a little longer.
#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Durably queued for reliable, ACK/retry delivery (see `send_reliable_text`'s doc
    /// -- this means "queued and a first attempt was made," not "the recipient has it
    /// yet").
    Sent,
    /// Refused -- and nothing was sent -- because `text` doesn't fit in the single-chunk
    /// reliable-delivery path. There is deliberately no automatic fallback to a weaker,
    /// best-effort send: reliable chunked-transfer support for longer text is future
    /// work, not something to silently substitute today.
    TooLongForReliableText,
    /// `remote_node_id` wasn't valid hex, this contact's identity isn't known yet, or
    /// the encryption/send attempt itself failed.
    Failed,
}

/// Parses a hex string back into a fixed-size byte array (the inverse of `hex_encode`).
/// Returns `None` if the string is the wrong length or contains non-hex characters --
/// e.g. a UI passing back a garbled/edited node id.
fn hex_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Parses the (remote node id, call id) pair used by every call-signaling/frame method.
fn parse_target_and_call(remote_node_id: &str, call_id: &str) -> Option<(mesh_core::NodeId, [u8; 16])> {
    Some((hex_decode::<32>(remote_node_id)?, hex_decode::<16>(call_id)?))
}

/// Reconstructs the identity to use for a new `MeshClient`: reuses a previously-persisted
/// seed if it's present and the right length, otherwise generates a brand-new one.
/// Corrupted/wrong-length storage (e.g. a truncated or empty Keychain/Keystore entry, or
/// simply nothing saved yet on first launch) fails safely by falling back to a fresh
/// identity rather than panicking or refusing to start -- a device that lost its identity
/// storage should still be usable, just as a new, unrecognized node on the mesh.
fn resolve_identity(seed: Option<Vec<u8>>) -> Identity {
    match seed.and_then(|s| <[u8; 32]>::try_from(s).ok()) {
        Some(seed) => Identity::from_seed(seed),
        None => Identity::generate(),
    }
}

/// Milestone 3C.1: reconstructs the at-rest storage key protecting `InboxStore`'s
/// content, the same way `resolve_identity` reconstructs the identity seed -- reuses a
/// previously-persisted key if present and the right length, otherwise generates a
/// fresh random one. Corrupted/wrong-length storage fails safe by generating a new key
/// rather than panicking or refusing to start; the cost is that any previously-stored
/// chat history becomes unreadable (see `InboxStore`'s doc on a wrong key failing
/// closed) rather than the app being unusable.
fn resolve_storage_key(key: Option<Vec<u8>>) -> [u8; 32] {
    match key.and_then(|k| <[u8; 32]>::try_from(k).ok()) {
        Some(key) => key,
        None => {
            let mut random_key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut random_key);
            random_key
        }
    }
}

/// How many times to (re-)send a call-signaling message like accept/reject/end, and how
/// long to wait between attempts. These are single UDP packets with no acknowledgement,
/// so a lost one otherwise leaves the other side stuck (e.g. still "ringing" after the
/// caller already hung up, with nothing in this app to time that out on its own).
const SIGNAL_RETRY_COUNT: usize = 3;
const SIGNAL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Sends a call signal (accept/reject/end -- never an invite, see below) a few times a
/// short moment apart for reliability.
///
/// Deliberately not used for `Invite`: the receiving side treats *every* incoming invite
/// as a new ring (showing the incoming-call banner, or auto-declining if already on a
/// call), so repeating an invite could look like a second call attempt. Accept/Reject/End
/// don't have that problem -- the receiving side's handling of each is idempotent, so a
/// duplicate that arrives after the first one was already acted on is just a no-op.
async fn send_signal_with_retries(
    node: &MeshNode<UdpTransport>,
    target: mesh_core::NodeId,
    signal: mesh_core::call::CallSignal,
) {
    for attempt in 0..SIGNAL_RETRY_COUNT {
        let result = match signal {
            mesh_core::call::CallSignal::Accept { call_id } => node.call_accept(target, call_id).await,
            mesh_core::call::CallSignal::Reject { call_id } => node.call_reject(target, call_id).await,
            mesh_core::call::CallSignal::End { call_id } => node.call_end(target, call_id).await,
            mesh_core::call::CallSignal::Invite { call_id, video } => node.call_invite(target, call_id, video).await,
        };
        let _ = result;
        if attempt + 1 != SIGNAL_RETRY_COUNT {
            tokio::time::sleep(SIGNAL_RETRY_DELAY).await;
        }
    }
}

fn to_progress_update(
    progress: mesh_core::TransferProgress,
    direction: TransferDirection,
) -> TransferProgressUpdate {
    TransferProgressUpdate {
        transfer_id: hex_encode(&progress.transfer_id),
        kind: progress.kind.into(),
        direction,
        done_chunks: progress.done_chunks,
        total_chunks: progress.total_chunks,
    }
}

/// Shared conversion from `mesh-core`'s `ReceivedContent` to the UniFFI-facing
/// `ReceivedMessage` -- used both by the live receive loop and by `chatHistory()`
/// (Milestone 3C), so a message looks identical to the UI regardless of whether it just
/// arrived or was loaded back from the durable inbox on launch.
fn to_received_message(sender: mesh_core::NodeId, message_id: [u8; 16], content: mesh_core::ReceivedContent) -> ReceivedMessage {
    let peer_id = hex_encode(&sender);
    let sender_id = short_id(&sender);
    let message_id = hex_encode(&message_id);
    match content {
        mesh_core::ReceivedContent::Text(text) => ReceivedMessage { peer_id, sender_id, message_id, text: Some(text), attachment: None },
        mesh_core::ReceivedContent::File { transfer_id, name, mime, kind, data } => ReceivedMessage {
            peer_id,
            sender_id,
            message_id,
            text: None,
            attachment: Some(FileAttachment { transfer_id: hex_encode(&transfer_id), kind: kind.into(), name, mime, data }),
        },
    }
}

/// A nearby device found via LAN auto-discovery, ready to connect to with one tap
/// instead of typing its IP address -- similar to a Bluetooth/Wi-Fi device picker.
#[derive(uniffi::Record)]
pub struct DiscoveredPeer {
    /// Short display id (see `short_id`).
    pub node_id: String,
    /// Full hex-encoded node id -- pass this to `startCall`/`acceptCall`/etc to address
    /// call signaling and media frames to this specific peer.
    pub full_node_id: String,
    pub display_name: String,
    pub address: String,
    /// Short numeric code, identical on both devices, so the user can visually confirm
    /// they're pairing with the right device (Bluetooth-style numeric comparison).
    pub pairing_code: String,
}

/// How much this device currently trusts a contact's identity -- **local trust state
/// only**, mirrors `mesh_core::session::VerificationState`. A cryptographically valid
/// binding (checked before a contact is ever stored) proves the X25519 key belongs to
/// that `NodeId`; it does not by itself mean the human on the other end has been
/// verified (e.g. via a future QR/safety-number flow) -- see `ContactIdentity`.
#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum ContactVerification {
    Unverified,
    Verified,
}

impl From<VerificationState> for ContactVerification {
    fn from(state: VerificationState) -> Self {
        match state {
            VerificationState::Unverified => ContactVerification::Unverified,
            VerificationState::Verified => ContactVerification::Verified,
        }
    }
}

/// A known contact: enough to send them a real "MeshTalk Direct Encryption v1" message
/// (`MeshClient::send`/`sendFile`) even when they're not currently visible via live
/// discovery -- e.g. seen yesterday, offline right now. Since Milestone 2B.2a this is
/// backed by a persistent on-disk `ContactStore` (see that module's doc) when the app
/// supplies a `contacts_db_path`, so a contact discovered yesterday remains sendable-to
/// after an app restart, not only within the same app session.
#[derive(uniffi::Record)]
pub struct ContactIdentity {
    pub node_id: String,
    pub full_node_id: String,
    /// Untrusted, cosmetic name this contact advertises about themselves -- never used
    /// for authentication/trust decisions. Distinct from a future local alias the user
    /// sets themselves.
    pub advertised_name: String,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub verification: ContactVerification,
    /// Whether this contact has an unacknowledged "identity changed" warning pending --
    /// persisted, so it survives an app restart instead of silently disappearing before
    /// the user has actually seen and dismissed it (call `acknowledgeIdentityChange` to
    /// clear it). See `mesh_core::ContactRecord::identity_change_pending`.
    pub identity_change_pending: bool,
}

/// What kind of news a `ContactEvent` carries.
#[derive(uniffi::Enum)]
pub enum ContactEventKind {
    /// Seen for the first time -- a new entry in the contact cache.
    Discovered,
    /// A **previously-known** `NodeId` just advertised *different* cryptographic
    /// material (a different X25519 public key) than what was stored for it. This is
    /// never silently accepted -- the UI must surface this prominently (e.g. "⚠ Security
    /// identity changed") rather than quietly trusting the new key, since this is
    /// exactly the situation a device impersonation attempt (or a factory
    /// reset/reinstall on the other end) would produce.
    IdentityChanged,
}

/// Poll this (like `pollMessage`) to learn about new contacts and identity changes.
#[derive(uniffi::Record)]
pub struct ContactEvent {
    pub full_node_id: String,
    pub kind: ContactEventKind,
}

/// Whether a `CallFrameUpdate` carries a chunk of audio or a video frame.
#[derive(uniffi::Enum)]
pub enum CallMediaKind {
    Audio,
    Video,
}

impl From<CallMediaKind> for mesh_core::MediaKind {
    fn from(kind: CallMediaKind) -> Self {
        match kind {
            CallMediaKind::Audio => mesh_core::MediaKind::Audio,
            CallMediaKind::Video => mesh_core::MediaKind::Video,
        }
    }
}

impl From<mesh_core::MediaKind> for CallMediaKind {
    fn from(kind: mesh_core::MediaKind) -> Self {
        match kind {
            mesh_core::MediaKind::Audio => CallMediaKind::Audio,
            mesh_core::MediaKind::Video => CallMediaKind::Video,
        }
    }
}

/// What kind of call-signaling news a `CallEvent` carries.
#[derive(uniffi::Enum)]
pub enum CallEventKind {
    /// Someone is calling us.
    IncomingInvite { video: bool },
    /// The other side picked up.
    Accepted,
    /// The other side declined.
    Rejected,
    /// The other side hung up (or the call was cancelled before being answered).
    Ended,
}

/// A call signaling update -- poll this (like `pollMessage`) to drive incoming-call UI,
/// and to know when the other side answers/declines/hangs up.
#[derive(uniffi::Record)]
pub struct CallEvent {
    /// Hex-encoded call id, shared by both sides of this call.
    pub call_id: String,
    /// Full hex node id of the other party -- pass this back to `acceptCall`/
    /// `rejectCall`/`endCall`/`sendCallFrame` to address them.
    pub remote_node_id: String,
    /// Short display id of the other party.
    pub remote_short_id: String,
    pub kind: CallEventKind,
}

/// One frame of live audio or video from an ongoing call -- poll this frequently (e.g.
/// every 20-40ms) from a dedicated playback loop, separately from `pollMessage`/
/// `pollTransferProgress`, since call audio in particular needs low, steady latency.
#[derive(uniffi::Record)]
pub struct CallFrameUpdate {
    pub call_id: String,
    pub remote_node_id: String,
    pub media: CallMediaKind,
    pub sequence: u32,
    pub data: Vec<u8>,
}

/// A running mesh node. Construct one per app session; keep it alive for as long as the
/// app wants to stay part of the mesh (e.g. behind a singleton or view-model on the
/// Swift/Kotlin side).
#[derive(uniffi::Object)]
pub struct MeshClient {
    runtime: tokio::runtime::Runtime,
    node: Arc<MeshNode<UdpTransport>>,
    inbox: Arc<Mutex<VecDeque<ReceivedMessage>>>,
    /// Live progress for in-flight sends and receives -- see `TransferProgressUpdate`.
    progress: Arc<Mutex<VecDeque<TransferProgressUpdate>>>,
    /// Call signaling news (incoming invites, accept/reject/end) -- see `CallEvent`.
    call_events: Arc<Mutex<VecDeque<CallEvent>>>,
    /// Live audio/video frames for ongoing calls -- see `CallFrameUpdate`.
    call_frames: Arc<Mutex<VecDeque<CallFrameUpdate>>>,
    discovery: Mutex<Option<LanDiscovery>>,
    /// Cache of every contact ever discovered, keyed by `NodeId` -- see
    /// `ContactIdentity`. This, not the transient `discovered_peers()` radar view, is
    /// what `send`/`sendFile` actually look up: a contact seen yesterday but not
    /// currently broadcasting must still be sendable-to (store-and-forward-friendly),
    /// which this cache -- unlike live discovery -- makes possible. Since Milestone
    /// 2B.2a this is backed by `contact_store` (when the app supplied a
    /// `contacts_db_path`), so it also survives an app restart, not only this session.
    contacts: Mutex<HashMap<mesh_core::NodeId, ContactRecord>>,
    /// New-contact/identity-change news -- see `ContactEvent`.
    contact_events: Mutex<VecDeque<ContactEvent>>,
    /// Persistent on-disk backing for `contacts`, if the app supplied a
    /// `contacts_db_path` -- `None` means contacts are in-memory only for this session
    /// (e.g. a test, or an app that hasn't been updated to pass a path yet).
    contact_store: Option<ContactStore>,
    display_name: String,
    node_id: mesh_core::NodeId,
    node_id_str: String,
    /// Hex-encoded 32-byte Ed25519 seed backing this session's identity -- see
    /// `identitySeed()`. Kept around so the app can persist it after construction
    /// regardless of whether it was freshly generated or reused from a previous launch.
    identity_seed_hex: String,
    /// Hex-encoded 32-byte at-rest storage key for `InboxStore`'s content -- see
    /// `inboxStorageKey()`. Milestone 3C.1.
    inbox_storage_key_hex: String,
}

const INBOX_CAPACITY: usize = 500;
/// Bounded like the inbox; progress updates arrive far more often (once per chunk) so
/// this is generous, but a burst of many concurrent transfers still can't grow it
/// unbounded.
const PROGRESS_CAPACITY: usize = 2000;
const CALL_EVENT_CAPACITY: usize = 100;
/// Call frames arrive continuously (dozens per second) while a call is active. Bounded so
/// a UI that falls behind on polling (e.g. backgrounded) doesn't grow this unboundedly --
/// old, stale audio/video frames aren't worth keeping around anyway.
const CALL_FRAME_CAPACITY: usize = 500;

/// Configuration for [`MeshClient::new`], bundled into a single UniFFI record instead of
/// many individual constructor parameters.
///
/// # Why a record and not separate parameters
/// This constructor used to take 12 individual parameters (11 of them non-primitive --
/// `String`/`Vec<String>`/`Option<String>`/`Option<Vec<u8>>`). On Android, each
/// non-primitive UniFFI argument is lowered into its own `RustBuffer` struct and passed
/// to the native library as a separate struct-by-value parameter via JNA. This triggers
/// a known, unresolved upstream bug where JNA corrupts struct-by-value marshaling on
/// Android ARM64 when a single native call has many such parameters, surfacing as
/// `RustBuffer length exceeds capacity` / `null RustBuffer had non-zero capacity` --
/// see <https://github.com/mozilla/uniffi-rs/issues/2624> (confirmed by multiple
/// unrelated projects: Wire, Proton Mail, chaintope/rust-tapyrus-wallet-ffi,
/// worldcoin/bedrock -- the uniffi-rs maintainers have no fix yet, only a long-term
/// plan to stop using JNA for structs). Bundling everything into one `Record` means
/// exactly one `RustBuffer` crosses the FFI boundary for this call instead of eleven,
/// which avoids the trigger condition. Swift/iOS was never affected (it doesn't go
/// through JNA), but both platforms use this same constructor for one shared API.
#[derive(uniffi::Record)]
pub struct MeshClientConfig {
    pub display_name: String,
    pub listen_addr: String,
    pub peer_addrs: Vec<String>,
    pub channel_passphrase: String,
    pub ttl: u8,
    #[uniffi(default = None)]
    pub identity_seed: Option<Vec<u8>>,
    #[uniffi(default = None)]
    pub contacts_db_path: Option<String>,
    #[uniffi(default = None)]
    pub replay_store_path: Option<String>,
    #[uniffi(default = None)]
    pub delivery_store_path: Option<String>,
    #[uniffi(default = None)]
    pub forward_store_path: Option<String>,
    #[uniffi(default = None)]
    pub inbox_store_path: Option<String>,
    #[uniffi(default = None)]
    pub inbox_storage_key: Option<Vec<u8>>,
}

#[uniffi::export]
impl MeshClient {
    /// Starts a new mesh node.
    ///
    /// All fields below are documented on [`MeshClientConfig`]'s individual fields;
    /// this constructor takes a single `config: MeshClientConfig` argument (see that
    /// type's doc comment for why) rather than one parameter per field:
    ///
    /// - `display_name`: shown alongside your messages.
    /// - `listen_addr`: local address to listen on, e.g. "0.0.0.0:9001".
    /// - `peer_addrs`: addresses of directly-reachable peers, e.g. those on the same
    ///   Wi-Fi hotspot -- this is your simulated "radio range" (see the BLE transport
    ///   roadmap for genuinely radio-range-limited discovery instead of a fixed list).
    /// - `channel_passphrase`: only devices using the same passphrase can read messages.
    /// - `ttl`: max hop count a message can travel before being dropped.
    /// - `identity_seed`: a previously-persisted identity seed (see `identitySeed()`), so
    ///   this device keeps the same `NodeId` across app restarts instead of generating a
    ///   brand-new random identity every launch -- pass `None` (or anything other than
    ///   exactly 32 bytes) on first-ever launch, then persist whatever `identitySeed()`
    ///   returns (e.g. in the iOS Keychain or Android Keystore-backed storage) and pass
    ///   it back in on every subsequent launch. Treat this value like a private key.
    /// - `contacts_db_path`: a file path, in app-private storage (e.g. the iOS Documents
    ///   directory, or Android's internal files dir), where the persistent contact cache
    ///   (see `ContactIdentity`/Milestone 2B.2a's `ContactStore`) is loaded from and saved
    ///   to. Pass `None` to keep contacts in-memory only for this session (e.g. in
    ///   tests) -- every real app should pass a real path so a contact discovered
    ///   yesterday can still be sent to (while offline) after a restart.
    /// - `replay_store_path`: a file path (Milestone 2D), separate from `contacts_db_path`,
    ///   where durable replay protection (`mesh_core::ReplayStore` -- which
    ///   `(sender, message_id)` pairs have already been processed) is persisted. Pass
    ///   `None` to keep replay protection in-memory only for this session (it still
    ///   works, just doesn't survive a restart) -- every real app should pass a real
    ///   path so a captured/replayed message can't be re-delivered just by killing and
    ///   relaunching the app.
    /// - `delivery_store_path`: a file path (Milestone 3A), where this device's own
    ///   not-yet-acknowledged outgoing reliable messages (`send`, via
    ///   `MeshNode::send_reliable_text`) and their retry/backoff state are persisted.
    ///   Pass `None` to keep this in-memory only (a message still queued when the app
    ///   is killed won't resume retrying after a restart) -- every real app should pass
    ///   a real path so a message being retried survives a restart.
    /// - `forward_store_path`: a file path (Milestone 3B), where this device's
    ///   per-neighbor relay-forwarding retry state is persisted -- relevant whenever
    ///   this device is relaying `DirectV1` traffic for other devices on the mesh, not
    ///   only when it's the sender/recipient. Pass `None` to keep this in-memory only.
    /// - `inbox_store_path`: a file path (Milestone 3C), where every durably-accepted
    ///   received message is persisted -- this is the actual source of truth for chat
    ///   history (see `chatHistory`), not just an in-memory cache; an authenticated
    ///   `DeliveryAck` is only ever sent once a message has been durably written here.
    ///   Pass `None` to keep this in-memory only (received chat history is lost on
    ///   restart) -- every real app should pass a real path.
    /// - `inbox_storage_key`: a previously-persisted 32-byte at-rest encryption key
    ///   (see `inboxStorageKey()`) protecting the *content* of everything in
    ///   `inbox_store_path` (Milestone 3C.1) -- without this, durable chat history
    ///   would sit as plaintext in that SQLite file, readable by anyone who can reach
    ///   app-private storage (a device backup, a rooted/jailbroken filesystem read,
    ///   etc.) without ever going through the app itself. Pass `None` (or anything
    ///   other than exactly 32 bytes) on first-ever launch, then persist whatever
    ///   `inboxStorageKey()` returns -- in the iOS Keychain / Android Keystore-backed
    ///   storage, the same way `identitySeed()` already is -- and pass it back in on
    ///   every subsequent launch. Treat this value like a private key; losing it
    ///   makes any previously-stored chat history permanently unreadable (see
    ///   `InboxStore`'s doc on a wrong/missing key failing closed).
    #[uniffi::constructor]
    pub fn new(config: MeshClientConfig) -> Result<Arc<Self>, MeshError> {
        let MeshClientConfig {
            display_name,
            listen_addr,
            peer_addrs,
            channel_passphrase,
            ttl,
            identity_seed,
            contacts_db_path,
            replay_store_path,
            delivery_store_path,
            forward_store_path,
            inbox_store_path,
            inbox_storage_key,
        } = config;
        init_logging();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| MeshError::Network(e.to_string()))?;

        let identity = resolve_identity(identity_seed);
        let identity_seed_hex = hex_encode(&identity.seed());
        let node_id = short_id(&identity.node_id());
        let channel_key = ChannelKey::from_passphrase(&channel_passphrase);
        let inbox_storage_key = resolve_storage_key(inbox_storage_key);
        let inbox_storage_key_hex = hex_encode(&inbox_storage_key);

        let (contact_store, initial_contacts) = match contacts_db_path {
            Some(path) => {
                let (store, contacts) = ContactStore::open(path);
                (Some(store), contacts)
            }
            None => (None, HashMap::new()),
        };

        let replay_store = match replay_store_path {
            Some(path) => {
                let (store, was_reset) = mesh_core::ReplayStore::open(path);
                if was_reset {
                    // The previous replay-protection history is gone (the on-disk file
                    // was corrupt and had to be replaced) -- this must be surfaced, not
                    // silently swallowed, since any (sender, message_id) pairs recorded
                    // before this reset are no longer remembered.
                    log::warn!("mesh-mobile: replay protection history was reset (previous store was corrupt/unreadable)");
                }
                store
            }
            None => mesh_core::ReplayStore::in_memory(),
        };

        let delivery_store = match delivery_store_path {
            Some(path) => {
                let (store, was_reset) = mesh_core::DeliveryStore::open(path);
                if was_reset {
                    log::warn!("mesh-mobile: outbound delivery/retry state was reset (previous store was corrupt/unreadable)");
                }
                store
            }
            None => mesh_core::DeliveryStore::in_memory(),
        };

        let forward_store = match forward_store_path {
            Some(path) => {
                let (store, was_reset) = mesh_core::ForwardStore::open(path);
                if was_reset {
                    log::warn!("mesh-mobile: relay forwarding retry state was reset (previous store was corrupt/unreadable)");
                }
                store
            }
            None => mesh_core::ForwardStore::in_memory(),
        };

        let inbox_store = match inbox_store_path {
            Some(path) => {
                let (store, was_reset) = mesh_core::InboxStore::open(path, inbox_storage_key);
                if was_reset {
                    // Milestone 3C: unlike the other stores, a reset here means
                    // previously-received chat history is genuinely gone, not just
                    // replay-protection bookkeeping -- still fails safe (starts empty)
                    // rather than panicking, but this is the most user-visible of the
                    // resets this constructor can report.
                    log::warn!("mesh-mobile: durable chat history was reset (previous inbox store was corrupt/unreadable)");
                }
                store
            }
            None => mesh_core::InboxStore::in_memory(),
        };

        let transport = runtime
            .block_on(UdpTransport::bind(&listen_addr, peer_addrs))
            .map_err(|e| MeshError::Network(e.to_string()))?;

        let raw_node_id = identity.node_id();
        let node = Arc::new(MeshNode::new_with_stores(identity, channel_key, transport, ttl, replay_store, delivery_store, forward_store, inbox_store));
        let inbox: Arc<Mutex<VecDeque<ReceivedMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
        let progress: Arc<Mutex<VecDeque<TransferProgressUpdate>>> = Arc::new(Mutex::new(VecDeque::new()));
        let call_events: Arc<Mutex<VecDeque<CallEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let call_frames: Arc<Mutex<VecDeque<CallFrameUpdate>>> = Arc::new(Mutex::new(VecDeque::new()));

        let recv_node = node.clone();
        let recv_inbox = inbox.clone();
        let recv_progress = progress.clone();
        let recv_call_events = call_events.clone();
        let recv_call_frames = call_frames.clone();
        runtime.spawn(async move {
            loop {
                let raw = match recv_node.recv_raw().await {
                    Ok(raw) => raw,
                    Err(_) => break, // transport closed; stop the background loop
                };
                match recv_node.handle_incoming(raw).await {
                    Ok(Some(mesh_core::IncomingEvent::Content(delivered))) => {
                        // Milestone 3C: `mesh-core` already durably persisted this (to
                        // `inbox_store_path`) and, for a `DirectV1` message, already
                        // acknowledged it to the sender -- there is nothing left here
                        // to defer or remember for later.
                        let message = to_received_message(delivered.sender, delivered.message_id, delivered.content);
                        let mut inbox = recv_inbox.lock().unwrap();
                        inbox.push_back(message);
                        if inbox.len() > INBOX_CAPACITY {
                            inbox.pop_front();
                        }
                    }
                    Ok(Some(mesh_core::IncomingEvent::Progress(_sender, p))) => {
                        let mut progress = recv_progress.lock().unwrap();
                        progress.push_back(to_progress_update(p, TransferDirection::Receiving));
                        if progress.len() > PROGRESS_CAPACITY {
                            progress.pop_front();
                        }
                    }
                    Ok(Some(mesh_core::IncomingEvent::Call(sender, message))) => match message {
                        mesh_core::CallMessage::Signal(signal) => {
                            let (call_id, kind) = match signal {
                                mesh_core::CallSignal::Invite { call_id, video } => {
                                    (call_id, CallEventKind::IncomingInvite { video })
                                }
                                mesh_core::CallSignal::Accept { call_id } => (call_id, CallEventKind::Accepted),
                                mesh_core::CallSignal::Reject { call_id } => (call_id, CallEventKind::Rejected),
                                mesh_core::CallSignal::End { call_id } => (call_id, CallEventKind::Ended),
                            };
                            let mut events = recv_call_events.lock().unwrap();
                            events.push_back(CallEvent {
                                call_id: hex_encode(&call_id),
                                remote_node_id: hex_encode(&sender),
                                remote_short_id: short_id(&sender),
                                kind,
                            });
                            if events.len() > CALL_EVENT_CAPACITY {
                                events.pop_front();
                            }
                        }
                        mesh_core::CallMessage::Frame(frame) => {
                            let mut frames = recv_call_frames.lock().unwrap();
                            frames.push_back(CallFrameUpdate {
                                call_id: hex_encode(&frame.call_id),
                                remote_node_id: hex_encode(&sender),
                                media: frame.media.into(),
                                sequence: frame.sequence,
                                data: frame.data,
                            });
                            if frames.len() > CALL_FRAME_CAPACITY {
                                frames.pop_front();
                            }
                        }
                    },
                    Ok(None) => {} // duplicate, invalid, or not decryptable by us
                    Err(_) => {} // malformed packet; drop and keep listening
                }
            }
        });

        // Milestone 3A/3B: periodically retry this device's own not-yet-acknowledged
        // reliable messages (`retry_due_deliveries`) and any relayed message that
        // hasn't yet reached every neighbor known when it first arrived
        // (`retry_pending_forwards`) -- see `send`'s doc. Every 2 seconds is
        // comfortably inside the "every 1-2 seconds" both methods' own docs call for,
        // without polling so often it wastes battery.
        let retry_node = node.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                retry_node.retry_due_deliveries().await;
                retry_node.retry_pending_forwards().await;
            }
        });

        Ok(Arc::new(Self {
            runtime,
            node,
            inbox,
            progress,
            call_events,
            call_frames,
            discovery: Mutex::new(None),
            contacts: Mutex::new(initial_contacts),
            contact_events: Mutex::new(VecDeque::new()),
            contact_store,
            display_name,
            node_id: raw_node_id,
            node_id_str: node_id,
            identity_seed_hex,
            inbox_storage_key_hex,
        }))
    }

    /// This device's persistent identity seed (32 bytes, hex-encoded) -- persist this
    /// securely (iOS Keychain, Android Keystore-backed storage) and pass it back into the
    /// `identitySeed` constructor parameter on the next launch so this device keeps the
    /// same `NodeId` instead of becoming a new, unrecognized identity every restart.
    /// Treat this exactly like a private key: anyone with it can sign messages as you.
    pub fn identity_seed(&self) -> String {
        self.identity_seed_hex.clone()
    }

    /// Milestone 3C.1: this session's at-rest storage key protecting `InboxStore`'s
    /// content (32 bytes, hex-encoded) -- persist this securely (iOS Keychain, Android
    /// Keystore-backed storage) and pass it back into the `inboxStorageKey`
    /// constructor parameter on the next launch, the same way `identitySeed()` already
    /// works. Treat this like a private key: losing it makes any previously-stored
    /// chat history permanently unreadable; anyone who obtains it can read the
    /// contents of `inbox_store_path`.
    pub fn inbox_storage_key(&self) -> String {
        self.inbox_storage_key_hex.clone()
    }

    /// This device's short node id (derived from its public key).
    pub fn node_id(&self) -> String {
        self.node_id_str.clone()
    }

    /// This device's full hex-encoded node id -- give this to someone (or exchange it via
    /// discovery's `fullNodeId` on `DiscoveredPeer`) so they can address a call to you via
    /// `startCall`.
    pub fn full_node_id(&self) -> String {
        hex_encode(&self.node_id)
    }

    /// Sends a direct text message to `remote_node_id` -- one specific conversation
    /// partner, not a broadcast to everyone on the channel, so per-contact chat threads
    /// work. Uses "MeshTalk Direct Encryption v1" (per-recipient session keys, not the
    /// shared channel passphrase) -- looks up `remote_node_id`'s `PublicIdentity` in the
    /// contact cache (populated by `discoveredPeers()`/`startDiscovery()`).
    ///
    /// Milestone 3C: always uses `MeshNode::send_reliable_text`, so ordinary chat text
    /// gets durable, restart-surviving retry-until-acknowledged delivery (see that
    /// method's doc -- `MeshClient::new`'s periodic retry loop keeps retrying it until
    /// an ack arrives or it expires). Deliberately does **not** fall back to the older
    /// best-effort `MeshNode::send_text` for a message too long for the single-chunk
    /// reliable path -- silently downgrading a message's delivery guarantee just
    /// because it happened to be a little longer is exactly the kind of surprise this
    /// API must never produce. Returns `SendOutcome::TooLongForReliableText` instead;
    /// reliable delivery for longer text via chunked transfer is future work.
    pub fn send(&self, remote_node_id: String, text: String) -> SendOutcome {
        let Some(target) = hex_decode::<32>(&remote_node_id) else { return SendOutcome::Failed };
        let Some(public_identity) = self.contacts.lock().unwrap().get(&target).map(|c| c.public_identity.clone()) else {
            log::warn!("mesh-mobile: send() refused -- no known identity for recipient={}", short_id(&target));
            return SendOutcome::Failed; // unknown identity -- fail closed, never fall back to ChannelV1
        };
        if text.as_bytes().len() > mesh_core::CHUNK_SIZE {
            log::warn!(
                "mesh-mobile: send() refused -- text too long for reliable delivery ({} bytes, max {})",
                text.as_bytes().len(),
                mesh_core::CHUNK_SIZE
            );
            return SendOutcome::TooLongForReliableText;
        }
        let outcome = match self.runtime.block_on(self.node.send_reliable_text(&public_identity, &text)) {
            Ok(()) => SendOutcome::Sent,
            Err(_) => SendOutcome::Failed,
        };
        log::debug!("mesh-mobile: send() to recipient={} mode=DirectV1 outcome={:?}", short_id(&target), outcome);
        outcome
    }

    /// Milestone 3C: every durably-accepted message this device has ever received,
    /// oldest first -- call this once on launch to hydrate the chat-history UI from
    /// disk (via `inbox_store_path`) instead of starting from an empty list. Unlike
    /// `pollMessage()` (which only ever returns each message once, as it arrives), this
    /// can be called repeatedly and always returns the full history.
    ///
    /// Loads the *entire* history across every conversation -- fine for the message
    /// volumes this app deals with today, but prefer `chatHistoryPage` for a single
    /// conversation once history grows large (Milestone 3C.1: don't build UI
    /// assumptions around always hydrating the whole database).
    pub fn chat_history(&self) -> Vec<ReceivedMessage> {
        self.node.inbox_messages().into_iter().map(|m| to_received_message(m.sender, m.message_id, m.content)).collect()
    }

    /// Milestone 3C.1: a single conversation's durably-accepted messages, newest
    /// first, bounded to `limit` -- the paginated alternative to `chatHistory` a UI
    /// should move toward once a conversation's history grows large. Call with
    /// `beforeReceivedAtMs: null` for the most recent page; pass the last item's
    /// `receivedAtMs` from the previous page to fetch the next (older) page.
    ///
    /// Returns `[]` (rather than an error) for an invalid `remoteNodeId`.
    pub fn chat_history_page(&self, remote_node_id: String, before_received_at_ms: Option<u64>, limit: u32) -> Vec<ReceivedMessage> {
        let Some(peer) = hex_decode::<32>(&remote_node_id) else { return Vec::new() };
        self.node
            .inbox_messages_for_peer(&peer, before_received_at_ms, limit as usize)
            .into_iter()
            .map(|m| to_received_message(m.sender, m.message_id, m.content))
            .collect()
    }

    /// Sends a direct file attachment (image, video, voice note, or generic file) to
    /// `remote_node_id`, split into chunks so it flows through the same relay path as any
    /// other message, encrypted the same "MeshTalk Direct Encryption v1" way as `send`
    /// (see its doc for the fail-closed/no-ChannelV1-fallback behavior -- the same rules
    /// apply here). Large attachments (especially video) may not reliably arrive over
    /// many hops -- the mesh has no retransmission -- so this works best for images and
    /// short voice notes.
    ///
    /// Returns immediately (`true` means "accepted and started", not "fully delivered") --
    /// the actual send happens in the background so a large attachment doesn't block the
    /// caller for the several seconds it might take. Poll `pollTransferProgress()` to
    /// track it (the final update, where `doneChunks == totalChunks`, means it's been
    /// fully handed off to the network). Returns `false` immediately (nothing is sent) if
    /// `remote_node_id` isn't valid hex or this contact's identity isn't known yet.
    pub fn send_file(&self, remote_node_id: String, data: Vec<u8>, file_name: String, mime_type: String, kind: AttachmentKind) -> bool {
        let Some(target) = hex_decode::<32>(&remote_node_id) else { return false };
        let Some(public_identity) = self.contacts.lock().unwrap().get(&target).map(|c| c.public_identity.clone()) else {
            log::warn!("mesh-mobile: send_file() refused -- no known identity for recipient={}", short_id(&target));
            return false; // unknown identity -- fail closed, never fall back to ChannelV1
        };
        log::debug!("mesh-mobile: send_file() started to recipient={} bytes={}", short_id(&target), data.len());
        let node = self.node.clone();
        let progress = self.progress.clone();
        let core_kind: mesh_core::ContentKind = kind.into();
        let log_target = target;
        self.runtime.spawn(async move {
            let progress_for_callback = progress.clone();
            let result = node
                .send_file(&public_identity, core_kind, file_name, mime_type, &data, move |p| {
                    let mut progress = progress_for_callback.lock().unwrap();
                    progress.push_back(to_progress_update(p, TransferDirection::Sending));
                    if progress.len() > PROGRESS_CAPACITY {
                        progress.pop_front();
                    }
                })
                .await;
            if result.is_err() {
                log::warn!("mesh-mobile: send_file() to recipient={} failed", short_id(&log_target));
            }
        });
        true
    }

    /// Non-blocking: returns the next received message if one is waiting, or `None`.
    /// Call this from a UI timer/poll loop (e.g. every 200-500ms) rather than blocking a
    /// UI thread on it.
    pub fn poll_message(&self) -> Option<ReceivedMessage> {
        self.inbox.lock().unwrap().pop_front()
    }

    /// Non-blocking: returns the next transfer-progress update if one is waiting, or
    /// `None`. Call this from the same UI timer/poll loop as `pollMessage` to drive a live
    /// progress bar for sends and receives.
    pub fn poll_transfer_progress(&self) -> Option<TransferProgressUpdate> {
        self.progress.lock().unwrap().pop_front()
    }

    /// Calls another node directly (must already be a reachable peer, e.g. via
    /// discovery/`addPeer`, or reachable through one that is). Returns the new call's
    /// hex-encoded id (hang on to it -- you'll need it for `endCall`/`sendCallFrame`), or
    /// `None` if `remote_node_id` isn't valid hex.
    ///
    /// This only sends the invite; wait for a `CallEvent` with `kind == .accepted` (via
    /// `pollCallEvent`) before starting to send audio/video frames.
    pub fn start_call(&self, remote_node_id: String, video: bool) -> Option<String> {
        let target: mesh_core::NodeId = hex_decode(&remote_node_id)?;
        let call_id = mesh_core::call::random_call_id();
        let node = self.node.clone();
        self.runtime.spawn(async move {
            let _ = node.call_invite(target, call_id, video).await;
        });
        Some(hex_encode(&call_id))
    }

    /// Answers an incoming call (from the `remoteNodeId`/`callId` on the `CallEvent` that
    /// reported it). Start sending audio/video frames right after calling this.
    ///
    /// Sends the signal a few times a short moment apart (like `reject`/`end`) since it's
    /// a single UDP packet with no acknowledgement -- if it's lost, the other side is
    /// stuck showing "ringing" until it times out on its own, which nothing in this app
    /// currently does. Repeating it is cheap and harmless (the receiving side's handling
    /// of `Accepted`/`Rejected`/`Ended` is idempotent -- a repeat after the first one's
    /// already been acted on is simply a no-op).
    pub fn accept_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::Accept { call_id }).await;
        });
        true
    }

    /// Declines an incoming call. See `acceptCall` for why this retries a few times.
    pub fn reject_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::Reject { call_id }).await;
        });
        true
    }

    /// Ends a call in progress, or cancels one that hasn't been answered yet. See
    /// `acceptCall` for why this retries a few times.
    pub fn end_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::End { call_id }).await;
        });
        true
    }

    /// Sends one frame of live audio or video to `remote_node_id` for the given call.
    /// Fire-and-forget, like the rest of the mesh -- no acknowledgement, no retry. Call
    /// this from a dedicated capture loop (e.g. every ~20ms for audio).
    ///
    /// Unlike `sendFile`, this blocks briefly (a single encrypt+sign+socket-send, well
    /// under a millisecond) rather than spawning a detached background task -- frames
    /// must go out in the order the caller sends them, and spawning a separate task per
    /// frame lets the async runtime interleave/reorder them across worker threads
    /// (observed directly: frames arrived out of order in testing once this used
    /// `runtime.spawn` per frame instead of blocking).
    pub fn send_call_frame(
        &self,
        remote_node_id: String,
        call_id: String,
        media: CallMediaKind,
        sequence: u32,
        data: Vec<u8>,
    ) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let core_media: mesh_core::MediaKind = media.into();
        self.runtime
            .block_on(self.node.send_call_frame(target, call_id, core_media, sequence, data))
            .is_ok()
    }

    /// Non-blocking: returns the next call signaling update (incoming invite, or the
    /// other side accepting/rejecting/ending), or `None`. Call this from a UI timer/poll
    /// loop, same cadence as `pollMessage`.
    pub fn poll_call_event(&self) -> Option<CallEvent> {
        self.call_events.lock().unwrap().pop_front()
    }

    /// Non-blocking: returns the next incoming audio/video frame for an active call, or
    /// `None`. Call this from a dedicated, frequently-polled loop (e.g. every ~20ms)
    /// separate from `pollMessage`/`pollTransferProgress`, since call audio especially
    /// needs steady, low latency.
    pub fn poll_call_frame(&self) -> Option<CallFrameUpdate> {
        self.call_frames.lock().unwrap().pop_front()
    }

    /// Starts broadcasting this device's presence on the local Wi-Fi network and
    /// listening for others -- like turning on Bluetooth/Wi-Fi discovery, instead of
    /// requiring the user to type in an IP address. Advertises this device's real
    /// `PublicIdentity` (X25519 key + Ed25519 binding signature), not just its `NodeId`
    /// -- see `mesh_transport_udp::discovery`'s "Milestone 2B.2" doc -- so peers can
    /// actually send this device a real "MeshTalk Direct Encryption v1" message. Safe to
    /// call more than once (only the first call actually starts it).
    pub fn start_discovery(&self) -> Result<(), MeshError> {
        let mut discovery = self.discovery.lock().unwrap();
        if discovery.is_some() {
            return Ok(()); // already running
        }
        let port = self
            .node
            .transport()
            .local_addr()
            .map_err(|e| MeshError::Network(e.to_string()))?
            .port();
        let started = self
            .runtime
            .block_on(LanDiscovery::start(self.node.public_identity(), self.display_name.clone(), port))
            .map_err(|e| MeshError::Network(e.to_string()))?;
        *discovery = Some(started);
        Ok(())
    }

    /// Nearby devices found via LAN discovery, ready to connect to with one tap. Call
    /// `start_discovery()` first; call this from a UI timer/poll loop (e.g. every 1-2s).
    /// Also maintains the persistent contact cache (see `contacts()`/`pollContactEvent()`)
    /// as a side effect -- this is the only place that cache is updated from, since it's
    /// already polled regularly by every app using discovery.
    pub fn discovered_peers(&self) -> Vec<DiscoveredPeer> {
        let discovery = self.discovery.lock().unwrap();
        let Some(discovery) = discovery.as_ref() else {
            return Vec::new();
        };
        let peers = discovery.discovered_peers();
        self.update_contact_cache(&peers);
        peers
            .into_iter()
            .map(|p| DiscoveredPeer {
                node_id: short_id(&p.node_id),
                full_node_id: hex_encode(&p.node_id),
                display_name: p.display_name,
                address: p.address,
                pairing_code: p.pairing_code,
            })
            .collect()
    }

    /// Every contact this device has ever discovered this session, keyed internally by
    /// `NodeId` -- unlike `discoveredPeers()` (a live radar view), this includes contacts
    /// not currently broadcasting (e.g. seen yesterday, offline right now). This is what
    /// makes `send`/`sendFile` able to encrypt for someone who isn't nearby anymore --
    /// the prerequisite for an eventual store-and-forward design.
    pub fn contacts(&self) -> Vec<ContactIdentity> {
        let contacts = self.contacts.lock().unwrap();
        contacts
            .values()
            .map(|c| ContactIdentity {
                node_id: short_id(&c.public_identity.node_id),
                full_node_id: hex_encode(&c.public_identity.node_id),
                advertised_name: c.advertised_name.clone().unwrap_or_default(),
                first_seen_ms: c.first_seen_ms,
                last_seen_ms: c.last_seen_ms,
                verification: c.verification.into(),
                identity_change_pending: c.identity_change_pending,
            })
            .collect()
    }

    /// Non-blocking: returns the next new-contact/identity-change news, or `None`. Call
    /// this from the same UI timer/poll loop as `pollMessage`. An `IdentityChanged` event
    /// means a previously-known contact just advertised different cryptographic material
    /// -- the UI must surface this prominently (see `ContactEventKind`'s doc), not
    /// silently accept it.
    pub fn poll_contact_event(&self) -> Option<ContactEvent> {
        self.contact_events.lock().unwrap().pop_front()
    }

    /// Call once the local user has seen and dismissed an `IdentityChanged` warning for
    /// `remote_node_id` -- persists immediately (see `ContactRecord::acknowledge_identity_change`)
    /// so the warning doesn't silently reappear (or silently vanish unacknowledged) on
    /// the next app restart. Does **not** mark the contact as verified -- acknowledging a
    /// warning and out-of-band verifying an identity are different actions. No-op (and
    /// returns `false`) if `remote_node_id` isn't a known contact.
    pub fn acknowledge_identity_change(&self, remote_node_id: String) -> bool {
        let Some(target) = hex_decode::<32>(&remote_node_id) else { return false };
        let mut contacts = self.contacts.lock().unwrap();
        let Some(existing) = contacts.get_mut(&target) else { return false };
        existing.acknowledge_identity_change();
        self.persist_contacts(&contacts);
        log::debug!("mesh-mobile: identity-change warning acknowledged for contact={}", short_id(&target));
        true
    }

    /// Adds a discovered (or manually entered) peer address as a directly-reachable relay
    /// target, without needing to reconnect/restart the client.
    pub fn add_peer(&self, address: String) {
        self.runtime.block_on(self.node.transport().add_peer(address));
    }

    /// Explicitly wipes every stored contact, in memory and on disk (if a
    /// `contacts_db_path` was configured) -- e.g. an app's "reset identity"/"forget all
    /// contacts" settings action. Deliberately not called implicitly from anywhere else
    /// (not from `disconnect`, not from identity changes, not from anything else in this
    /// file) -- resetting contact history is always an explicit, separate user action.
    pub fn reset_contacts(&self) {
        let mut contacts = self.contacts.lock().unwrap();
        let count = contacts.len();
        contacts.clear();
        if let Some(store) = &self.contact_store {
            store.clear();
        }
        log::warn!("mesh-mobile: reset_contacts() -- explicitly wiped {} contact(s)", count);
    }
}

// Not part of the `#[uniffi::export]` impl block above -- UniFFI's export macro tries to
// generate FFI bindings for *every* item in an annotated impl block, including private
// helpers, and this method's parameter type (`mesh_transport_udp::DiscoveredPeer`) isn't
// (and doesn't need to be) UniFFI-exposed.
impl MeshClient {
    /// Writes the current contact cache to disk via `contact_store`, if one was
    /// configured -- a no-op (not an error) when the app didn't supply a
    /// `contacts_db_path`. Shared by every mutation path (`update_contact_cache`,
    /// `acknowledge_identity_change`) so there is exactly one place that decides how
    /// persistence happens.
    fn persist_contacts(&self, contacts: &HashMap<mesh_core::NodeId, ContactRecord>) {
        if let Some(store) = &self.contact_store {
            store.save(contacts);
        }
    }

    /// Inserts newly-discovered contacts, updates `last_seen`/advertised name for known
    /// ones, and detects identity changes -- see `ContactEventKind::IdentityChanged`'s
    /// doc for why a change is never silently accepted as the new trusted state. Persists
    /// the updated cache to disk (via `contact_store`, if one was configured) whenever
    /// anything actually changed, so a contact discovered just before the app is killed
    /// isn't lost.
    fn update_contact_cache(&self, discovered: &[mesh_transport_udp::DiscoveredPeer]) {
        let mut contacts = self.contacts.lock().unwrap();
        let mut events = self.contact_events.lock().unwrap();
        let now = now_millis();
        let mut changed = false;
        for peer in discovered {
            match contacts.get_mut(&peer.node_id) {
                Some(existing) => {
                    if existing.public_identity.x25519_public != peer.public_identity.x25519_public {
                        // A known NodeId is now advertising *different* cryptographic
                        // material. Update the stored identity (so future sends use the
                        // key that actually works) but deliberately reset verification
                        // to Unverified and raise a *persistent* pending-warning flag --
                        // never silently keep whatever trust state existed for the old
                        // key, and never let the warning quietly disappear on restart
                        // before the user has actually seen it.
                        existing.public_identity = peer.public_identity.clone();
                        existing.mark_identity_change_pending();
                        existing.advertised_name = Some(peer.display_name.clone());
                        existing.touch_last_seen(now);
                        events.push_back(ContactEvent {
                            full_node_id: hex_encode(&peer.node_id),
                            kind: ContactEventKind::IdentityChanged,
                        });
                        changed = true;
                        log::warn!("mesh-mobile: identity CHANGED for contact={} -- reset to Unverified, warning persisted", short_id(&peer.node_id));
                    } else {
                        existing.advertised_name = Some(peer.display_name.clone());
                        existing.touch_last_seen(now);
                        changed = true;
                    }
                }
                None => {
                    let record = ContactRecord::new(peer.public_identity.clone(), Some(peer.display_name.clone()), now);
                    contacts.insert(peer.node_id, record);
                    events.push_back(ContactEvent {
                        full_node_id: hex_encode(&peer.node_id),
                        kind: ContactEventKind::Discovered,
                    });
                    changed = true;
                    log::debug!("mesh-mobile: new contact discovered={}", short_id(&peer.node_id));
                }
            }
            if events.len() > CONTACT_EVENT_CAPACITY {
                events.pop_front();
            }
        }
        if changed {
            self.persist_contacts(&contacts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A guaranteed-unique temp directory for one test -- combines a nanosecond
    /// timestamp with a per-process atomic counter, so parallel test execution can
    /// never collide two tests onto the same path the way a millisecond-resolution
    /// timestamp alone occasionally could.
    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        std::env::temp_dir().join(format!("meshtalk-mobile-test-{label}-{nanos}-{n}"))
    }

    /// The core restart-persistence guarantee: a previously-saved 32-byte seed must
    /// reconstruct to the exact same `NodeId`, otherwise every app restart would
    /// silently become a new, unrecognized device on the mesh.
    #[test]
    fn resolve_identity_reuses_a_valid_saved_seed() {
        let original = Identity::generate();
        let seed = original.seed().to_vec();
        let restored = resolve_identity(Some(seed));
        assert_eq!(original.node_id(), restored.node_id());
    }

    /// First-ever launch: nothing saved yet.
    #[test]
    fn resolve_identity_generates_fresh_identity_when_none_saved() {
        let a = resolve_identity(None);
        let b = resolve_identity(None);
        assert_ne!(a.node_id(), b.node_id());
    }

    /// Corrupted/truncated storage (e.g. a partially-written Keychain/Keystore entry)
    /// must fail safely -- falling back to a fresh identity -- rather than panicking and
    /// leaving the app unable to start at all.
    #[test]
    fn resolve_identity_falls_back_safely_on_wrong_length_seed() {
        let too_short = vec![1, 2, 3];
        let identity = resolve_identity(Some(too_short));
        // Doesn't panic, and produces a usable identity.
        let _ = identity.node_id();

        let too_long = vec![0u8; 64];
        let identity = resolve_identity(Some(too_long));
        let _ = identity.node_id();

        let empty: Vec<u8> = Vec::new();
        let identity = resolve_identity(Some(empty));
        let _ = identity.node_id();
    }

    /// The end-to-end Milestone 2B.2a guarantee, exercised through the real
    /// `MeshClient` (not just `ContactStore` directly): a contact discovered in one
    /// `MeshClient` instance must still be there -- and still usable -- in a brand new
    /// `MeshClient` instance constructed afterward with the same identity seed and the
    /// same `contacts_db_path`. Dropping the first client and constructing a completely
    /// separate second one is the actual "kill the app, relaunch it" scenario, not just
    /// reusing the same in-memory object under a different name.
    #[test]
    fn contacts_persist_across_a_simulated_app_restart() {
        let dir = unique_test_dir("restart");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("contacts.json").to_str().unwrap().to_string();

        let seed = Identity::generate().seed().to_vec();
        let peer_identity = Identity::generate();
        let peer_node_id = peer_identity.node_id();

        {
            let client = MeshClient::new(MeshClientConfig {
                display_name: "Alice".to_string(),
                listen_addr: "127.0.0.1:0".to_string(),
                peer_addrs: vec![],
                channel_passphrase: "test-channel".to_string(),
                ttl: 8,
                identity_seed: Some(seed.clone()),
                contacts_db_path: Some(db_path.clone()),
                replay_store_path: None,
                delivery_store_path: None,
                forward_store_path: None,
                inbox_store_path: None,
                inbox_storage_key: None,
            })
            .unwrap();

            // Simulates what `update_contact_cache` would do for a freshly-discovered
            // peer -- inserted directly here (rather than via a real
            // `mesh_transport_udp::DiscoveredPeer`, which has a private field not
            // constructible outside that crate) and persisted the same way any real
            // mutation path does, via the shared `persist_contacts` helper.
            {
                let mut contacts = client.contacts.lock().unwrap();
                let record = ContactRecord::new(mesh_core::PublicIdentity::new(&peer_identity), Some("Bob".to_string()), now_millis());
                contacts.insert(peer_node_id, record);
                client.persist_contacts(&contacts);
            }
            assert_eq!(client.contacts().len(), 1, "contact should be visible before the simulated restart");
            // `client` (and its underlying socket) is dropped here at the end of this
            // block -- simulating the app process being killed.
        }

        // A brand new `MeshClient` -- as if the app just relaunched -- reusing the same
        // identity seed and, crucially, the same `contacts_db_path`.
        let restarted = MeshClient::new(MeshClientConfig {
            display_name: "Alice".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: Some(seed),
            contacts_db_path: Some(db_path),
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();

        let contacts = restarted.contacts();
        assert_eq!(contacts.len(), 1, "contact discovered before the simulated restart should still be there");
        assert_eq!(contacts[0].full_node_id, hex_encode(&peer_node_id));

        // Prerequisite for store-and-forward: this restarted client must be able to
        // encrypt a message to Bob using only the persisted identity -- Bob does not
        // need to be currently reachable/broadcasting for `send` to succeed at the
        // encryption step. `send` also requires a live transport round-trip to actually
        // deliver, which isn't set up in this test, but a `false` return here would mean
        // encryption itself was refused for lack of a known identity -- which is exactly
        // what persistence must prevent from happening after a restart. Since Bob's UDP
        // address isn't actually reachable in this test, this call may still fail at the
        // network layer, but must not fail because the identity was forgotten -- checked
        // indirectly above via `contacts()` already containing Bob's `PublicIdentity`.
        let _ = restarted.send(hex_encode(&peer_node_id), "hello from a fresh launch".to_string());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The single most important composed proof before moving on to physical-device
    /// testing (Milestone 2B.2b): persisted identity + persisted contact + real
    /// DirectV1 encryption while the recipient is offline + a restart of **both**
    /// sender and receiver + literally captured wire bytes, all in one deterministic
    /// test. If this doesn't hold, no phone test would either.
    ///
    /// Scenario:
    /// 1. Alice discovers Bob (his `PublicIdentity` is persisted to Alice's
    ///    `ContactStore`) -- Bob is never even started for this step.
    /// 2. Alice's `MeshClient` (and its socket) is dropped entirely -- "Alice is
    ///    killed". Bob remains offline throughout.
    /// 3. A brand new Alice `MeshClient` is constructed from the same identity seed and
    ///    the same `contacts_db_path` -- "Alice relaunches" -- and sends "hello Bob"
    ///    using *only* the persisted contact, with no fresh discovery involved.
    /// 4. The literal bytes Alice's real `UdpTransport` puts on the wire are captured by
    ///    a plain listening `UdpSocket` standing in for "the network" -- proving the
    ///    plaintext never appears in what's actually transmitted.
    /// 5. A brand new Bob `MeshClient` ("Bob relaunches") is constructed, and the
    ///    captured bytes are fed directly into its real `handle_incoming` pipeline
    ///    (simulating a relay delivering them late, the way a future store-and-forward
    ///    hop would) -- and it decrypts the original text correctly, using only the
    ///    sender identity embedded in the DirectV1 packet itself (Bob never needed
    ///    Alice as a pre-existing contact to do this -- see `direct_crypto`'s
    ///    self-contained-header design).
    #[test]
    fn persisted_offline_contact_survives_restart_and_decrypts_after_capture() {
        let base = unique_test_dir("e2e-restart-capture");
        std::fs::create_dir_all(&base).unwrap();
        let alice_contacts_path = base.join("alice-contacts.json").to_str().unwrap().to_string();

        let alice_seed = Identity::generate().seed().to_vec();
        let bob_identity = Identity::generate();
        let bob_public_identity = mesh_core::PublicIdentity::new(&bob_identity);
        let bob_node_id = bob_identity.node_id();

        // A plain UDP socket standing in for "the network" -- captures the literal
        // bytes Alice transmits, the same way a relay or a packet sniffer would.
        let capture_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        capture_socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let bob_address = capture_socket.local_addr().unwrap();

        // Step 1: Alice discovers Bob (identity only -- no live Bob process needed for
        // discovery itself) and it gets persisted.
        {
            let alice1 = MeshClient::new(MeshClientConfig {
                display_name: "Alice".to_string(),
                listen_addr: "127.0.0.1:0".to_string(),
                peer_addrs: vec![],
                channel_passphrase: "test-channel".to_string(),
                ttl: 8,
                identity_seed: Some(alice_seed.clone()),
                contacts_db_path: Some(alice_contacts_path.clone()),
                replay_store_path: None,
                delivery_store_path: None,
                forward_store_path: None,
                inbox_store_path: None,
                inbox_storage_key: None,
            })
            .unwrap();
            {
                let mut contacts = alice1.contacts.lock().unwrap();
                let record = ContactRecord::new(bob_public_identity.clone(), Some("Bob".to_string()), now_millis());
                contacts.insert(bob_node_id, record);
                alice1.persist_contacts(&contacts);
            }
            // `alice1` (and its socket) dropped here -- "Alice is killed". Bob was
            // never started in this phase at all -- "Bob is offline".
        }

        // Step 2: Alice relaunches -- same seed, same contacts_db_path -- and sends
        // using only the persisted contact, no rediscovery involved.
        let alice_node_id: mesh_core::NodeId;
        {
            let alice2 = MeshClient::new(MeshClientConfig {
                display_name: "Alice".to_string(),
                listen_addr: "127.0.0.1:0".to_string(),
                peer_addrs: vec![],
                channel_passphrase: "test-channel".to_string(),
                ttl: 8,
                identity_seed: Some(alice_seed.clone()),
                contacts_db_path: Some(alice_contacts_path.clone()),
                replay_store_path: None,
                delivery_store_path: None,
                forward_store_path: None,
                inbox_store_path: None,
                inbox_storage_key: None,
            })
            .unwrap();
            alice_node_id = hex_decode::<32>(&alice2.full_node_id()).unwrap();
            assert_eq!(alice2.contacts().len(), 1, "Bob's identity should have survived Alice's restart");

            alice2.add_peer(bob_address.to_string());
            assert_eq!(
                alice2.send(hex_encode(&bob_node_id), "hello Bob".to_string()),
                SendOutcome::Sent,
                "sending to a known-but-currently-offline contact must still succeed at the encryption/send step"
            );
            // `alice2` dropped here too, once the send above has gone out.
        }

        // Step 3: capture whatever Alice actually transmitted, and confirm the
        // plaintext never appears in it.
        let mut buf = vec![0u8; 64 * 1024];
        let (len, _from) = capture_socket
            .recv_from(&mut buf)
            .expect("expected to capture the packet Alice sent to Bob's address");
        let captured = buf[..len].to_vec();
        let haystack = String::from_utf8_lossy(&captured);
        assert!(!haystack.contains("hello Bob"), "plaintext must never appear in the transmitted bytes");
        drop(capture_socket);

        // Step 4: Bob relaunches (a brand new `MeshClient` sharing nothing with the
        // above except the same identity seed) and the captured bytes are delivered
        // to it directly -- simulating a relay/store-and-forward hop handing them over
        // late, exactly the scenario a future DTN design must support.
        let bob2 = MeshClient::new(MeshClientConfig {
            display_name: "Bob".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: Some(bob_identity.seed().to_vec()),
            contacts_db_path: None,
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();

        let event = bob2
            .runtime
            .block_on(bob2.node.handle_incoming(captured))
            .expect("handle_incoming must not error on a legitimately captured packet")
            .expect("a legitimate DirectV1 message must produce an IncomingEvent");

        match event {
            mesh_core::IncomingEvent::Content(delivered) => {
                assert_eq!(delivered.sender, alice_node_id);
                assert!(matches!(delivered.content, mesh_core::ReceivedContent::Text(ref text) if text == "hello Bob"));
            }
            _ => panic!("expected a decrypted text message from Alice"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Milestone 2D, exercised through the real `MeshClient` (not just `mesh-core`'s
    /// lower-level `MeshNode` tests): a captured DirectV1 packet, replayed after Bob's
    /// `MeshClient` restarts with the same `replay_store_path`, must be rejected -- and
    /// a genuinely new message afterward must still go through. This is the
    /// mobile-stack equivalent of `mesh-core`'s
    /// `replayed_message_is_rejected_after_restart`; Milestone 2B's physical-device
    /// report's Test F is this same scenario, one layer further out, on a real device.
    #[test]
    fn replayed_packet_is_rejected_after_a_mesh_client_restart() {
        let base = unique_test_dir("replay-restart");
        std::fs::create_dir_all(&base).unwrap();
        let bob_replay_path = base.join("bob-replay.sqlite").to_str().unwrap().to_string();

        let alice_seed = Identity::generate().seed().to_vec();
        let bob_identity = Identity::generate();
        let bob_public_identity = mesh_core::PublicIdentity::new(&bob_identity);
        let bob_node_id = bob_identity.node_id();
        let bob_seed = bob_identity.seed().to_vec();

        // A raw capture socket standing in for "the network", the same technique the
        // restart+capture test above uses, so we get our hands on the literal bytes
        // Alice transmits.
        let capture_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        capture_socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let bob_address = capture_socket.local_addr().unwrap();

        let alice = MeshClient::new(MeshClientConfig {
            display_name: "Alice".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: Some(alice_seed),
            contacts_db_path: None,
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();
        {
            let mut contacts = alice.contacts.lock().unwrap();
            let record = ContactRecord::new(bob_public_identity, Some("Bob".to_string()), now_millis());
            contacts.insert(bob_node_id, record);
        }
        alice.add_peer(bob_address.to_string());
        assert_eq!(alice.send(hex_encode(&bob_node_id), "hello Bob".to_string()), SendOutcome::Sent);

        let mut buf = vec![0u8; 64 * 1024];
        let (len, _from) = capture_socket.recv_from(&mut buf).expect("expected to capture Alice's transmitted packet");
        let captured = buf[..len].to_vec();
        drop(capture_socket);

        // Bob (first launch): the captured packet is delivered once and accepted.
        {
            let bob1 = MeshClient::new(MeshClientConfig {
                display_name: "Bob".to_string(),
                listen_addr: "127.0.0.1:0".to_string(),
                peer_addrs: vec![],
                channel_passphrase: "test-channel".to_string(),
                ttl: 8,
                identity_seed: Some(bob_seed.clone()),
                contacts_db_path: None,
                replay_store_path: Some(bob_replay_path.clone()),
                delivery_store_path: None,
                forward_store_path: None,
                inbox_store_path: None,
                inbox_storage_key: None,
            })
            .unwrap();
            let event = bob1.runtime.block_on(bob1.node.handle_incoming(captured.clone())).unwrap();
            match &event {
                Some(mesh_core::IncomingEvent::Content(delivered)) => {
                    assert!(matches!(delivered.content, mesh_core::ReceivedContent::Text(_)));
                }
                _ => panic!("expected a decrypted text message from Alice"),
            };
            // Milestone 3C: `bob1.node.handle_incoming` above already durably
            // persisted and acked this message automatically -- no app-side callback
            // needed (see `MeshNode::inbox_messages`/`inbox_store.rs`'s doc).
            // `bob1` dropped here -- simulating the app being killed.
        }

        // Bob restarts: same identity, same `replay_store_path`.
        let bob2 = MeshClient::new(MeshClientConfig {
            display_name: "Bob".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: Some(bob_seed),
            contacts_db_path: None,
            replay_store_path: Some(bob_replay_path),
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();

        // The exact same captured bytes, replayed after the restart, must be rejected.
        let replay_event = bob2.runtime.block_on(bob2.node.handle_incoming(captured)).unwrap();
        assert!(replay_event.is_none(), "a replayed packet must be rejected even after MeshClient restarts, given the same replay_store_path");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Milestone 3C: `send` must never silently downgrade a message that's too long for
    /// the reliable path to a weaker, best-effort delivery guarantee -- it must report
    /// `TooLongForReliableText` explicitly, and nothing may be transmitted at all.
    #[test]
    fn send_reports_too_long_explicitly_instead_of_silently_falling_back() {
        let alice = MeshClient::new(MeshClientConfig {
            display_name: "Alice".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: None,
            contacts_db_path: None,
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();
        let bob_identity = Identity::generate();
        {
            let mut contacts = alice.contacts.lock().unwrap();
            let record = ContactRecord::new(mesh_core::PublicIdentity::new(&bob_identity), Some("Bob".to_string()), now_millis());
            contacts.insert(bob_identity.node_id(), record);
        }

        let too_long = "x".repeat(mesh_core::CHUNK_SIZE + 1);
        assert_eq!(alice.send(hex_encode(&bob_identity.node_id()), too_long), SendOutcome::TooLongForReliableText);

        // A short message to the same contact still works normally.
        assert_eq!(alice.send(hex_encode(&bob_identity.node_id()), "short".to_string()), SendOutcome::Sent);
    }

    /// Milestone 3C: `chatHistory()` must survive a `MeshClient` restart -- the durable
    /// inbox, not any in-memory list, is the actual source of truth for received chat
    /// content.
    #[test]
    fn chat_history_survives_a_mesh_client_restart() {
        let base = unique_test_dir("chat-history-restart");
        std::fs::create_dir_all(&base).unwrap();
        let bob_inbox_path = base.join("bob-inbox.sqlite").to_str().unwrap().to_string();
        let bob_seed = Identity::generate().seed().to_vec();
        let bob_inbox_storage_key = vec![0x11u8; 32];

        let alice = MeshClient::new(MeshClientConfig {
            display_name: "Alice".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: None,
            contacts_db_path: None,
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: None,
            inbox_storage_key: None,
        })
        .unwrap();

        {
            let bob = MeshClient::new(MeshClientConfig {
                display_name: "Bob".to_string(),
                listen_addr: "127.0.0.1:0".to_string(),
                peer_addrs: vec![],
                channel_passphrase: "test-channel".to_string(),
                ttl: 8,
                identity_seed: Some(bob_seed.clone()),
                contacts_db_path: None,
                replay_store_path: None,
                delivery_store_path: None,
                forward_store_path: None,
                inbox_store_path: Some(bob_inbox_path.clone()),
                inbox_storage_key: Some(bob_inbox_storage_key.clone()),
            })
            .unwrap();
            {
                let mut contacts = alice.contacts.lock().unwrap();
                let record = ContactRecord::new(mesh_core::PublicIdentity::new(&Identity::from_seed(bob_seed.clone().try_into().unwrap())), Some("Bob".to_string()), now_millis());
                contacts.insert(hex_decode::<32>(&bob.full_node_id()).unwrap(), record);
            }
            alice.add_peer(bob.node.transport().local_addr().unwrap().to_string());
            assert_eq!(alice.send(hex_encode(&hex_decode::<32>(&bob.full_node_id()).unwrap()), "hello Bob".to_string()), SendOutcome::Sent);

            // `MeshClient::new` already spawns its own background loop driving
            // `recv_raw`/`handle_incoming` -- poll `chat_history()` instead of racing
            // it with a second, manual `recv_raw` call for the same packet.
            let mut history = Vec::new();
            for _ in 0..100 {
                history = bob.chat_history();
                if !history.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(history.len(), 1, "bob should have received alice's message");
            // `bob` dropped here -- simulating the app being killed.
        }

        let bob_restarted = MeshClient::new(MeshClientConfig {
            display_name: "Bob".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            peer_addrs: vec![],
            channel_passphrase: "test-channel".to_string(),
            ttl: 8,
            identity_seed: Some(bob_seed),
            contacts_db_path: None,
            replay_store_path: None,
            delivery_store_path: None,
            forward_store_path: None,
            inbox_store_path: Some(bob_inbox_path),
            inbox_storage_key: Some(bob_inbox_storage_key),
        })
        .unwrap();

        let history = bob_restarted.chat_history();
        assert_eq!(history.len(), 1, "chat history must survive a restart -- it lives in the durable inbox, not an in-memory list");
        assert_eq!(history[0].text.as_deref(), Some("hello Bob"));

        let _ = std::fs::remove_dir_all(&base);
    }
}

