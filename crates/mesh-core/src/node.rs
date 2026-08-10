//! The mesh node: ties identity, channel encryption, de-duplication and a pluggable
//! transport together into flood routing with a TTL hop budget. This is the direct
//! implementation of "chain relay" networking: a message from node A reaches node J ten
//! hops away because every node in between decrements the TTL and re-broadcasts it to its
//! own directly-reachable peers.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;

use crate::crypto::ChannelKey;
use crate::call::{CallFrame, CallMessage, CallSignal, MediaKind};
use crate::direct_crypto::{decrypt_direct_message, encrypt_direct_message, DirectCryptoError, DirectCryptoHeaderV1, DirectEnvelopeBody, DirectMessageAadV1};
use crate::identity::{short_id, verify, Identity, NodeId};
use crate::message::{EncryptionMode, Envelope, MessageType, PROTOCOL_VERSION};
use crate::payload::{split_into_chunks, Chunk, ContentKind, DeliveryAck, ReceivedContent, TransferProgress, WirePayload, CHUNK_SIZE};
use crate::reassembly::{Accepted, Reassembler};
use crate::delivery_store::{DeliveryStore, OutboundState};
use crate::flood_guard::FloodGuard;
use crate::forward_store::{ForwardState, ForwardStore};
use crate::inbox_store::{InboxMessage, InboxStore, InsertOutcome};
use crate::replay_store::ReplayStore;
use crate::session::{PublicIdentity, Session};
use crate::transport::Transport;

/// How long a message stays valid before relays should stop forwarding it, if it hasn't
/// reached its recipient by then -- see `Envelope::expires_at`. Generous for a live-flood
/// LAN mesh; a future store-and-forward milestone will let a message wait far longer than
/// this by holding it on an intermediate node instead of only ever flooding live.
const DEFAULT_MESSAGE_LIFETIME: Duration = Duration::from_secs(5 * 60);

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Short hex prefix of a `message_id` (or similar opaque byte id) for log lines --
/// message ids are random, non-secret values (not keys, not plaintext), so logging a
/// short prefix of one is safe and just gives enough to correlate related log lines for
/// the same message.
fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().take(6).map(|b| format!("{:02x}", b)).collect()
}

/// How many chunks to send back-to-back before pausing briefly. Sending every chunk of a
/// multi-MB photo/video (thousands of 1KB chunks) in one uninterrupted burst can overrun
/// the receiver's socket buffer and the network's own buffering, causing silent drops --
/// and since reassembly needs *every* chunk to complete (no retransmission), losing even
/// one means the whole attachment never arrives. But pacing *every single* chunk (as
/// opposed to every burst) makes large transfers needlessly slow. Sending in small
/// bursts with a short pause between them is a middle ground: real-world testing with a
/// 3MB attachment (2930 chunks) was reliable at burst=16/pause=8ms, several times faster
/// than pacing every chunk individually while still avoiding the packet loss that not
/// pacing at all reproduced.
const CHUNK_BURST_SIZE: usize = 16;
const CHUNK_BURST_PAUSE: Duration = Duration::from_millis(8);

/// Hard cap on how large a single raw incoming packet is allowed to be before we even
/// attempt to deserialize it. `bincode`'s default deserialization trusts length prefixes
/// (e.g. a `Vec<u8>`'s byte count) without bounds-checking them against how much data
/// actually followed -- a malicious or corrupted packet could claim an enormous length
/// and make deserialization try to allocate far more memory than the packet itself ever
/// contained. Real envelopes are small (a chunk's ciphertext is bounded by
/// `payload::CHUNK_SIZE` plus a modest fixed overhead), so anything anywhere near this
/// size is already not a legitimate envelope -- reject it before deserializing at all.
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// Who a payload is being sent to, and which crypto scheme to use for it -- see
/// `MeshNode::send_one_payload`. Deliberately explicit (rather than inferring the scheme
/// from whether a recipient is present) so `recipient: Some(..)` never silently implies
/// a particular crypto behavior -- see `EncryptionMode`'s doc.
enum Destination {
    /// Everyone on the channel (`recipient: None`) -- uses the shared `ChannelKey`
    /// (`EncryptionMode::ChannelV1`). Used by `broadcast_text` (the `mesh-cli` demo).
    Broadcast,
    /// A specific node, still using the shared `ChannelKey`
    /// (`EncryptionMode::ChannelV1`) -- used only for call signaling/frames (see
    /// `send_call`). Switching live call media to per-session keys is a deliberately
    /// separate, not-yet-undertaken follow-up.
    ChannelDirect(NodeId),
    /// A specific node, using "MeshTalk Direct Encryption v1" (see `direct_crypto.rs`,
    /// `EncryptionMode::DirectV1`) -- used for direct chat messages (`send_text`/
    /// `send_file`).
    DirectV1(PublicIdentity),
}

impl Destination {
    fn recipient_node_id(&self) -> Option<NodeId> {
        match self {
            Destination::Broadcast => None,
            Destination::ChannelDirect(target) => Some(*target),
            Destination::DirectV1(identity) => Some(identity.node_id),
        }
    }
}

/// Milestone 3C: everything the caller needs about one fully-reassembled piece of
/// content that has *already been durably persisted* to `MeshNode::inbox_messages` and
/// acknowledged (if applicable) by the time this event is returned -- there is nothing
/// left for the application layer to do to make this content durable; it already is.
pub struct DeliveredContent {
    pub sender: NodeId,
    /// The sender's verified `PublicIdentity` -- `Some` for a `DirectV1` message (the
    /// common case for chat), `None` for a `ChannelV1` broadcast (which has no
    /// per-sender key binding to verify).
    pub sender_identity: Option<PublicIdentity>,
    /// The envelope-level `message_id` of the chunk that completed this transfer. For a
    /// single-chunk message (every reliable text message -- see
    /// `MeshNode::send_reliable_text`) this is the one and only envelope's id. For a
    /// multi-chunk transfer (a file attachment) this is only the *last* chunk's id --
    /// multi-chunk transfers are not covered by the outbound ACK/retry protocol in
    /// `delivery_store.rs` (though they are still durably recorded in `inbox_store.rs`
    /// and acknowledged on receipt).
    pub message_id: [u8; 16],
    pub content: ReceivedContent,
}

/// One received chunk's worth of news: either its transfer is still incomplete (with a
/// progress snapshot to show the user), or it was the last chunk needed and the full
/// content is ready. Or, a call-related message/frame addressed to us.
pub enum IncomingEvent {
    Progress(NodeId, TransferProgress),
    Content(DeliveredContent),
    /// A call signaling message or media frame from `NodeId`, addressed to us -- see
    /// `call.rs`. Messages addressed to *other* nodes are relayed onward (like anything
    /// else) but never surface as an `IncomingEvent`.
    Call(NodeId, CallMessage),
}

pub struct MeshNode<T: Transport> {
    identity: Identity,
    channel_key: ChannelKey,
    /// Durable `(sender, message_id)` record of messages this node has *successfully*
    /// finished handling -- either fully decrypted-and-accepted (if we're the final
    /// recipient) or successfully forwarded (if we're relaying) -- see `replay_store.rs`
    /// and this file's Milestone 2D.1 doc section in `handle_incoming`. Never written to
    /// merely because a signature verified; a packet that was only authenticated but not
    /// yet successfully processed must remain retryable, including across a restart.
    replay_store: ReplayStore,
    /// Fast, in-memory-only, non-durable loop/duplicate suppression -- see
    /// `flood_guard.rs`. Used for the entire high-rate, ephemeral `CallFrame` path
    /// (never durable -- a call doesn't survive a restart anyway, and durable storage
    /// per audio/video frame would be a needless disk-I/O hot path).
    flood_guard: Mutex<FloodGuard>,
    /// Milestone 3A: durable outgoing delivery/retry state for reliable DirectV1 text
    /// messages -- see `delivery_store.rs` and `send_reliable_text`'s doc. A completely
    /// separate concern from `replay_store` ("have I authenticated this incoming packet
    /// before") -- this one tracks "has *my own* outgoing message actually been durably
    /// accepted yet, and when should I retry it if not".
    delivery: DeliveryStore,
    /// Milestone 3B: durable per-neighbor forwarding state for messages this node is
    /// only relaying -- see `forward_store.rs`. Distinct from `delivery` (this node's
    /// *own* originated messages) and from `replay_store` (a single collapsed
    /// seen/not-seen boolean per message) -- this tracks, independently per peer,
    /// whether *that specific neighbor* has actually received a relayed message yet.
    forward: ForwardStore,
    /// Milestone 3C: durable inbound message store -- the actual authority for "has
    /// the final recipient durably accepted this specific `Chat` message", replacing
    /// the app-callback-based contract `acknowledge_content` used to rely on. See
    /// `inbox_store.rs`'s module doc for exactly why that distinction matters.
    inbox: InboxStore,
    reassembler: Mutex<Reassembler>,
    transport: T,
    /// Sender-authenticated hop budget applied to every message this node originates --
    /// see `Envelope::max_hops`.
    default_max_hops: u8,
}

impl<T: Transport> MeshNode<T> {
    /// Replay protection is in-memory only (does not survive a restart) with this
    /// constructor -- see `Self::new_with_replay_store` for a persistent one. Fine for
    /// anything that doesn't need restart-surviving replay protection (tests,
    /// `mesh-cli`'s short-lived demo runs).
    pub fn new(identity: Identity, channel_key: ChannelKey, transport: T, default_max_hops: u8) -> Self {
        Self::new_with_replay_store(identity, channel_key, transport, default_max_hops, ReplayStore::in_memory())
    }

    /// Like `Self::new`, but with an explicit `ReplayStore` -- pass `ReplayStore::open(path)`
    /// (Milestone 2D) so a previously-seen `(sender, message_id)` is still remembered
    /// after this node restarts, closing the replay hole a purely in-memory seen-cache
    /// left open. Works identically for a node acting purely as a relay (never the
    /// final recipient of anything) as it does for one receiving its own messages --
    /// `handle_incoming` runs this check before branching on either role.
    pub fn new_with_replay_store(identity: Identity, channel_key: ChannelKey, transport: T, default_max_hops: u8, replay_store: ReplayStore) -> Self {
        Self::new_with_stores(
            identity,
            channel_key,
            transport,
            default_max_hops,
            replay_store,
            DeliveryStore::in_memory(),
            ForwardStore::in_memory(),
            InboxStore::in_memory(),
        )
    }

    /// Like `Self::new_with_replay_store`, but also takes explicit `DeliveryStore`,
    /// `ForwardStore` and `InboxStore` instances -- pass `DeliveryStore::open(path)`
    /// (Milestone 3A) so not-yet-acknowledged outgoing messages (see
    /// `send_reliable_text`) are still retried after this node restarts,
    /// `ForwardStore::open(path)` (Milestone 3B) so not-yet-fully-forwarded relayed
    /// messages keep their per-neighbor retry state across a restart too, and
    /// `InboxStore::open(path)` (Milestone 3C) so durably-accepted chat history
    /// actually survives a restart instead of only ever living in the caller's memory.
    pub fn new_with_stores(
        identity: Identity,
        channel_key: ChannelKey,
        transport: T,
        default_max_hops: u8,
        replay_store: ReplayStore,
        delivery_store: DeliveryStore,
        forward_store: ForwardStore,
        inbox_store: InboxStore,
    ) -> Self {
        Self {
            identity,
            channel_key,
            replay_store,
            flood_guard: Mutex::new(FloodGuard::new()),
            delivery: delivery_store,
            forward: forward_store,
            inbox: inbox_store,
            reassembler: Mutex::new(Reassembler::new()),
            transport,
            default_max_hops,
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    /// This node's own public identity -- share this with another node (e.g. so it can
    /// call `send_text`/`send_file` addressed to this node) the same way `node_id()` is
    /// already shared today.
    pub fn public_identity(&self) -> PublicIdentity {
        PublicIdentity::new(&self.identity)
    }

    /// Access to the underlying transport, e.g. so callers can add newly-discovered
    /// peers at runtime (`Transport` impls that support it, like `UdpTransport`, expose
    /// their own methods for this -- `MeshNode` itself stays transport-agnostic).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Block until the next raw packet arrives on the underlying transport.
    pub async fn recv_raw(&self) -> anyhow::Result<Vec<u8>> {
        self.transport.recv().await
    }

    /// Encrypt, sign and flood a plain-text message to `recipient` -- a direct message
    /// to one specific conversation partner (like a real chat app), not a broadcast to
    /// everyone on the channel. Uses "MeshTalk Direct Encryption v1" (see
    /// `direct_crypto.rs`): the message is encrypted with a session key derived from
    /// this node's and `recipient`'s X25519 identities, not the shared channel
    /// passphrase -- other nodes still relay the envelope onward (so it can reach
    /// `recipient` multiple hops away) but cannot decrypt it. Fails closed (returns
    /// `direct_crypto::DirectCryptoError::CannotEncryptForRecipient`, never sends
    /// plaintext or falls back to the shared channel key) if `recipient`'s X25519
    /// binding doesn't verify or the resulting shared secret is non-contributory.
    pub async fn send_text(&self, recipient: &PublicIdentity, text: &str) -> anyhow::Result<()> {
        let chunks = split_into_chunks(ContentKind::Text, None, None, text.as_bytes());
        self.broadcast_chunks(Destination::DirectV1(recipient.clone()), chunks, |_| {}).await
    }

    /// Milestone 3A: like `send_text`, but with durable, restart-surviving
    /// retry-until-acknowledged delivery -- see `delivery_store.rs`'s module doc. Only
    /// covers messages short enough to fit in a single chunk (`payload::CHUNK_SIZE`,
    /// currently 1024 bytes) -- comfortably enough for ordinary chat text. Returns an
    /// error without sending anything for a longer message; use `send_text` instead for
    /// that (best-effort, no ack/retry).
    ///
    /// The returned `Ok(())` means "durably queued and a first delivery attempt was
    /// made," not "the recipient has it" -- call `retry_due_deliveries` periodically
    /// (e.g. every 1-2 seconds) to keep retrying until an ack arrives or the message
    /// expires.
    pub async fn send_reliable_text(&self, recipient: &PublicIdentity, text: &str) -> anyhow::Result<()> {
        let mut chunks = split_into_chunks(ContentKind::Text, None, None, text.as_bytes());
        if chunks.len() != 1 {
            anyhow::bail!(
                "message too long for reliable delivery ({} chunks needed, max 1 -- i.e. up to {} bytes of UTF-8 text); use send_text for a best-effort send instead",
                chunks.len(),
                CHUNK_SIZE
            );
        }
        let chunk = chunks.pop().expect("checked above that exactly one chunk exists");

        let envelope = self.build_envelope(&Destination::DirectV1(recipient.clone()), MessageType::Chat, &WirePayload::Chunk(chunk))?;
        let message_id = envelope.message_id;
        let sender = envelope.sender;
        let created_at = envelope.created_at;
        let expires_at = envelope.expires_at;
        let envelope_bytes = bincode::serialize(&envelope)?;

        // Mark our own message as seen -- both the fast in-memory `FloodGuard` and
        // durably in `ReplayStore`, immediately, unlike a message we *receive* from
        // someone else (see `handle_incoming`'s Milestone 3C doc section for why that
        // case defers marking until `self.inbox` durably persists it). That deferral is
        // specifically about not prematurely trusting "authenticated" (or even
        // "decrypted") as "durably accepted" for someone else's content -- it doesn't
        // apply to a message we ourselves just originated, which has no such question
        // to defer: if this exact envelope loops back to us via the mesh (a real,
        // common occurrence, since flooding doesn't check whether a relay happens to be
        // the original sender), it must be recognized immediately as already handled,
        // or we would re-flood our own message indefinitely.
        self.flood_guard.lock().unwrap().check_and_insert(&sender, &message_id);
        self.replay_store.mark_seen(&sender, &message_id, created_at, expires_at);

        // Persisted BEFORE the first transmission attempt -- see `delivery_store.rs`'s
        // doc: a crash between here and the first `flood_bytes` call below still leaves
        // this message queued and retryable after a restart.
        self.delivery.enqueue(&message_id, &recipient.node_id, &envelope_bytes, created_at, expires_at);

        self.attempt_delivery(&message_id, &envelope_bytes).await;
        Ok(())
    }

    /// Makes (or retries) one delivery attempt for an already-persisted outbound
    /// message: transmits the exact same bytes, then records the attempt (transitions
    /// `Queued` -> `Sent`, schedules the next backoff, or is a harmless no-op if the
    /// message has already reached a terminal state -- see `delivery_store.rs`).
    async fn attempt_delivery(&self, message_id: &[u8; 16], envelope_bytes: &[u8]) {
        if let Err(err) = self.flood_bytes(*message_id, envelope_bytes.to_vec()).await {
            log::warn!("mesh-core: reliable-delivery attempt failed for message_id={} -- {} (will retry)", hex_prefix(message_id), err);
        }
        self.delivery.record_attempt(message_id, now_millis());
    }

    /// Call periodically (e.g. every 1-2 seconds, from a UI-driven timer -- see
    /// `mesh-mobile`) to retry every reliable message that's still waiting for an ack
    /// and whose backoff delay has elapsed. Also purges anything that's passed its
    /// expiry into the terminal `Expired` state first. Returns how many messages a
    /// (re)transmission was just attempted for.
    pub async fn retry_due_deliveries(&self) -> usize {
        let now = now_millis();
        self.delivery.expire_overdue(now);
        let due = self.delivery.due_for_attempt(now);
        let count = due.len();
        for message in due {
            log::debug!("mesh-core: retrying delivery of message_id={} (attempt {})", hex_prefix(&message.message_id), message.attempts + 1);
            self.attempt_delivery(&message.message_id, &message.envelope_bytes).await;
        }
        count
    }

    /// Milestone 3B: forwards `relayed` (this node's own hop-incremented copy of the
    /// original envelope) to every peer known right now, tracking each peer's outcome
    /// independently in `forward` (see `forward_store.rs`) rather than collapsing the
    /// whole fan-out into a single success/failure boolean. Returns `true` once every
    /// peer tracked for `message_id` has resolved (forwarded, or gave up after
    /// expiring) -- the caller uses this to decide whether it's finally safe to
    /// durably mark the message `seen` in `ReplayStore`.
    async fn relay_forward(&self, sender: &NodeId, message_id: [u8; 16], expires_at: u64, relayed: &Envelope) -> bool {
        let peers = self.transport.peers();
        if peers.is_empty() {
            return true; // vacuously nothing to forward -- matches pre-Milestone-3B behavior
        }
        let Ok(bytes) = bincode::serialize(relayed) else { return false };
        let now = now_millis();
        self.forward.enqueue_pending(&message_id, sender, &peers, &bytes, now, expires_at);

        for peer in &peers {
            // A peer already resolved (from an earlier attempt at this same message_id,
            // e.g. a resend within this process's uptime, or a prior background retry)
            // needs no further attempt here.
            if !matches!(self.forward.state_of(&message_id, peer), Some(ForwardState::Pending)) {
                continue;
            }
            let result = self.transport.send_to_peer(peer, bytes.clone()).await;
            let succeeded = result.is_ok();
            if let Err(err) = result {
                log::warn!("mesh-core: failed to forward message_id={} to a peer -- {} (will remain retryable)", hex_prefix(&message_id), err);
            }
            self.forward.record_attempt_result(&message_id, peer, succeeded, now_millis());
        }

        self.forward.all_peers_resolved(&message_id)
    }

    /// Call periodically (e.g. every 1-2 seconds, alongside `retry_due_deliveries`) to
    /// retry forwarding to every neighbor this relay hasn't yet successfully forwarded
    /// a relayed message to -- see `forward_store.rs`'s module doc for why this is a
    /// separate, per-neighbor concern from `retry_due_deliveries` (this node's own
    /// originated messages). Also purges anything past its expiry into the terminal
    /// `Expired` state first, and durably marks any message that becomes fully
    /// forwarded as a result of these retries `seen` in `ReplayStore`. Returns how many
    /// per-neighbor forwarding attempts were just made.
    pub async fn retry_pending_forwards(&self) -> usize {
        let now = now_millis();
        self.forward.expire_overdue(now);
        let due = self.forward.due_for_attempt(now);
        let count = due.len();
        let mut touched_message_ids: Vec<[u8; 16]> = Vec::new();
        for item in due {
            let result = self.transport.send_to_peer(&item.peer, item.envelope_bytes.clone()).await;
            let succeeded = result.is_ok();
            if let Err(err) = result {
                log::warn!(
                    "mesh-core: retrying forward of message_id={} to a peer failed -- {} (attempt {})",
                    hex_prefix(&item.message_id),
                    err,
                    item.attempts + 1
                );
            }
            self.forward.record_attempt_result(&item.message_id, &item.peer, succeeded, now_millis());
            if !touched_message_ids.contains(&item.message_id) {
                touched_message_ids.push(item.message_id);
            }
        }
        for message_id in touched_message_ids {
            if self.forward.all_peers_resolved(&message_id) {
                if let Some((sender, expires_at)) = self.forward.sender_and_expiry_of(&message_id) {
                    self.replay_store.mark_seen(&sender, &message_id, now_millis(), expires_at);
                }
            }
        }
        count
    }

    /// Milestone 3C: every durably-accepted inbound `Chat` message, oldest first -- for
    /// an application to hydrate its own chat-history UI from disk on launch instead of
    /// starting from an empty in-memory list (see `inbox_store.rs`'s module doc). This
    /// is now the actual source of truth for received chat content -- there is no
    /// longer an app-facing "acknowledge once you've saved it yourself" API, because
    /// there is nothing left for the app to durably save that this crate doesn't
    /// already durably have by the time `handle_incoming` returns a `Content` event.
    ///
    /// Loads the entire inbox -- fine for today's message volumes, but prefer
    /// `inbox_messages_for_peer` for a single conversation once history grows large
    /// (Milestone 3C.1).
    pub fn inbox_messages(&self) -> Vec<InboxMessage> {
        self.inbox.all_messages()
    }

    /// Milestone 3C.1: a single conversation's durably-accepted messages, newest
    /// first, bounded to `limit` -- see `InboxStore::messages_for_peer`'s doc for the
    /// pagination cursor convention (pass the previous page's oldest `received_at` as
    /// `before_received_at` to fetch the next page).
    pub fn inbox_messages_for_peer(&self, peer: &NodeId, before_received_at: Option<u64>, limit: usize) -> Vec<InboxMessage> {
        self.inbox.messages_for_peer(peer, before_received_at, limit)
    }

    /// Sends an authenticated `DeliveryAckV1` receipt to `recipient` for `message_id` --
    /// the wire-level half of what Milestone 3A's `acknowledge_content` used to do.
    /// Milestone 3C: this alone is no longer sufficient reason to durably mark anything
    /// -- see `handle_incoming`'s `WirePayload::Chunk`/`Accepted::Complete` handling,
    /// which only ever calls this *after* `self.inbox.insert_if_absent` has already
    /// durably committed the content (or found it already durably present).
    async fn send_delivery_ack(&self, recipient: &PublicIdentity, message_id: [u8; 16]) -> anyhow::Result<()> {
        let payload = WirePayload::DeliveryAck(DeliveryAck { acked_message_id: message_id });
        self.send_one_payload(&Destination::DirectV1(recipient.clone()), MessageType::DeliveryAck, &payload).await
    }

    /// Broadcasts a plain-text message to everyone on the channel (not addressed to any
    /// one conversation partner) -- used by the `mesh-cli` demo, which doesn't do
    /// per-contact chat threads. Uses the shared channel key (`EncryptionMode::ChannelV1`),
    /// same as before -- broadcast is deliberately kept on a separate crypto path from
    /// direct messages (see `EncryptionMode`'s doc). Mobile apps use `send_text` instead.
    pub async fn broadcast_text(&self, text: &str) -> anyhow::Result<()> {
        let chunks = split_into_chunks(ContentKind::Text, None, None, text.as_bytes());
        self.broadcast_chunks(Destination::Broadcast, chunks, |_| {}).await
    }

    /// Encrypt, sign and flood a file attachment (image, video, voice note, or generic
    /// file) to `recipient`, using "MeshTalk Direct Encryption v1" the same way as
    /// `send_text` (see its doc for the fail-closed behavior). Splits the data into
    /// chunks that flow through the same relay path as any other message -- each chunk
    /// is its own independently-encrypted envelope, with its own fresh AEAD nonce.
    /// `on_progress` is called after every chunk is handed to the transport (not just at
    /// the end) so the caller can show a live send progress bar for larger attachments
    /// instead of the call appearing to hang until it's done.
    pub async fn send_file(
        &self,
        recipient: &PublicIdentity,
        kind: ContentKind,
        file_name: String,
        mime_type: String,
        data: &[u8],
        on_progress: impl FnMut(TransferProgress),
    ) -> anyhow::Result<()> {
        let chunks = split_into_chunks(kind, Some(file_name), Some(mime_type), data);
        self.broadcast_chunks(Destination::DirectV1(recipient.clone()), chunks, on_progress).await
    }

    async fn broadcast_chunks(
        &self,
        destination: Destination,
        chunks: Vec<Chunk>,
        mut on_progress: impl FnMut(TransferProgress),
    ) -> anyhow::Result<()> {
        let total_chunks = chunks.len() as u32;
        let transfer_id = chunks.first().map(|c| c.transfer_id).unwrap_or([0u8; 16]);
        let kind = chunks.first().map(|c| c.kind).unwrap_or(ContentKind::Text);
        let last_index = chunks.len().saturating_sub(1);

        for (i, chunk) in chunks.into_iter().enumerate() {
            self.send_one_payload(&destination, MessageType::Chat, &WirePayload::Chunk(chunk)).await?;
            on_progress(TransferProgress {
                transfer_id,
                kind,
                done_chunks: (i + 1) as u32,
                total_chunks,
            });
            if i != last_index && (i + 1) % CHUNK_BURST_SIZE == 0 {
                tokio::time::sleep(CHUNK_BURST_PAUSE).await;
            }
        }
        Ok(())
    }

    /// Sends a call invite to `target` (a specific node, not the whole channel) -- "I'm
    /// calling you." `call_id` should be freshly generated per call attempt (see
    /// `call::random_call_id`) so replies/frames can be matched to it.
    pub async fn call_invite(&self, target: NodeId, call_id: [u8; 16], video: bool) -> anyhow::Result<()> {
        self.send_call(target, CallMessage::Signal(CallSignal::Invite { call_id, video })).await
    }

    /// Accepts an incoming call -- "I'm picking up."
    pub async fn call_accept(&self, target: NodeId, call_id: [u8; 16]) -> anyhow::Result<()> {
        self.send_call(target, CallMessage::Signal(CallSignal::Accept { call_id })).await
    }

    /// Declines an incoming call.
    pub async fn call_reject(&self, target: NodeId, call_id: [u8; 16]) -> anyhow::Result<()> {
        self.send_call(target, CallMessage::Signal(CallSignal::Reject { call_id })).await
    }

    /// Ends a call in progress (or cancels one that hasn't been answered yet).
    pub async fn call_end(&self, target: NodeId, call_id: [u8; 16]) -> anyhow::Result<()> {
        self.send_call(target, CallMessage::Signal(CallSignal::End { call_id })).await
    }

    /// Sends one frame of live audio or video to `target`. No chunking, no pacing delay,
    /// no reassembly on the other end -- a live call needs every frame delivered (or
    /// dropped) immediately, not buffered until some larger transfer completes.
    pub async fn send_call_frame(
        &self,
        target: NodeId,
        call_id: [u8; 16],
        media: MediaKind,
        sequence: u32,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.send_call(target, CallMessage::Frame(CallFrame { call_id, media, sequence, data })).await
    }

    async fn send_call(&self, target: NodeId, message: CallMessage) -> anyhow::Result<()> {
        let message_type = match message {
            CallMessage::Signal(_) => MessageType::CallSignal,
            CallMessage::Frame(_) => MessageType::CallFrame,
        };
        self.send_one_payload(&Destination::ChannelDirect(target), message_type, &WirePayload::Call(message)).await
    }

    async fn send_one_payload(
        &self,
        destination: &Destination,
        message_type: MessageType,
        payload: &WirePayload,
    ) -> anyhow::Result<()> {
        let envelope = self.build_envelope(destination, message_type, payload)?;
        let sender = envelope.sender;
        let message_id = envelope.message_id;

        // Diagnostic only -- message_id is random opaque bytes, sender/recipient are
        // public short ids, and neither the plaintext nor any key material is ever
        // logged here or anywhere else in this file.
        log::debug!(
            "mesh-core: sending message_id={} type={:?} mode={:?} sender={} recipient={}",
            hex_prefix(&message_id),
            message_type,
            envelope.encryption_mode,
            short_id(&sender),
            envelope.recipient.map(|r| short_id(&r)).unwrap_or_else(|| "broadcast".to_string()),
        );

        // Mark our own message as seen -- both the fast in-memory `FloodGuard` and
        // (for anything other than ephemeral `CallFrame` traffic) durably in
        // `ReplayStore`, immediately. Unlike a *received* message (see
        // `handle_incoming`'s Milestone 2D.1/3C doc sections, where a `Chat` message's
        // durable marking is deliberately deferred until `self.inbox` durably persists
        // it), there is nothing to defer judgment on for a message *we* originate: we
        // are the author, so "successfully originated" is true unconditionally, before
        // we even attempt to flood it -- and if this exact envelope loops back to us
        // via the mesh (a real, common occurrence, since flooding doesn't check
        // whether a relay happens to be the original sender), it must be recognized
        // immediately as already handled, or we would re-flood our own message
        // indefinitely. This applies equally to a best-effort `send_text` `Chat`
        // message as to any other type -- the deferred-until-durably-persisted rule is
        // specifically about *received* content, not our own.
        self.flood_guard.lock().unwrap().check_and_insert(&sender, &message_id);
        if message_type != MessageType::CallFrame {
            self.replay_store.mark_seen(&sender, &message_id, envelope.created_at, envelope.expires_at);
        }
        self.flood(&envelope).await
    }

    /// Builds (and signs) an `Envelope` for `payload` addressed via `destination`,
    /// without sending or recording anything -- split out from `send_one_payload` so
    /// `send_reliable_text` can persist the exact bytes to `delivery_store` *before* the
    /// first transmission attempt (see that method's doc).
    fn build_envelope(&self, destination: &Destination, message_type: MessageType, payload: &WirePayload) -> anyhow::Result<Envelope> {
        let plaintext = bincode::serialize(payload)?;
        let mut message_id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut message_id);
        let sender = self.identity.node_id();
        let created_at = now_millis();
        let expires_at = created_at + DEFAULT_MESSAGE_LIFETIME.as_millis() as u64;
        let recipient = destination.recipient_node_id();

        let (encryption_mode, nonce, ciphertext) = match destination {
            Destination::Broadcast | Destination::ChannelDirect(_) => {
                let (ciphertext, nonce) = self.channel_key.encrypt(&plaintext);
                (EncryptionMode::ChannelV1, nonce, ciphertext)
            }
            Destination::DirectV1(recipient_identity) => {
                // Fail closed: never fall back to the shared channel key or plaintext if
                // the recipient's encryption identity doesn't check out (see
                // `direct_crypto`'s module doc).
                let verified = recipient_identity
                    .clone()
                    .verify()
                    .map_err(|_| DirectCryptoError::CannotEncryptForRecipient)?;
                let session = Session::establish(&self.identity, &verified).ok_or(DirectCryptoError::CannotEncryptForRecipient)?;
                let aad = DirectMessageAadV1::new(
                    PROTOCOL_VERSION,
                    message_id,
                    sender,
                    recipient_identity.node_id,
                    message_type,
                    created_at,
                    expires_at,
                    self.default_max_hops,
                );
                let direct_ciphertext = encrypt_direct_message(&self.identity, &session, &aad, &plaintext);
                let body = DirectEnvelopeBody {
                    header: DirectCryptoHeaderV1::new(&self.identity),
                    message: direct_ciphertext,
                };
                let ciphertext_bytes = bincode::serialize(&body)?;
                // The envelope's own top-level `nonce` field isn't used by DirectV1 --
                // the real AEAD nonce lives inside `body.message.nonce` -- so this is
                // just an unused placeholder for this mode.
                (EncryptionMode::DirectV1, [0u8; 24], ciphertext_bytes)
            }
        };

        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id,
            sender,
            recipient,
            message_type,
            encryption_mode,
            created_at,
            expires_at,
            max_hops: self.default_max_hops,
            hops_used: 0,
            nonce,
            ciphertext,
            signature: Vec::new(),
        };
        envelope.signature = self.identity.sign(&envelope.signed_payload()).to_vec();
        Ok(envelope)
    }

    /// Sends `envelope`'s bytes to every currently-reachable peer. Returns `Ok(())` if
    /// at least one peer accepted it (or there were no peers to send to at all -- a
    /// vacuous, technically-blameless "nothing to forward"), or `Err` only if there was
    /// at least one peer and *every* attempt failed. This distinction (rather than
    /// always claiming success, which the previous implementation did) is what lets
    /// `handle_incoming`'s Milestone 2D.1 relay logic tell "forwarding actually
    /// succeeded" apart from "forwarding didn't happen" -- see its doc.
    async fn flood(&self, envelope: &Envelope) -> anyhow::Result<()> {
        let bytes = bincode::serialize(envelope)?;
        self.flood_bytes(envelope.message_id, bytes).await
    }

    /// Like `flood`, but takes already-serialized bytes directly -- used by the
    /// Milestone 3A delivery-retry path (`attempt_delivery`/`retry_due_deliveries`),
    /// which must retransmit the *exact* originally-persisted envelope bytes rather than
    /// re-serializing (or, worse, re-encrypting) anything.
    async fn flood_bytes(&self, message_id: [u8; 16], bytes: Vec<u8>) -> anyhow::Result<()> {
        let peers = self.transport.peers();
        log::debug!("mesh-core: flooding message_id={} to {} peer(s)", hex_prefix(&message_id), peers.len());
        let mut any_succeeded = peers.is_empty();
        for peer in &peers {
            match self.transport.send_to_peer(peer, bytes.clone()).await {
                Ok(()) => any_succeeded = true,
                Err(err) => {
                    log::warn!("mesh-core: failed to forward message_id={} to a peer -- {}", hex_prefix(&message_id), err);
                }
            }
        }
        if any_succeeded {
            Ok(())
        } else {
            anyhow::bail!("failed to forward message_id={} to any of {} peer(s)", hex_prefix(&message_id), peers.len());
        }
    }

    /// Feed one raw incoming packet in. Returns `Some(IncomingEvent::Content(..))` once a
    /// full message (which may have taken several chunks) is ready to show the user, or
    /// `Some(IncomingEvent::Progress(..))` for a chunk that's part of a still-incomplete
    /// transfer (so the caller can show a live progress bar). Already-seen, invalid,
    /// expired, or undecryptable packets return `None` (they may still have been relayed
    /// onward). Malformed input (garbage bytes, truncated packets, oversized packets)
    /// never panics -- it returns `Ok(None)` or an `Err` the caller can log and ignore.
    ///
    /// # Milestone 2D.1: "authenticated" is not "successfully handled"
    /// A packet whose outer signature verifies is merely *authenticated* -- it is not
    /// yet decrypted, not yet accepted by the application layer, and (if this node is
    /// only relaying it) not yet actually forwarded anywhere. Durably recording a
    /// `(sender, message_id)` pair in the `ReplayStore` the moment a signature checks
    /// out -- what an earlier version of this milestone did -- conflates "seen" with
    /// "successfully handled," which creates a real failure mode: if this node is a
    /// relay whose forwarding attempt fails (e.g. the network is briefly down) or
    /// crashes between recording "seen" and actually forwarding, a legitimate resend of
    /// the exact same authenticated packet would be silently dropped forever, even
    /// though it was never actually delivered anywhere. For an off-grid resilient
    /// mesh, occasionally forwarding (or accepting) a duplicate is far less harmful than
    /// silently losing a message that was never truly handled -- so this implementation
    /// only durably records a pair once whatever it was protecting has *actually,
    /// successfully* happened:
    /// - **Relaying:** only after `flood()` reports at least one peer actually accepted
    ///   the forward (see `flood`'s doc). A failed or not-yet-attempted forward leaves
    ///   the pair unrecorded, so a later resend (even after this node restarts) is
    ///   free to retry it.
    /// - **Being the final recipient:** only after `decrypt_payload` succeeds. A
    ///   packet whose outer signature is valid but whose inner AEAD/decryption fails
    ///   (tampered ciphertext, wrong session, or any other decrypt failure) is
    ///   deliberately *not* recorded either -- decryption of the exact same bytes is
    ///   deterministic, so repeating it teaches an attacker nothing new and blocks
    ///   nothing a legitimate resend would need; the only cost is repeated CPU work,
    ///   which is a rate-limiting concern for a later milestone, not a replay-security
    ///   one.
    ///
    /// A separate, fast, purely in-memory `FloodGuard` (not durable, reset on every
    /// restart by design) still runs first for every packet, for two reasons: (a) it's
    /// the *only* de-duplication `CallFrame` traffic gets at all (see `flood_guard.rs`'s
    /// doc for why high-rate call media deliberately never touches the durable SQLite
    /// store), and (b) for everything else, it's a cheap first-pass filter that avoids
    /// hitting SQLite at all for a packet flooded to this node redundantly by multiple
    /// neighbors within the same process's uptime -- it never overrides the
    /// `ReplayStore` logic above, it only skips redundant work within one session.
    pub async fn handle_incoming(&self, raw: Vec<u8>) -> anyhow::Result<Option<IncomingEvent>> {
        if raw.len() > MAX_ENVELOPE_BYTES {
            log::warn!("mesh-core: rejected incoming packet -- oversized ({} bytes)", raw.len());
            return Ok(None); // reject before ever attempting to deserialize it
        }
        let envelope: Envelope = bincode::deserialize(&raw)?;

        let Ok(sig): Result<[u8; 64], _> = envelope.signature.clone().try_into() else {
            log::warn!("mesh-core: rejected message_id={} -- malformed signature length", hex_prefix(&envelope.message_id));
            return Ok(None); // malformed signature length, drop silently
        };
        if !verify(&envelope.sender, &envelope.signed_payload(), &sig) {
            log::warn!("mesh-core: rejected message_id={} from sender={} -- signature verification failed", hex_prefix(&envelope.message_id), short_id(&envelope.sender));
            return Ok(None); // forged or corrupted, drop silently
        }
        if !envelope.is_supported_version() {
            log::warn!(
                "mesh-core: rejected message_id={} -- unsupported protocol_version={}",
                hex_prefix(&envelope.message_id),
                envelope.protocol_version
            );
            return Ok(None); // a future wire format we don't know how to interpret
        }

        // Never mark/check a (sender, message_id) pair before its signature has already
        // been verified above -- both fields are part of `signed_payload()`, so an
        // attacker can't forge a packet reusing an arbitrary (e.g. not-yet-used)
        // sender/message_id pair without also forging a valid signature for it -- this
        // ordering (verify first) is what stops a fake, unauthenticated packet from
        // ever pre-emptively "consuming" a pair and causing a later legitimate packet
        // reusing it to be dropped. Do not reorder this. See this function's doc for why
        // `CallFrame` traffic uses only the in-memory `FloodGuard`, never `ReplayStore`.
        let is_call_frame = envelope.message_type == MessageType::CallFrame;
        if is_call_frame {
            if !self.flood_guard.lock().unwrap().check_and_insert(&envelope.sender, &envelope.message_id) {
                return Ok(None);
            }
        } else if self.replay_store.contains(&envelope.sender, &envelope.message_id) {
            log::debug!(
                "mesh-core: dropped message_id={} from sender={} -- already durably recorded as successfully processed (duplicate/replay)",
                hex_prefix(&envelope.message_id),
                short_id(&envelope.sender)
            );
            // Milestone 3C: for a `Chat` message *addressed to us specifically*,
            // `replay_store` is only ever durably marked once `self.inbox` has already
            // durably persisted it (see this function's `Accepted::Complete` handling
            // below) -- so reaching here for such a message means it's already durably
            // in this node's own inbox and an ack was already sent. If that ack never
            // reached the sender, they'll keep retrying forever unless we re-send it
            // now. This is cheap and safe to do even for a genuine duplicate: it only
            // re-verifies the DirectV1 header's binding signature (no AEAD decryption,
            // no re-processing/re-surfacing of content to the application layer).
            // Deliberately gated on us being the addressee -- a pure relay's
            // `replay_store` entry for this pair means "I successfully forwarded it"
            // (see the relay-marking below), which says nothing about whether the real
            // recipient ever accepted it, so a relay must never generate an ack on the
            // original sender's behalf.
            let we_are_the_addressee = envelope.recipient == Some(self.identity.node_id());
            if envelope.message_type == MessageType::Chat && we_are_the_addressee {
                if let Some(sender_identity) = Self::try_extract_direct_v1_sender(&envelope) {
                    let _ = self.send_delivery_ack(&sender_identity, envelope.message_id).await;
                }
            }
            return Ok(None);
        }

        if envelope.is_expired(now_millis()) {
            log::debug!("mesh-core: dropped message_id={} -- expired", hex_prefix(&envelope.message_id));
            return Ok(None); // stale -- don't relay or process a message past its expiry
        }

        // Relay onward first (store-and-forward the hop) regardless of whether we can
        // decrypt the payload ourselves, or who it's addressed to, so nodes without the
        // channel key -- and messages meant for someone else -- still get carried across
        // hops. Only durably recorded as "seen" if forwarding actually succeeded -- see
        // this function's Milestone 2D.1 doc section. Deliberately gated on this node
        // NOT being the addressee: if we *are* the addressee (or it's a broadcast with
        // no single addressee), the durable "successfully handled" signal must come
        // exclusively from decryption actually succeeding below, not merely from also
        // having relayed the still-undecrypted bytes onward -- otherwise a message
        // addressed to us that we happen to also be able to flood, but then fail to
        // decrypt, would be wrongly marked as durably "handled".
        let we_are_not_the_addressee = matches!(envelope.recipient, Some(recipient) if recipient != self.identity.node_id());
        if !envelope.hop_budget_exhausted() {
            let mut relayed = envelope.clone();
            relayed.hops_used += 1;
            if !is_call_frame && we_are_not_the_addressee {
                // Milestone 3B: durable, per-neighbor forwarding state instead of a
                // single collapsed "at least one peer accepted it" success/failure
                // signal -- see `forward_store.rs`'s doc for the partial-forwarding-
                // failure scenario (some neighbors succeed, others fail) this closes.
                // Only marks the message durably `seen` in `ReplayStore` once *every*
                // neighbor known at receipt time has actually received it (or given up
                // after expiring) -- a peer that failed stays independently retryable
                // via `retry_pending_forwards`, regardless of whichever other peers
                // already succeeded.
                if self.relay_forward(&envelope.sender, envelope.message_id, envelope.expires_at, &relayed).await {
                    self.replay_store.mark_seen(&envelope.sender, &envelope.message_id, now_millis(), envelope.expires_at);
                }
            } else if let Err(err) = self.flood(&relayed).await {
                // Either ephemeral call-frame traffic (never durably tracked at all --
                // see `flood_guard.rs`) or this node is also the addressee/it's a
                // broadcast (the durable "handled" signal for those comes from
                // decryption succeeding below, not from this best-effort extra hop) --
                // so a failed attempt here is only ever a best-effort log, never
                // durably recorded either way.
                log::warn!("mesh-core: forwarding attempt failed for message_id={} -- {}", hex_prefix(&envelope.message_id), err);
            }
        }

        // Addressed to someone else -- already relayed above, nothing more to do. This
        // check happens before decrypting, using the envelope's own (authenticated)
        // `recipient` field, so a non-recipient doesn't need to touch the ciphertext at
        // all for a message that isn't theirs (this alone doesn't make content private
        // from other channel members -- see `crypto.rs` -- it only avoids unnecessary
        // decrypt attempts and keeps addressing legible to a future routing layer).
        if let Some(recipient) = envelope.recipient {
            if recipient != self.identity.node_id() {
                return Ok(None);
            }
        }

        let Some((payload, sender_identity)) = self.decrypt_payload(&envelope) else {
            log::warn!(
                "mesh-core: could not decrypt message_id={} from sender={} mode={:?} -- dropped, not durably recorded",
                hex_prefix(&envelope.message_id),
                short_id(&envelope.sender),
                envelope.encryption_mode
            );
            return Ok(None);
        };
        log::debug!(
            "mesh-core: decrypted message_id={} from sender={} mode={:?}",
            hex_prefix(&envelope.message_id),
            short_id(&envelope.sender),
            envelope.encryption_mode
        );
        // Milestone 3C: for `Chat` messages, durable "seen" marking is deliberately
        // deferred until `self.inbox.insert_if_absent` actually durably commits the
        // reassembled content -- NOT done here, even though decryption just succeeded.
        // See the module doc and `inbox_store.rs`'s doc: a packet must not become
        // permanently "processed" merely because AEAD decryption succeeded, or a crash
        // between here and the durable inbox write would silently and permanently lose
        // the content on a legitimate retry. For every other type (`CallSignal`,
        // `DeliveryAck`), there is no ack protocol, so marking immediately (the old
        // Milestone 2D.1 behavior) is still correct.
        if !is_call_frame && envelope.message_type != MessageType::Chat {
            self.replay_store.mark_seen(&envelope.sender, &envelope.message_id, now_millis(), envelope.expires_at);
        }

        match payload {
            WirePayload::Chunk(chunk) => {
                let accepted = self.reassembler.lock().unwrap().accept(chunk);
                match accepted {
                    Accepted::Progress(p) => Ok(Some(IncomingEvent::Progress(envelope.sender, p))),
                    Accepted::Complete(content) => {
                        // Milestone 3C: this durable insert -- not decryption, not
                        // reassembly completing, not any app callback -- is what
                        // decides whether an ack goes out at all. See
                        // `inbox_store.rs`'s module doc for the full ordering
                        // rationale and exactly what each outcome means.
                        let now = now_millis();
                        match self.inbox.insert_if_absent(&envelope.sender, &envelope.message_id, now, &content) {
                            Ok(InsertOutcome::Inserted) => {
                                self.replay_store.mark_seen(&envelope.sender, &envelope.message_id, now, envelope.expires_at);
                                if let Some(identity) = &sender_identity {
                                    if let Err(err) = self.send_delivery_ack(identity, envelope.message_id).await {
                                        log::warn!("mesh-core: failed to send delivery ack for message_id={} -- {} (sender will retry)", hex_prefix(&envelope.message_id), err);
                                    }
                                }
                                Ok(Some(IncomingEvent::Content(DeliveredContent {
                                    sender: envelope.sender,
                                    sender_identity,
                                    message_id: envelope.message_id,
                                    content,
                                })))
                            }
                            Ok(InsertOutcome::AlreadyPresent) => {
                                // Already durably accepted -- the sender's earlier ack
                                // apparently never arrived, or this is a legitimate
                                // retry racing with one still in flight. Re-ack, but
                                // never resurface already-accepted content to the
                                // application layer again.
                                if let Some(identity) = &sender_identity {
                                    let _ = self.send_delivery_ack(identity, envelope.message_id).await;
                                }
                                Ok(None)
                            }
                            Err(err) => {
                                // The durable write itself failed -- do NOT ack. This is
                                // the crux of Milestone 3C: an ack must mean "durably
                                // accepted," never merely "decrypted/reassembled," so a
                                // failure right here must leave the sender's own retry
                                // loop (see `delivery_store.rs`) free to try again later.
                                log::warn!(
                                    "mesh-core: failed to durably persist message_id={} from sender={} -- {} (no ack sent; sender should retry)",
                                    hex_prefix(&envelope.message_id),
                                    short_id(&envelope.sender),
                                    err
                                );
                                Ok(None)
                            }
                        }
                    }
                }
            }
            // The envelope-level `recipient` check above already filtered out anything
            // not addressed to us, so reaching here means this call message is ours.
            WirePayload::Call(message) => Ok(Some(IncomingEvent::Call(envelope.sender, message))),
            WirePayload::DeliveryAck(ack) => {
                let accepted = self.delivery.acknowledge_from(&ack.acked_message_id, &envelope.sender);
                log::debug!(
                    "mesh-core: delivery ack for message_id={} from sender={} accepted={}",
                    hex_prefix(&ack.acked_message_id),
                    short_id(&envelope.sender),
                    accepted
                );
                // Acks are protocol-internal bookkeeping, never surfaced to the
                // application layer as a content/call event.
                Ok(None)
            }
        }
    }

    /// Re-verifies a `DirectV1` envelope's embedded header binding (no AEAD decryption)
    /// to recover the sender's `PublicIdentity` -- used only to re-send a `DeliveryAck`
    /// for an already-acknowledged duplicate (see `handle_incoming`'s Milestone 3A
    /// re-ack branch), where re-doing the full decrypt would be unnecessary work for
    /// content that's already been durably accepted and will not be reprocessed.
    fn try_extract_direct_v1_sender(envelope: &Envelope) -> Option<PublicIdentity> {
        if envelope.encryption_mode != EncryptionMode::DirectV1 {
            return None;
        }
        let body: DirectEnvelopeBody = bincode::deserialize(&envelope.ciphertext).ok()?;
        let verified = body.header.verify_sender(envelope.sender)?;
        Some(verified.public_identity().clone())
    }

    /// Decrypts an envelope's `ciphertext` into a `WirePayload`, dispatching on
    /// `encryption_mode` -- `None` on any failure (wrong key, tampered data, unverified
    /// sender X25519 binding, non-contributory shared secret, malformed inner structure),
    /// never a panic. Only ever called after the envelope-level checks in
    /// `handle_incoming` (signature, version, dedup, expiry, recipient) already passed.
    /// Also returns the sender's verified `PublicIdentity` for a `DirectV1` message
    /// (`None` for `ChannelV1`, which has no per-sender key binding) -- see
    /// `DeliveredContent::sender_identity`'s doc for why the caller needs this.
    fn decrypt_payload(&self, envelope: &Envelope) -> Option<(WirePayload, Option<PublicIdentity>)> {
        match envelope.encryption_mode {
            EncryptionMode::ChannelV1 => {
                let plaintext = self.channel_key.decrypt(&envelope.ciphertext, &envelope.nonce)?;
                let payload = bincode::deserialize::<WirePayload>(&plaintext).ok()?;
                Some((payload, None))
            }
            EncryptionMode::DirectV1 => {
                let body: DirectEnvelopeBody = bincode::deserialize(&envelope.ciphertext).ok()?;
                // Reconstruct and verify the sender's identity from what the packet
                // itself carries -- no prior handshake or discovery needed (see
                // `direct_crypto`'s module doc's "Milestone 2B.1" section). This is what
                // makes a DirectV1 envelope self-contained and store-and-forward-friendly.
                let verified_sender = body.header.verify_sender(envelope.sender)?;
                let sender_identity = verified_sender.public_identity().clone();
                let session = Session::establish(&self.identity, &verified_sender)?;
                let aad = DirectMessageAadV1::new(
                    envelope.protocol_version,
                    envelope.message_id,
                    envelope.sender,
                    self.identity.node_id(),
                    envelope.message_type,
                    envelope.created_at,
                    envelope.expires_at,
                    envelope.max_hops,
                );
                let plaintext = decrypt_direct_message(&session, &envelope.sender, &aad, &body.message).ok()?;
                let payload = bincode::deserialize::<WirePayload>(&plaintext).ok()?;
                Some((payload, Some(sender_identity)))
            }
        }
    }
}

#[cfg(test)]
mod relay_tests {
    //! Exercises `MeshNode`'s actual relay/addressing logic end-to-end (not just the
    //! `Envelope` struct in isolation) using an in-memory `Transport` -- no real
    //! sockets, so these run fast and deterministically. This is the multi-hop chain
    //! relay behavior the whole project is built around: a message from node A reaches
    //! node C two hops away via B, without B being able to tell the UI layer it arrived.

    use super::*;
    use crate::crypto::ChannelKey;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex as AsyncMutex};

    /// Shared registry so `MockTransport::send_to_peer` can hand bytes directly to
    /// another node's inbox by name, standing in for "radio range" delivery.
    struct MockNetwork {
        senders: Mutex<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    }

    impl MockNetwork {
        fn new() -> Arc<Self> {
            Arc::new(Self { senders: Mutex::new(HashMap::new()) })
        }
    }

    struct MockTransport {
        network: Arc<MockNetwork>,
        peer_names: Vec<String>,
        rx: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        /// Milestone 2D.1: when set, every `send_to_peer` call fails -- lets tests
        /// simulate a relay whose forwarding attempt genuinely fails (e.g. the network
        /// is briefly down), as distinct from a successful forward.
        fail_sends: Arc<AtomicBool>,
        /// Milestone 3B: names of peers whose `send_to_peer` calls should fail, while
        /// every other peer still succeeds -- lets tests simulate a relay's flood being
        /// only *partially* successful (e.g. B succeeds, C and D fail), as opposed to
        /// `fail_sends`' all-or-nothing blanket failure.
        fail_peers: Arc<Mutex<HashSet<String>>>,
    }

    impl MockTransport {
        fn register(network: &Arc<MockNetwork>, name: &str, peer_names: Vec<String>) -> Self {
            Self::register_with_failure_flag(network, name, peer_names, Arc::new(AtomicBool::new(false)))
        }

        fn register_with_failure_flag(network: &Arc<MockNetwork>, name: &str, peer_names: Vec<String>, fail_sends: Arc<AtomicBool>) -> Self {
            Self::register_with_failures(network, name, peer_names, fail_sends, Arc::new(Mutex::new(HashSet::new())))
        }

        fn register_with_failing_peers(network: &Arc<MockNetwork>, name: &str, peer_names: Vec<String>, fail_peers: Arc<Mutex<HashSet<String>>>) -> Self {
            Self::register_with_failures(network, name, peer_names, Arc::new(AtomicBool::new(false)), fail_peers)
        }

        fn register_with_failures(
            network: &Arc<MockNetwork>,
            name: &str,
            peer_names: Vec<String>,
            fail_sends: Arc<AtomicBool>,
            fail_peers: Arc<Mutex<HashSet<String>>>,
        ) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            network.senders.lock().unwrap().insert(name.to_string(), tx);
            Self {
                network: network.clone(),
                peer_names,
                rx: AsyncMutex::new(rx),
                fail_sends,
                fail_peers,
            }
        }
    }

    #[async_trait::async_trait]
    impl Transport for MockTransport {
        async fn send_to_peer(&self, peer: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
            if self.fail_sends.load(Ordering::SeqCst) {
                anyhow::bail!("simulated transport failure");
            }
            if self.fail_peers.lock().unwrap().contains(peer) {
                anyhow::bail!("simulated transport failure to peer {peer}");
            }
            if let Some(tx) = self.network.senders.lock().unwrap().get(peer) {
                let _ = tx.send(bytes);
            }
            Ok(())
        }

        async fn recv(&self) -> anyhow::Result<Vec<u8>> {
            self.rx
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("mock network channel closed"))
        }

        fn peers(&self) -> Vec<String> {
            self.peer_names.clone()
        }
    }

    fn make_node(network: &Arc<MockNetwork>, name: &str, peer_names: Vec<String>, max_hops: u8) -> MeshNode<MockTransport> {
        make_node_with_identity(network, name, peer_names, max_hops, Identity::generate())
    }

    /// Like `make_node`, but takes an already-constructed `Identity` instead of
    /// generating a fresh one -- needed by tests that manually craft envelopes (they
    /// need a second, independent `Identity` handle for the same node to sign/derive
    /// things with directly, since `MeshNode` doesn't expose its internal `Identity`).
    /// Reconstruct a matching identity via `Identity::from_seed(original.seed())`.
    fn make_node_with_identity(network: &Arc<MockNetwork>, name: &str, peer_names: Vec<String>, max_hops: u8, identity: Identity) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register(network, name, peer_names);
        MeshNode::new(identity, channel_key, transport, max_hops)
    }

    /// Like `make_node_with_identity`, but backed by a persistent (file-based)
    /// `ReplayStore` at `replay_store_path` instead of an in-memory-only one --
    /// Milestone 2D tests use this to simulate a node restarting with its replay
    /// protection intact, the same way `mesh-mobile`'s `contacts_db_path` simulates a
    /// restarted `MeshClient` keeping its contact cache.
    fn make_node_with_identity_and_replay_store(
        network: &Arc<MockNetwork>,
        name: &str,
        peer_names: Vec<String>,
        max_hops: u8,
        identity: Identity,
        replay_store_path: &std::path::Path,
    ) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register(network, name, peer_names);
        let (replay_store, _was_reset) = ReplayStore::open(replay_store_path);
        MeshNode::new_with_replay_store(identity, channel_key, transport, max_hops, replay_store)
    }

    /// Like `make_node_with_identity_and_replay_store`, but the returned node's
    /// transport shares `fail_sends` -- flip it to `true` (via `Ordering::SeqCst`) to
    /// make every subsequent forward attempt fail, simulating Milestone 2D.1's "the
    /// network is briefly down" scenario.
    fn make_node_with_identity_replay_store_and_failure_flag(
        network: &Arc<MockNetwork>,
        name: &str,
        peer_names: Vec<String>,
        max_hops: u8,
        identity: Identity,
        replay_store_path: &std::path::Path,
        fail_sends: Arc<AtomicBool>,
    ) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register_with_failure_flag(network, name, peer_names, fail_sends);
        let (replay_store, _was_reset) = ReplayStore::open(replay_store_path);
        MeshNode::new_with_replay_store(identity, channel_key, transport, max_hops, replay_store)
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        needle.is_empty() || haystack.windows(needle.len()).any(|window| window == needle)
    }

    /// The core "chain relay" guarantee this project is named after: a direct message
    /// reaches its recipient two hops away, and the relay in the middle can forward it
    /// without being able to tell it was ever delivered.
    #[tokio::test]
    async fn direct_message_reaches_recipient_two_hops_away_via_relay() {
        let network = MockNetwork::new();
        // Chain: alice -- bob -- carol (alice and carol are NOT directly connected).
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string(), "carol".to_string()], 16);
        let carol = make_node(&network, "carol", vec!["bob".to_string()], 16);

        let alice_id = alice.node_id();
        alice.send_text(&carol.public_identity(), "hello carol").await.unwrap();

        // Bob relays it onward, but it's addressed to Carol, not him -- no content event.
        let bob_raw = bob.recv_raw().await.unwrap();
        let bob_event = bob.handle_incoming(bob_raw).await.unwrap();
        assert!(bob_event.is_none());

        // Carol receives the relayed copy and it decrypts to the original text.
        let carol_raw = carol.recv_raw().await.unwrap();
        let carol_event = carol.handle_incoming(carol_raw).await.unwrap();
        match carol_event {
            Some(IncomingEvent::Content(DeliveredContent { sender, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(sender, alice_id);
                assert_eq!(text, "hello carol");
            }
            _ => panic!("expected carol to receive the relayed text message"),
        }
    }

    /// A message addressed to Carol must not surface as an `IncomingEvent` to Bob, the
    /// relay in between -- this is what keeps a 1:1 conversation private to its two
    /// participants at the application layer, even over multiple hops.
    #[tokio::test]
    async fn relay_never_surfaces_content_not_addressed_to_it() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string(), "carol".to_string()], 16);
        let carol = make_node(&network, "carol", vec!["bob".to_string()], 16);

        alice.send_text(&carol.public_identity(), "private to carol").await.unwrap();

        let bob_raw = bob.recv_raw().await.unwrap();
        assert!(bob.handle_incoming(bob_raw).await.unwrap().is_none());
    }

    /// A sender-declared hop budget of 0 means "don't relay this beyond whoever I sent it
    /// to directly" -- bob (a direct neighbor of alice) still receives the transmission
    /// itself, but must not forward it onward to carol once the budget is exhausted.
    #[tokio::test]
    async fn hop_budget_of_zero_prevents_any_relay_beyond_direct_neighbors() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 0);
        let bob = make_node(&network, "bob", vec!["alice".to_string(), "carol".to_string()], 0);
        let carol = make_node(&network, "carol", vec!["bob".to_string()], 0);

        alice.send_text(&carol.public_identity(), "should not be relayed further").await.unwrap();

        // Bob gets the direct transmission from alice, but he's not the recipient and
        // his hop budget for relaying it further is already exhausted (max_hops: 0).
        let bob_raw = bob.recv_raw().await.unwrap();
        assert!(bob.handle_incoming(bob_raw).await.unwrap().is_none());

        // Carol must never receive anything -- bob never relayed it onward.
        let carol_result = tokio::time::timeout(std::time::Duration::from_millis(200), carol.recv_raw()).await;
        assert!(carol_result.is_err(), "carol should not have received a relayed packet");
    }

    /// A broadcast (no specific recipient) must still reach every node on the channel,
    /// same as before this change -- `mesh-cli`'s demo depends on this.
    #[tokio::test]
    async fn broadcast_reaches_all_connected_nodes() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.broadcast_text("hello everyone").await.unwrap();

        let bob_raw = bob.recv_raw().await.unwrap();
        match bob.handle_incoming(bob_raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(text, "hello everyone");
            }
            _ => panic!("expected bob to receive the broadcast text message"),
        }
    }

    /// A node with an unsupported `protocol_version` must be dropped outright, even if
    /// everything else about it (signature, recipient, etc) is otherwise valid.
    #[tokio::test]
    async fn handle_incoming_drops_envelope_with_unsupported_protocol_version() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec![], 16);
        let alice_identity = Identity::generate();
        let mut envelope = Envelope {
            protocol_version: crate::message::PROTOCOL_VERSION + 1,
            message_id: [1u8; 16],
            sender: alice_identity.node_id(),
            recipient: Some(bob.node_id()),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::ChannelV1,
            created_at: now_millis(),
            expires_at: now_millis() + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext: vec![1, 2, 3],
            signature: Vec::new(),
        };
        envelope.signature = alice_identity.sign(&envelope.signed_payload()).to_vec();
        let raw = bincode::serialize(&envelope).unwrap();

        assert!(bob.handle_incoming(raw).await.unwrap().is_none());
    }

    /// Random noise that isn't a validly-encoded `Envelope` at all must never panic --
    /// it should just fail to deserialize and surface as an ordinary `Err`, which callers
    /// already handle by dropping the packet and continuing (see `mesh-mobile`'s recv
    /// loop).
    #[tokio::test]
    async fn handle_incoming_never_panics_on_random_garbage_bytes() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec![], 16);

        let mut garbage = vec![0u8; 200];
        rand::rngs::OsRng.fill_bytes(&mut garbage);

        // The only requirement here is that this doesn't panic; whether it comes back
        // as Ok(None) or Err depends on whether the random bytes happen to look like a
        // structurally valid (but unsigned/unverifiable) envelope.
        let _ = bob.handle_incoming(garbage).await;
    }

    /// A truncated (cut-off mid-packet) copy of an otherwise-valid envelope must not
    /// panic -- just fail to deserialize cleanly.
    #[tokio::test]
    async fn handle_incoming_never_panics_on_truncated_envelope() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec![], 16);
        let alice_identity = Identity::generate();
        let mut envelope = Envelope {
            protocol_version: crate::message::PROTOCOL_VERSION,
            message_id: [1u8; 16],
            sender: alice_identity.node_id(),
            recipient: Some(bob.node_id()),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::ChannelV1,
            created_at: now_millis(),
            expires_at: now_millis() + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext: vec![1, 2, 3, 4, 5, 6, 7, 8],
            signature: Vec::new(),
        };
        envelope.signature = alice_identity.sign(&envelope.signed_payload()).to_vec();
        let raw = bincode::serialize(&envelope).unwrap();
        let truncated = raw[..raw.len() / 2].to_vec();

        // Must not panic; a truncated packet failing to parse is expected and fine.
        let _ = bob.handle_incoming(truncated).await;
    }

    /// An oversized raw packet must be rejected immediately (before ever attempting to
    /// deserialize it) rather than triggering a large allocation -- see
    /// `MAX_ENVELOPE_BYTES`. Wrapped in a timeout so a regression that removed the size
    /// check (and instead tried to fully process/allocate for this) would fail the test
    /// by timing out, not just by being slow.
    #[tokio::test]
    async fn handle_incoming_quickly_rejects_oversized_packets() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec![], 16);
        let huge = vec![0u8; MAX_ENVELOPE_BYTES + 1];

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), bob.handle_incoming(huge)).await;
        assert!(result.is_ok(), "oversized packet should be rejected quickly, not hang");
        assert!(result.unwrap().unwrap().is_none());
    }

    /// A signature field with the wrong byte length (not a valid Ed25519 signature at
    /// all) must be rejected gracefully instead of panicking on the length conversion.
    #[tokio::test]
    async fn handle_incoming_rejects_malformed_signature_length() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec![], 16);
        let alice_identity = Identity::generate();
        let envelope = Envelope {
            protocol_version: crate::message::PROTOCOL_VERSION,
            message_id: [1u8; 16],
            sender: alice_identity.node_id(),
            recipient: Some(bob.node_id()),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::ChannelV1,
            created_at: now_millis(),
            expires_at: now_millis() + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext: vec![1, 2, 3],
            signature: vec![0u8; 10], // valid Ed25519 signatures are always 64 bytes
        };
        let raw = bincode::serialize(&envelope).unwrap();

        assert!(bob.handle_incoming(raw).await.unwrap().is_none());
    }

    // -- Milestone 2B.1: real "MeshTalk Direct Encryption v1" integration end-to-end --

    /// Alice -- Bob, directly connected: the simplest real encrypted messaging case.
    #[tokio::test]
    async fn alice_to_bob_direct_encrypted_message_is_delivered() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let bob_raw = bob.recv_raw().await.unwrap();
        match bob.handle_incoming(bob_raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { sender, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(sender, alice.node_id());
                assert_eq!(text, "hello");
            }
            _ => panic!("expected bob to receive and decrypt alice's message"),
        }
    }

    /// Alice -- Relay -- Bob: the relay forwards the encrypted envelope but never
    /// decrypts it (it isn't the recipient, and has no session key even if it tried).
    #[tokio::test]
    async fn alice_to_bob_via_one_relay_decrypts_and_relay_never_sees_plaintext() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay".to_string()], 16);
        let relay = make_node(&network, "relay", vec!["alice".to_string(), "bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["relay".to_string()], 16);

        alice.send_text(&bob.public_identity(), "secret for bob").await.unwrap();

        // The relay receives it, forwards it (it's not addressed to relay), and never
        // surfaces any content -- it cannot decrypt this.
        let relay_raw = relay.recv_raw().await.unwrap();
        assert!(relay.handle_incoming(relay_raw.clone()).await.unwrap().is_none());
        // Defense-in-depth: the plaintext must not appear anywhere in what the relay
        // actually received (guards against a future regression leaking plaintext
        // alongside the ciphertext).
        assert!(!contains_bytes(&relay_raw, b"secret for bob"));

        let bob_raw = bob.recv_raw().await.unwrap();
        match bob.handle_incoming(bob_raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { sender, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(sender, alice.node_id());
                assert_eq!(text, "secret for bob");
            }
            _ => panic!("expected bob to receive and decrypt alice's message"),
        }
    }

    /// Alice -- Relay1 -- Relay2 -- Bob: the same guarantee holds across two hops.
    #[tokio::test]
    async fn alice_to_bob_via_two_relays_decrypts_and_relays_never_see_plaintext() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay1".to_string()], 16);
        let relay1 = make_node(&network, "relay1", vec!["alice".to_string(), "relay2".to_string()], 16);
        let relay2 = make_node(&network, "relay2", vec!["relay1".to_string(), "bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["relay2".to_string()], 16);

        alice.send_text(&bob.public_identity(), "multi-hop secret").await.unwrap();

        let relay1_raw = relay1.recv_raw().await.unwrap();
        assert!(relay1.handle_incoming(relay1_raw.clone()).await.unwrap().is_none());
        assert!(!contains_bytes(&relay1_raw, b"multi-hop secret"));

        let relay2_raw = relay2.recv_raw().await.unwrap();
        assert!(relay2.handle_incoming(relay2_raw.clone()).await.unwrap().is_none());
        assert!(!contains_bytes(&relay2_raw, b"multi-hop secret"));

        let bob_raw = bob.recv_raw().await.unwrap();
        match bob.handle_incoming(bob_raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { sender, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(sender, alice.node_id());
                assert_eq!(text, "multi-hop secret");
            }
            _ => panic!("expected bob to receive and decrypt alice's message"),
        }
    }

    /// Charlie, uninvolved in the Alice/Bob conversation, cannot decrypt it even though
    /// he receives the exact same raw envelope bytes (e.g. as a relay would).
    #[tokio::test]
    async fn charlie_cannot_decrypt_alice_to_bob_at_the_meshnode_level() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let charlie = make_node(&network, "charlie", vec![], 16);

        alice.send_text(&bob.public_identity(), "private").await.unwrap();
        let raw = bob.recv_raw().await.unwrap();

        // Charlie feeds the exact same bytes Bob received into his own handle_incoming.
        // He's not the recipient, so this is dropped outright -- but even if he were
        // (simulated separately below), he still couldn't decrypt it.
        assert!(charlie.handle_incoming(raw.clone()).await.unwrap().is_none());
    }

    /// Every authenticated field being mutated must invalidate the *outer* envelope
    /// signature (since all of them are part of `Envelope::signed_payload()`), causing
    /// `handle_incoming` to reject the packet before ever attempting to decrypt it.
    /// Covers: modified ciphertext, changed sender, changed recipient, changed message
    /// type, and changed max_hops -- one parameterized-style test per field.
    async fn assert_mutating_envelope_field_is_rejected(mutate: impl FnOnce(&mut Envelope)) {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let raw = bob.recv_raw().await.unwrap();
        let mut envelope: Envelope = bincode::deserialize(&raw).unwrap();
        mutate(&mut envelope);
        let tampered = bincode::serialize(&envelope).unwrap();

        assert!(bob.handle_incoming(tampered).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn modified_ciphertext_is_rejected() {
        assert_mutating_envelope_field_is_rejected(|envelope| envelope.ciphertext[0] ^= 0xFF).await;
    }

    #[tokio::test]
    async fn changed_sender_is_rejected() {
        assert_mutating_envelope_field_is_rejected(|envelope| envelope.sender = [7u8; 32]).await;
    }

    #[tokio::test]
    async fn changed_recipient_is_rejected() {
        assert_mutating_envelope_field_is_rejected(|envelope| envelope.recipient = Some([7u8; 32])).await;
    }

    #[tokio::test]
    async fn changed_message_type_is_rejected() {
        assert_mutating_envelope_field_is_rejected(|envelope| envelope.message_type = MessageType::CallSignal).await;
    }

    #[tokio::test]
    async fn changed_max_hops_is_rejected() {
        assert_mutating_envelope_field_is_rejected(|envelope| envelope.max_hops = envelope.max_hops.wrapping_add(1)).await;
    }

    /// `hops_used` is deliberately *not* part of the signed envelope metadata (every
    /// relay must be able to increment it) -- mutating it must NOT prevent forwarding or
    /// decryption, unlike every other field above.
    #[tokio::test]
    async fn changed_hops_used_still_permits_decryption() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let raw = bob.recv_raw().await.unwrap();
        let mut envelope: Envelope = bincode::deserialize(&raw).unwrap();
        envelope.hops_used += 1;
        let tampered = bincode::serialize(&envelope).unwrap();

        match bob.handle_incoming(tampered).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => assert_eq!(text, "hello"),
            _ => panic!("hops_used mutation should not prevent decryption"),
        }
    }

    /// Builds a hand-crafted `DirectV1` envelope from `sender_identity` to `recipient`,
    /// with a *valid outer envelope signature* but using whatever `header` is passed in
    /// -- letting tests simulate an inner (`DirectCryptoHeaderV1`) problem independently
    /// of the outer envelope-level signature (which would otherwise mask it, since
    /// tampering the ciphertext bytes -- which the header lives inside -- normally
    /// invalidates the outer signature too).
    fn craft_direct_envelope(
        sender_identity: &Identity,
        recipient: &MeshNode<MockTransport>,
        header: DirectCryptoHeaderV1,
        plaintext: &[u8],
    ) -> Vec<u8> {
        let verified_recipient = recipient.public_identity().verify().unwrap();
        let session = Session::establish(sender_identity, &verified_recipient).unwrap();
        let created_at = now_millis();
        let mut message_id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut message_id);
        let aad = DirectMessageAadV1::new(
            PROTOCOL_VERSION,
            message_id,
            sender_identity.node_id(),
            recipient.node_id(),
            MessageType::Chat,
            created_at,
            created_at + 60_000,
            16,
        );
        let direct_ciphertext = encrypt_direct_message(sender_identity, &session, &aad, plaintext);
        let body = DirectEnvelopeBody { header, message: direct_ciphertext };
        let ciphertext = bincode::serialize(&body).unwrap();

        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id,
            sender: sender_identity.node_id(),
            recipient: Some(recipient.node_id()),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::DirectV1,
            created_at,
            expires_at: created_at + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext,
            signature: Vec::new(),
        };
        envelope.signature = sender_identity.sign(&envelope.signed_payload()).to_vec();
        bincode::serialize(&envelope).unwrap()
    }

    /// A header whose X25519 public key doesn't match its own binding signature (e.g. a
    /// relay -- or an attacker -- substituting a different key) must be rejected at the
    /// inner binding-verification step, even though the *outer* envelope signature is
    /// perfectly valid (genuinely produced by the real sender). Covers both "invalid
    /// X25519 binding rejected" and "wrong X25519 public key rejected" -- the same
    /// underlying scenario.
    #[tokio::test]
    async fn invalid_x25519_binding_in_header_is_rejected() {
        let network = MockNetwork::new();
        let alice_identity = Identity::generate();
        let alice_seed = alice_identity.seed();
        let _alice = make_node_with_identity(&network, "alice", vec!["bob".to_string()], 16, alice_identity);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let alice_for_crafting = Identity::from_seed(alice_seed);
        let mallory = Identity::generate();

        let bad_header = DirectCryptoHeaderV1 {
            sender_x25519_public: mallory.x25519_public(), // doesn't match the signature below
            sender_x25519_signature: alice_for_crafting.sign_x25519_public().to_vec(), // signs Alice's real key
        };
        let raw = craft_direct_envelope(&alice_for_crafting, &bob, bad_header, b"hi");

        assert!(bob.handle_incoming(raw).await.unwrap().is_none());
        assert!(
            bob.replay_store.is_empty(),
            "Milestone 2D.1: a valid outer signature with a failed inner decrypt must not be durably recorded"
        );
    }

    /// A structurally malformed header (signature the wrong byte length to even be an
    /// Ed25519 signature) must be rejected gracefully, not panic.
    #[tokio::test]
    async fn malformed_crypto_header_is_rejected() {
        let network = MockNetwork::new();
        let alice_identity = Identity::generate();
        let alice_seed = alice_identity.seed();
        let _alice = make_node_with_identity(&network, "alice", vec!["bob".to_string()], 16, alice_identity);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let alice_for_crafting = Identity::from_seed(alice_seed);

        let malformed_header = DirectCryptoHeaderV1 {
            sender_x25519_public: alice_for_crafting.x25519_public(),
            sender_x25519_signature: vec![0u8; 5], // nowhere near a valid 64-byte signature
        };
        let raw = craft_direct_envelope(&alice_for_crafting, &bob, malformed_header, b"hi");

        assert!(bob.handle_incoming(raw).await.unwrap().is_none());
        assert!(
            bob.replay_store.is_empty(),
            "Milestone 2D.1: a valid outer signature with a failed inner decrypt must not be durably recorded"
        );
    }

    /// The exact same raw bytes fed to `handle_incoming` twice must only ever produce
    /// content once -- the second attempt is deduplicated because the first already
    /// durably persisted it to `self.inbox` (Milestone 3C: durable marking for a `Chat`
    /// message happens automatically once `InboxStore::insert_if_absent` durably
    /// commits it, not via any app-called callback -- see `handle_incoming`'s doc).
    #[tokio::test]
    async fn duplicate_authenticated_message_is_delivered_only_once() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let raw = bob.recv_raw().await.unwrap();
        assert!(matches!(bob.handle_incoming(raw.clone()).await.unwrap(), Some(IncomingEvent::Content(_))), "expected the first delivery to succeed");
        assert!(bob.handle_incoming(raw).await.unwrap().is_none());
    }

    /// Milestone 3C.1: fixed at-rest storage key for tests that persist an
    /// `InboxStore` -- these tests aren't exercising key-management/wrong-key
    /// behavior (that's `inbox_store.rs`'s own unit tests' job), just restart
    /// persistence, so a single shared constant key is fine.
    const TEST_INBOX_STORAGE_KEY: [u8; 32] = [0x42; 32];

    /// Milestone 2D. Unique temp path for a persistent `ReplayStore` used by a single
    /// test -- always under a fresh, test-specific directory (nanosecond timestamp +
    /// atomic counter, not a bare millisecond timestamp, to avoid flaky collisions under
    /// parallel test execution) so parallel test runs never collide with each other's
    /// SQLite files.
    fn replay_store_path(label: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let dir = std::env::temp_dir().join(format!("meshtalk-node-replay-test-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("replay.sqlite")
    }

    /// The Milestone 2D headline guarantee -- supersedes the old, deliberately-red
    /// "known gap" characterization test this replaced (do not let both exist at once,
    /// see the module's git history for why): a legitimately authenticated message,
    /// replayed verbatim *after the recipient restarts*, is now REJECTED, because
    /// replay protection is backed by a durable `ReplayStore` rather than an in-memory
    /// `SeenCache` that reset on every restart.
    #[tokio::test]
    async fn replayed_message_is_rejected_after_restart() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);

        let bob_identity = Identity::generate();
        let bob_seed = bob_identity.seed();
        let path = replay_store_path("rejected-after-restart");

        let captured_raw = {
            let bob = make_node_with_identity_and_replay_store(&network, "bob", vec!["alice".to_string()], 16, bob_identity, &path);
            alice.send_text(&bob.public_identity(), "hello").await.unwrap();
            let raw = bob.recv_raw().await.unwrap();
            assert!(
                matches!(bob.handle_incoming(raw.clone()).await.unwrap(), Some(IncomingEvent::Content(_))),
                "the first, legitimate delivery must succeed"
            );
            // Milestone 3C: durable marking already happened automatically above, the
            // moment `self.inbox` durably persisted the reassembled content -- no
            // app-side callback needed.
            assert!(
                bob.handle_incoming(raw.clone()).await.unwrap().is_none(),
                "an immediate in-process replay must still be deduplicated once durably persisted"
            );
            raw
            // `bob` is dropped here -- simulating the app/process being killed. Its
            // `ReplayStore`, unlike the old `SeenCache`, lives on disk at `path`.
        };

        // Bob "restarts": a brand new `MeshNode`, same identity as before, backed by
        // the *same* `ReplayStore` file -- simulating a real relaunch that reopens its
        // persistent replay-protection database.
        let bob_restarted = make_node_with_identity_and_replay_store(&network, "bob", vec!["alice".to_string()], 16, Identity::from_seed(bob_seed), &path);

        // An attacker (or a relay redelivering a held copy) replays the *exact* raw
        // bytes from the original, already-processed-once delivery.
        assert!(
            bob_restarted.handle_incoming(captured_raw).await.unwrap().is_none(),
            "a replay of an already-processed message must be rejected even after the recipient restarts"
        );

        // And a genuinely new message still gets through -- persistence must not have
        // turned into "reject everything".
        alice.send_text(&bob_restarted.public_identity(), "hello again").await.unwrap();
        let new_raw = bob_restarted.recv_raw().await.unwrap();
        match bob_restarted.handle_incoming(new_raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => assert_eq!(text, "hello again"),
            _ => panic!("a genuinely new message after a restart must still be delivered"),
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Replay protection isn't only for the final recipient -- a pure relay (never the
    /// addressed recipient of anything in this test) must also durably remember which
    /// messages it has already forwarded, so it doesn't forward a replayed copy again
    /// after its own restart. Chain: alice -- relay -- bob (alice and bob are not
    /// directly connected, so bob only ever gets a message via relay forwarding it).
    #[tokio::test]
    async fn relay_does_not_forward_a_replayed_message_after_a_successful_forward_and_restart() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["relay".to_string()], 16);

        let relay_identity = Identity::generate();
        let relay_seed = relay_identity.seed();
        let path = replay_store_path("relay-restart-success");

        let captured_raw = {
            let relay = make_node_with_identity_and_replay_store(
                &network,
                "relay",
                vec!["alice".to_string(), "bob".to_string()],
                16,
                relay_identity,
                &path,
            );
            alice.send_text(&bob.public_identity(), "hello via relay").await.unwrap();
            let raw = relay.recv_raw().await.unwrap();
            assert!(relay.handle_incoming(raw.clone()).await.unwrap().is_none(), "not addressed to the relay itself");
            // Confirms the relay actually, successfully forwarded it on to bob -- only
            // in this case (a genuinely successful forward) should the relay durably
            // remember it, per this file's Milestone 2D.1 doc section.
            let forwarded = bob.recv_raw().await.unwrap();
            assert!(
                matches!(bob.handle_incoming(forwarded).await.unwrap(), Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) if text == "hello via relay"),
                "bob should have received the message via the relay's first, successful forward"
            );
            raw
            // `relay` dropped here -- simulating the relay process being killed.
        };

        // Relay "restarts": same identity, same persistent ReplayStore file.
        let relay_restarted =
            make_node_with_identity_and_replay_store(&network, "relay", vec!["alice".to_string(), "bob".to_string()], 16, Identity::from_seed(relay_seed), &path);

        // The exact same raw bytes are replayed into the restarted relay. Because the
        // relay's *first* forward attempt genuinely succeeded (durably recorded before
        // the restart), this is safe to suppress -- there's no reason to believe bob
        // didn't already get it.
        assert!(
            relay_restarted.handle_incoming(captured_raw).await.unwrap().is_none(),
            "the restarted relay must reject the replay itself"
        );
        let second_forward = tokio::time::timeout(std::time::Duration::from_millis(200), bob.recv_raw()).await;
        assert!(second_forward.is_err(), "a message the relay already successfully forwarded before restarting must not be forwarded again");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Milestone 2D.1's core correction: a relay whose forwarding attempt *fails* (the
    /// network is briefly down, a peer is temporarily unreachable, etc.) must **not**
    /// durably remember the message as handled -- otherwise a legitimate resend of the
    /// exact same authenticated packet (even one delivered after this relay restarts)
    /// would be silently and permanently dropped, having never actually reached anyone.
    /// For an off-grid resilient mesh, risking an occasional duplicate forward is far
    /// less harmful than silently losing a message that was never truly delivered.
    #[tokio::test]
    async fn relay_retries_forwarding_after_a_failed_attempt_even_across_a_restart() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["relay".to_string()], 16);

        let relay_identity = Identity::generate();
        let relay_seed = relay_identity.seed();
        let path = replay_store_path("relay-restart-failure");
        let fail_sends = Arc::new(AtomicBool::new(true));

        let captured_raw = {
            let relay = make_node_with_identity_replay_store_and_failure_flag(
                &network,
                "relay",
                vec!["alice".to_string(), "bob".to_string()],
                16,
                relay_identity,
                &path,
                fail_sends.clone(),
            );
            alice.send_text(&bob.public_identity(), "hello via relay").await.unwrap();
            let raw = relay.recv_raw().await.unwrap();
            // The relay's forward attempt fails (simulated network outage) -- must not
            // be treated as an error the caller needs to react to, and must not be
            // durably recorded.
            assert!(relay.handle_incoming(raw.clone()).await.unwrap().is_none());
            assert!(relay.replay_store.is_empty(), "a failed forward attempt must not be durably recorded as handled");
            raw
            // `relay` dropped here -- simulating the relay process being killed while
            // the network was still down.
        };

        // Relay "restarts": same identity, same persistent ReplayStore file -- and the
        // network is back up now (fail_sends reset).
        fail_sends.store(false, Ordering::SeqCst);
        let relay_restarted = make_node_with_identity_replay_store_and_failure_flag(
            &network,
            "relay",
            vec!["alice".to_string(), "bob".to_string()],
            16,
            Identity::from_seed(relay_seed),
            &path,
            fail_sends,
        );

        // The same packet arrives again (e.g. alice or another relay resends it, having
        // never gotten confirmation it was delivered). Because the first attempt was
        // never durably recorded as successful, the restarted relay is free to retry --
        // and this time it actually reaches bob.
        assert!(relay_restarted.handle_incoming(captured_raw).await.unwrap().is_none());
        let forwarded = tokio::time::timeout(std::time::Duration::from_millis(500), bob.recv_raw())
            .await
            .expect("the restarted relay should have retried forwarding, reaching bob this time")
            .unwrap();
        assert!(
            matches!(bob.handle_incoming(forwarded).await.unwrap(), Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) if text == "hello via relay"),
            "bob should now receive the message the relay was finally able to forward"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Milestone 3B's headline scenario: a relay with three neighbors (b, c, d) whose
    /// flood only *partially* succeeds (b receives it, c and d don't) must not treat
    /// that as "message forwarded" -- c and d must remain independently retryable, and
    /// the message must not be durably marked `seen` until they, too, have received it.
    #[tokio::test]
    async fn relay_partial_flood_success_leaves_only_failed_neighbors_retryable() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay".to_string()], 16);
        let b = make_node(&network, "b", vec!["relay".to_string()], 16);
        let c = make_node(&network, "c", vec!["relay".to_string()], 16);
        let d = make_node(&network, "d", vec!["relay".to_string()], 16);

        let fail_peers = Arc::new(Mutex::new(HashSet::from(["c".to_string(), "d".to_string()])));
        let relay_identity = Identity::generate();
        let relay = make_relay_with_forward_store_and_failing_peers(
            &network,
            "relay",
            vec!["alice".to_string(), "b".to_string(), "c".to_string(), "d".to_string()],
            16,
            relay_identity,
            &replay_store_path("relay-partial-flood-replay"),
            &replay_store_path("relay-partial-flood-forward").with_file_name("relay-partial-flood-forward.sqlite"),
            fail_peers.clone(),
        );

        alice.send_text(&d.public_identity(), "hello via relay").await.unwrap();
        let raw = relay.recv_raw().await.unwrap();
        let envelope: Envelope = bincode::deserialize(&raw).unwrap();
        let message_id = envelope.message_id;
        assert!(relay.handle_incoming(raw).await.unwrap().is_none(), "not addressed to the relay itself");

        // B actually got a copy of the flooded packet (its own send succeeded); C and D
        // did not (their sends were made to fail). B and C/D aren't the message's
        // addressee (only D is), so only raw byte receipt matters here -- what each of
        // them would then do with an envelope addressed to someone else isn't this
        // test's concern.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), b.recv_raw()).await.is_ok(),
            "b should have received the relay's forward"
        );
        assert!(tokio::time::timeout(std::time::Duration::from_millis(150), c.recv_raw()).await.is_err(), "c must not have received anything yet");
        assert!(tokio::time::timeout(std::time::Duration::from_millis(150), d.recv_raw()).await.is_err(), "d must not have received anything yet");

        // Per-neighbor state reflects the partial success precisely.
        assert_eq!(relay.forward.state_of(&message_id, "b"), Some(ForwardState::Forwarded));
        assert_eq!(relay.forward.state_of(&message_id, "c"), Some(ForwardState::Pending));
        assert_eq!(relay.forward.state_of(&message_id, "d"), Some(ForwardState::Pending));
        assert!(
            relay.replay_store.is_empty(),
            "a partially-successful flood must not be durably marked seen while any neighbor is still pending"
        );

        // Network to C and D recovers; a background retry now reaches them both.
        fail_peers.lock().unwrap().clear();
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(relay.retry_pending_forwards().await, 2, "only the two still-pending neighbors should need a retry attempt");

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), c.recv_raw()).await.is_ok(),
            "c should now have received the retried forward"
        );
        let d_raw = tokio::time::timeout(std::time::Duration::from_millis(200), d.recv_raw())
            .await
            .expect("d should now have received the retried forward")
            .unwrap();
        assert!(matches!(d.handle_incoming(d_raw).await.unwrap(), Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) if text == "hello via relay"));

        // Now that every neighbor has finally received it, the relay durably marks it
        // seen -- a later resend of the exact same packet is dropped without
        // re-forwarding to anyone.
        assert!(relay.forward.all_peers_resolved(&message_id));
        assert!(!relay.replay_store.is_empty(), "once every neighbor is resolved, the message must be durably marked seen");
    }

    /// The relay's per-neighbor forwarding state must survive a restart, exactly like
    /// `ReplayStore`/`DeliveryStore`'s -- a neighbor that hadn't received the message
    /// yet when the relay crashed must still get it after the relay comes back up.
    #[tokio::test]
    async fn relay_retains_pending_forward_state_across_a_restart() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["relay".to_string()], 16);
        let c = make_node(&network, "c", vec!["relay".to_string()], 16);

        let fail_peers = Arc::new(Mutex::new(HashSet::from(["c".to_string()])));
        let relay_identity = Identity::generate();
        let relay_seed = relay_identity.seed();
        let replay_path = replay_store_path("relay-forward-restart-replay");
        let forward_path = replay_path.with_file_name("relay-forward-restart-forward.sqlite");

        let (captured_message_id, captured_raw) = {
            let relay = make_relay_with_forward_store_and_failing_peers(
                &network,
                "relay",
                vec!["alice".to_string(), "c".to_string()],
                16,
                relay_identity,
                &replay_path,
                &forward_path,
                fail_peers.clone(),
            );
            alice.send_text(&c.public_identity(), "hello via relay").await.unwrap();
            let raw = relay.recv_raw().await.unwrap();
            let envelope: Envelope = bincode::deserialize(&raw).unwrap();
            let message_id = envelope.message_id;
            assert!(relay.handle_incoming(raw.clone()).await.unwrap().is_none());
            assert_eq!(relay.forward.state_of(&message_id, "c"), Some(ForwardState::Pending));
            assert!(relay.replay_store.is_empty(), "c is still pending -- must not be marked seen yet");
            (message_id, raw)
            // `relay` dropped here -- simulating the relay process being killed while
            // c was still unreachable.
        };

        // Relay "restarts": same identity, same persistent ReplayStore/ForwardStore
        // files -- and the network to c is back up now.
        fail_peers.lock().unwrap().clear();
        let relay_restarted = make_relay_with_forward_store_and_failing_peers(
            &network,
            "relay",
            vec!["alice".to_string(), "c".to_string()],
            16,
            Identity::from_seed(relay_seed),
            &replay_path,
            &forward_path,
            fail_peers,
        );
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(relay_restarted.retry_pending_forwards().await, 1, "only c's still-pending forward should need a retry");

        let forwarded = tokio::time::timeout(std::time::Duration::from_millis(200), c.recv_raw())
            .await
            .expect("c should have received the retried forward after the relay restarted")
            .unwrap();
        assert!(
            matches!(c.handle_incoming(forwarded).await.unwrap(), Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) if text == "hello via relay")
        );
        assert!(relay_restarted.forward.all_peers_resolved(&captured_message_id));
        assert!(!relay_restarted.replay_store.is_empty(), "once c is finally resolved, the message must be durably marked seen");

        // A resend of the exact same original packet is now dropped without touching c
        // again -- the relay has nothing left pending for this message.
        let no_more_forwards = tokio::time::timeout(std::time::Duration::from_millis(150), async {
            relay_restarted.handle_incoming(captured_raw).await.unwrap();
            c.recv_raw().await
        })
        .await;
        assert!(no_more_forwards.is_err(), "a fully-resolved message must not be forwarded again to an already-resolved neighbor");

        let _ = std::fs::remove_dir_all(replay_path.parent().unwrap());
    }

    /// A packet whose outer signature fails verification must never reach the
    /// `ReplayStore` at all -- otherwise an attacker could pre-emptively "poison" the
    /// store with a sender/message_id pair that was never legitimately sent.
    #[tokio::test]
    async fn forged_signature_never_enters_replay_store() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let raw = bob.recv_raw().await.unwrap();
        let mut envelope: Envelope = bincode::deserialize(&raw).unwrap();
        envelope.ciphertext[0] ^= 0xFF; // invalidates the outer signature
        let tampered = bincode::serialize(&envelope).unwrap();

        assert!(bob.handle_incoming(tampered).await.unwrap().is_none());
        assert!(bob.replay_store.is_empty(), "a forged/unverified packet must never be recorded in the replay store");
    }

    /// Structurally malformed input (not even a deserializable envelope) must never
    /// reach the `ReplayStore` either -- it's rejected before any sender/message_id is
    /// even known.
    #[tokio::test]
    async fn malformed_packet_never_enters_replay_store() {
        let network = MockNetwork::new();
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        assert!(bob.handle_incoming(vec![0xFFu8; 40]).await.is_err());
        assert!(bob.replay_store.is_empty(), "malformed input must never be recorded in the replay store");
    }


    /// "Delayed" and "replay" are different things: a message the recipient has *never*
    /// seen before must still be accepted even when the recipient's `ReplayStore` was
    /// just (re)opened from disk (e.g. a real store-and-forward hop delivering it after
    /// the recipient restarted) -- persistence must not accidentally start rejecting
    /// everything. The mobile-level equivalent of this (with real persisted contacts and
    /// a real captured/held packet) is `mesh-mobile`'s
    /// `persisted_offline_contact_survives_restart_and_decrypts_after_capture` test.
    #[tokio::test]
    async fn first_time_message_is_still_delivered_when_recipients_replay_store_was_just_reopened_from_disk() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);

        let path = replay_store_path("first-time-after-reopen");
        // Bob's ReplayStore file already exists (as if from a previous session) but has
        // never seen this particular message before.
        {
            let _ = ReplayStore::open(&path);
        }
        let bob = make_node_with_identity_and_replay_store(&network, "bob", vec!["alice".to_string()], 16, Identity::generate(), &path);

        alice.send_text(&bob.public_identity(), "brand new message").await.unwrap();
        let raw = bob.recv_raw().await.unwrap();
        match bob.handle_incoming(raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => assert_eq!(text, "brand new message"),
            _ => panic!("a genuinely new message must be delivered even from a freshly-(re)opened persistent replay store"),
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A forged packet with an invalid signature must NOT be marked "seen" before its
    /// signature is checked -- otherwise it could pre-emptively consume a message_id and
    /// cause a later *legitimate* packet reusing that same id to be silently
    /// deduplicated away and lost. This directly exercises the ordering documented in
    /// `handle_incoming` (verify signature, *then* check/insert into the seen-cache).
    #[tokio::test]
    async fn fake_unauthenticated_duplicate_does_not_suppress_later_legitimate_message() {
        let network = MockNetwork::new();
        let alice_identity = Identity::generate();
        let alice_seed = alice_identity.seed();
        let _alice = make_node_with_identity(&network, "alice", vec!["bob".to_string()], 16, alice_identity);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let alice_for_crafting = Identity::from_seed(alice_seed);
        let mallory = Identity::generate();

        // Mallory crafts a fake envelope claiming to be from Alice, reusing a specific
        // message_id, but signs it with her own (wrong) identity -- an invalid forgery.
        let shared_message_id = [42u8; 16];
        let mut fake_envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: shared_message_id,
            sender: alice_for_crafting.node_id(), // claims to be Alice
            recipient: Some(bob.node_id()),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::ChannelV1,
            created_at: now_millis(),
            expires_at: now_millis() + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext: vec![1, 2, 3],
            signature: Vec::new(),
        };
        fake_envelope.signature = mallory.sign(&fake_envelope.signed_payload()).to_vec(); // wrong signer
        let fake_raw = bincode::serialize(&fake_envelope).unwrap();

        assert!(bob.handle_incoming(fake_raw).await.unwrap().is_none());

        // Now the real Alice sends a *legitimate* envelope reusing that exact same
        // message_id. It must still be processed -- the fake one must not have
        // poisoned the seen-cache for this id.
        let mut real_envelope = fake_envelope.clone();
        real_envelope.message_id = shared_message_id;
        real_envelope.signature = alice_for_crafting.sign(&real_envelope.signed_payload()).to_vec();
        let real_raw = bincode::serialize(&real_envelope).unwrap();

        // ChannelV1 ciphertext here is garbage (not a real encrypted WirePayload), so
        // this won't decrypt to content -- but the key assertion is that it gets past
        // the dedup check at all (rather than being silently dropped as "already
        // seen"), proving the fake packet didn't consume the id. We confirm this by
        // checking it's *not* rejected for the "already seen" reason: feeding it a
        // second time now correctly *does* get deduplicated (proving the first of this
        // pair was actually processed, not skipped).
        let _ = bob.handle_incoming(real_raw.clone()).await;
        assert!(bob.handle_incoming(real_raw).await.unwrap().is_none(), "second delivery of the real envelope should now be deduplicated");
    }

    /// If the recipient's `PublicIdentity` doesn't verify (e.g. corrupted/mismatched
    /// X25519 key), `send_text` must fail closed -- returning an `Err`, and critically,
    /// never actually transmitting anything (no silent `ChannelV1`/plaintext fallback).
    #[tokio::test]
    async fn recipient_without_valid_encryption_identity_fails_closed_and_sends_nothing() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        let mallory = Identity::generate();
        let mut tampered_bob_identity = bob.public_identity();
        tampered_bob_identity.x25519_public = mallory.x25519_public(); // invalidates the binding

        let result = alice.send_text(&tampered_bob_identity, "hello").await;
        assert!(result.is_err());

        // Nothing was ever transmitted -- bob has nothing waiting for him.
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), bob.recv_raw()).await;
        assert!(outcome.is_err(), "bob should not have received anything after a failed-closed send");
    }

    /// Structural proof that `DirectV1` never falls back to `ChannelV1`: every envelope
    /// `send_text` actually transmits has `encryption_mode == EncryptionMode::DirectV1`,
    /// never the shared-channel scheme, for a successfully-encrypted direct message.
    #[tokio::test]
    async fn direct_v1_send_never_falls_back_to_channel_v1() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.send_text(&bob.public_identity(), "hello").await.unwrap();

        let raw = bob.recv_raw().await.unwrap();
        let envelope: Envelope = bincode::deserialize(&raw).unwrap();
        assert_eq!(envelope.encryption_mode, EncryptionMode::DirectV1);
    }

    /// The scenario that matters most for the eventual store-and-forward architecture:
    /// a message Alice encrypted before "going offline" must still be decryptable by
    /// Bob after his app restarts -- simulated here by reconstructing Bob's `Identity`
    /// purely from its persisted seed (see Milestone 1) and feeding the *exact bytes
    /// Alice originally sent* into the fresh instance. There's no real store-and-forward
    /// queue yet (that's a later milestone) -- this only proves the crypto layer itself
    /// is restart-compatible, which is the prerequisite for that later milestone to work
    /// at all.
    #[tokio::test]
    async fn message_survives_recipient_app_restart() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob_identity = Identity::generate();
        let bob_seed = bob_identity.seed();
        let bob = make_node_with_identity(&network, "bob", vec!["alice".to_string()], 16, bob_identity);

        alice.send_text(&bob.public_identity(), "see you after restart").await.unwrap();
        let raw = bob.recv_raw().await.unwrap();

        // Bob's app restarts: a brand-new MeshNode, but reconstructed from the exact
        // same persisted seed -- same NodeId, same X25519 key, same everything.
        drop(bob);
        let restarted_bob = make_node_with_identity(&network, "bob", vec!["alice".to_string()], 16, Identity::from_seed(bob_seed));

        match restarted_bob.handle_incoming(raw).await.unwrap() {
            Some(IncomingEvent::Content(DeliveredContent { sender, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(sender, alice.node_id());
                assert_eq!(text, "see you after restart");
            }
            _ => panic!("restarted bob should still be able to decrypt alice's message"),
        }
    }

    // ------------------------------------------------------------------------------
    // Milestone 3A: durable delivery engine (send_reliable_text / DeliveryAckV1 /
    // retry_due_deliveries). These exercise the full round trip through the real
    // MeshNode API, not just DeliveryStore in isolation (see delivery_store.rs's own
    // unit tests for that layer).
    // ------------------------------------------------------------------------------

    /// Loops `node.recv_raw()`, processing every packet via `handle_incoming` (so side
    /// effects -- like Bob re-sending an ack for a duplicate -- still happen), until one
    /// whose *envelope* satisfies `predicate` is found -- returns that packet's raw
    /// bytes together with whatever `IncomingEvent` (if any) it produced. Needed
    /// because this project's flood-everyone routing floods a packet to *every*
    /// configured peer regardless of who it's addressed to -- including back to a
    /// two-node pair's original sender -- so a node can legitimately see more than one
    /// packet (e.g. a harmless bounced-back copy of its own earlier traffic) for what a
    /// test considers "one thing happening".
    async fn recv_until(node: &MeshNode<MockTransport>, mut predicate: impl FnMut(&Envelope) -> bool) -> (Vec<u8>, Option<IncomingEvent>) {
        loop {
            let raw = node.recv_raw().await.unwrap();
            let envelope: Envelope = bincode::deserialize(&raw).unwrap();
            let matched = predicate(&envelope);
            let event = node.handle_incoming(raw.clone()).await.unwrap();
            if matched {
                return (raw, event);
            }
        }
    }

    /// Happy path: Alice sends reliably, Bob receives it, durably "accepts" it (acks),
    /// and once Alice processes that ack she has nothing left to retry.
    #[tokio::test]
    async fn alice_sends_bob_acks_and_alice_stops_retrying() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();

        let (_, event) = recv_until(&bob, |e| e.sender == alice.node_id() && e.message_type == MessageType::Chat).await;
        let message_id = match event {
            Some(IncomingEvent::Content(DeliveredContent { message_id, content: ReceivedContent::Text(text), .. })) => {
                assert_eq!(text, "hello");
                message_id
            }
            _ => panic!("bob should have received the message"),
        };

        // Alice may also see a harmless bounced-back copy of her own message before the
        // real ack arrives -- keep processing until her delivery state actually flips.
        recv_until(&alice, |e| e.message_type == MessageType::DeliveryAck).await;
        assert_eq!(alice.delivery.state_of(&message_id), Some(OutboundState::Acknowledged));

        // Nothing left for Alice to retry.
        assert_eq!(alice.retry_due_deliveries().await, 0);
    }

    /// If Bob's ack never reaches Alice, Alice keeps retrying -- and Bob, recognizing
    /// the retry as a duplicate of something he already durably accepted, doesn't show
    /// it again but *does* re-send the ack (see `handle_incoming`'s Milestone 3C doc).
    #[tokio::test]
    async fn lost_ack_causes_alice_to_retry_and_bob_to_reack_without_reshowing_content() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
        let (message_raw, event) = recv_until(&bob, |e| e.sender == alice.node_id() && e.message_type == MessageType::Chat).await;
        let message_id = match event {
            Some(IncomingEvent::Content(DeliveredContent { message_id, .. })) => message_id,
            _ => panic!("bob should have received the message"),
        };
        // Bob's `handle_incoming` call above already durably persisted and acked this
        // message (Milestone 3C) -- no app-side callback needed.
        // The ack Bob just sent (and any harmless bounced-back copy of Alice's own
        // message) is deliberately drained from Alice's inbox WITHOUT processing it --
        // simulating the ack being lost in transit.
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(20), alice.recv_raw()).await {
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }

        // Alice, having received no ack, retries -- resending the *exact same* bytes.
        // Checked against a realistic future timestamp, not `u64::MAX`: `due_for_attempt`
        // also treats a message as expired once `expires_at <= now_ms`, so an
        // absurdly-large `now_ms` would make every message look already-expired.
        assert!(alice.delivery.due_for_attempt(now_millis() + 10_000).iter().any(|m| m.message_id == message_id));
        // No injectable clock exists for this retry engine (by design -- see
        // `delivery_store.rs`), so waiting out the real backoff window is how a test
        // observes "eventually due" without mocking time.
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(alice.retry_due_deliveries().await, 1);

        let (retried_raw, event) = recv_until(&bob, |e| e.message_id == message_id).await;
        assert_eq!(retried_raw, message_raw, "a retry must retransmit the exact original bytes, not a freshly re-encrypted message");
        // Bob sees this as a duplicate of something he already durably accepted --
        // must NOT surface it as a new content event again.
        assert!(event.is_none());

        // But Bob must have re-sent the ack, since the first one apparently never made
        // it -- keep processing Alice's inbox until her delivery state flips.
        recv_until(&alice, |e| e.message_type == MessageType::DeliveryAck).await;
        assert_eq!(alice.delivery.state_of(&message_id), Some(OutboundState::Acknowledged));
    }

    /// A message that fails to transmit at all (simulated network outage) still gets
    /// delivered once the network recovers and a retry attempt is made.
    #[tokio::test]
    async fn message_lost_on_first_attempt_is_eventually_delivered_via_retry() {
        let network = MockNetwork::new();
        let fail_sends = Arc::new(AtomicBool::new(true));
        let alice = make_node_with_identity_replay_store_and_failure_flag(
            &network,
            "alice",
            vec!["bob".to_string()],
            16,
            Identity::generate(),
            &replay_store_path("alice-send-failure"),
            fail_sends.clone(),
        );
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);

        alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
        // Nothing reaches bob -- the send genuinely failed.
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), bob.recv_raw()).await.is_err());

        // Network recovers; a retry attempt now succeeds. Wait out the real backoff
        // window scheduled after the first (failed) attempt -- no injectable clock
        // exists for this retry engine (see `delivery_store.rs`).
        fail_sends.store(false, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(alice.retry_due_deliveries().await, 1);

        let (_, event) = recv_until(&bob, |e| e.sender == alice.node_id() && e.message_type == MessageType::Chat).await;
        match event {
            Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => assert_eq!(text, "hello"),
            _ => panic!("bob should now receive the message"),
        }
    }

    /// If Alice's app crashes after the message was successfully sent (Bob has it) but
    /// before Bob's ack arrives, restarting Alice with the same `DeliveryStore` must
    /// retry using the *same* `message_id` and the *same* encrypted bytes -- never mint
    /// a new logical message for a retry.
    #[tokio::test]
    async fn alice_restart_after_send_before_ack_retries_the_same_message_id_and_bytes() {
        let network = MockNetwork::new();
        let alice_identity = Identity::generate();
        let alice_node_id = alice_identity.node_id();
        let alice_seed = alice_identity.seed();
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let path = replay_store_path("alice-restart-retry");
        let delivery_path = path.with_file_name("alice-delivery.sqlite");

        let (original_message_id, original_raw) = {
            let alice = make_node_with_identity_and_stores(&network, "alice", vec!["bob".to_string()], 16, alice_identity, &path, &delivery_path);
            alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
            let (raw, _) = recv_until(&bob, |e| e.sender == alice_node_id && e.message_type == MessageType::Chat).await;
            let envelope: Envelope = bincode::deserialize(&raw).unwrap();
            (envelope.message_id, raw)
            // `alice` dropped here -- simulating a crash before Bob's ack arrived.
        };

        let alice_restarted =
            make_node_with_identity_and_stores(&network, "alice", vec!["bob".to_string()], 16, Identity::from_seed(alice_seed), &path, &delivery_path);
        // Wait out the real backoff window scheduled after the original (pre-crash)
        // attempt -- no injectable clock exists for this retry engine (see
        // `delivery_store.rs`), so a restart alone doesn't reset it.
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(alice_restarted.retry_due_deliveries().await, 1);

        let (retried_raw, _) = recv_until(&bob, |e| e.message_id == original_message_id).await;
        assert_eq!(retried_raw, original_raw, "the retry after a restart must be byte-identical to the original send");
        let retried_envelope: Envelope = bincode::deserialize(&retried_raw).unwrap();
        assert_eq!(retried_envelope.message_id, original_message_id, "a retry must reuse the same logical message_id");
    }

    /// The critical Milestone 3C crash test: once Bob's `handle_incoming` call has
    /// durably persisted a message (and acked it), that survives a real restart --
    /// chat history is still there, and a resend of the exact same packet is
    /// recognized as a duplicate (never resurfaced to the application layer again),
    /// exactly as if nothing had crashed at all.
    #[tokio::test]
    async fn bob_restart_with_persisted_inbox_deduplicates_a_resend_without_reshowing_it() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob_identity = Identity::generate();
        let bob_seed = bob_identity.seed();
        let replay_path = replay_store_path("bob-inbox-restart-replay");
        let inbox_path = replay_path.with_file_name("bob-inbox-restart-inbox.sqlite");

        let raw = {
            let bob = make_node_with_identity_replay_and_inbox_stores(&network, "bob", vec!["alice".to_string()], 16, bob_identity, &replay_path, &inbox_path);
            alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
            let (raw, event) = recv_until(&bob, |e| e.sender == alice.node_id() && e.message_type == MessageType::Chat).await;
            match event {
                Some(IncomingEvent::Content(DeliveredContent { content: ReceivedContent::Text(text), .. })) => assert_eq!(text, "hello"),
                _ => panic!("bob should have received the message the first time"),
            }
            assert_eq!(bob.inbox_messages().len(), 1, "the message must be durably persisted immediately -- no app callback required");
            raw
            // `bob` dropped here -- simulating a crash right after successful
            // persistence+ack, or at any later point -- it no longer matters, since
            // there is no separate "acknowledge later" step left to crash before.
        };

        let bob_restarted =
            make_node_with_identity_replay_and_inbox_stores(&network, "bob", vec!["alice".to_string()], 16, Identity::from_seed(bob_seed), &replay_path, &inbox_path);
        assert_eq!(bob_restarted.inbox_messages().len(), 1, "durably-accepted chat history must survive a restart");

        // A resend of the exact same packet must not be shown to the app again...
        assert!(bob_restarted.handle_incoming(raw).await.unwrap().is_none());
        assert_eq!(bob_restarted.inbox_messages().len(), 1, "a resend must not create a duplicate inbox entry");
        // ...but Bob still re-acks it (necessary in case the original ack never
        // reached Alice) -- Alice's outbound delivery state reflects this.
        recv_until(&alice, |e| e.message_type == MessageType::DeliveryAck).await;
    }

    /// The other half of Milestone 3C's core guarantee: if the durable inbox write
    /// itself fails (a corrupted/unwritable database), Bob must NOT ack -- an ack must
    /// mean "durably accepted," never merely "decrypted." Alice's own retry loop is
    /// what recovers from this, not any special ack-retry logic on Bob's side.
    #[tokio::test]
    async fn inbox_write_failure_prevents_ack_and_leaves_the_message_retryable() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        bob.inbox.break_for_test();

        alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
        let raw = bob.recv_raw().await.unwrap();
        let envelope: Envelope = bincode::deserialize(&raw).unwrap();
        let message_id = envelope.message_id;

        assert!(bob.handle_incoming(raw).await.unwrap().is_none(), "a failed durable write must not surface content to the application layer");
        assert!(bob.replay_store.is_empty(), "a failed durable write must not be durably marked seen either");

        // No ack was ever sent -- Alice's delivery state must still show it as
        // unacknowledged and due for retry.
        assert_ne!(alice.delivery.state_of(&message_id), Some(OutboundState::Acknowledged));
        assert!(alice.delivery.due_for_attempt(now_millis() + 10_000).iter().any(|m| m.message_id == message_id));
    }


    /// A `DeliveryAck` claiming a `message_id` on behalf of the wrong node must not stop
    /// Alice's retries -- otherwise an attacker (or a misattributed/forged ack) could
    /// convince Alice a message was delivered when it never was.
    #[tokio::test]
    async fn forged_or_misattributed_ack_does_not_stop_retries() {
        let network = MockNetwork::new();
        let alice = make_node(&network, "alice", vec!["bob".to_string(), "mallory".to_string()], 16);
        let bob = make_node(&network, "bob", vec!["alice".to_string()], 16);
        let mallory_identity = Identity::generate();
        let mallory_seed = mallory_identity.seed();
        let _mallory = make_node_with_identity(&network, "mallory", vec!["alice".to_string()], 16, mallory_identity);

        alice.send_reliable_text(&bob.public_identity(), "hello").await.unwrap();
        let (raw, _) = recv_until(&bob, |e| e.sender == alice.node_id() && e.message_type == MessageType::Chat).await;
        let envelope: Envelope = bincode::deserialize(&raw).unwrap();
        let message_id = envelope.message_id;

        // Mallory sends a validly-signed ack (signed by her real identity) claiming the
        // same message_id -- but she is not who Alice actually sent it to.
        let mallory_for_crafting = Identity::from_seed(mallory_seed);
        let forged_ack_payload = WirePayload::DeliveryAck(DeliveryAck { acked_message_id: message_id });
        let forged_raw = craft_direct_envelope_to(&mallory_for_crafting, &alice, MessageType::DeliveryAck, &forged_ack_payload);

        assert!(alice.handle_incoming(forged_raw).await.unwrap().is_none());
        assert_ne!(
            alice.delivery.state_of(&message_id),
            Some(OutboundState::Acknowledged),
            "an ack from the wrong node must not be accepted as delivery confirmation"
        );
        // Wait out the real backoff window scheduled after the original attempt -- no
        // injectable clock exists for this retry engine (see `delivery_store.rs`).
        tokio::time::sleep(std::time::Duration::from_millis(2_600)).await;
        assert_eq!(alice.retry_due_deliveries().await, 1, "alice must still be retrying, since no legitimate ack has arrived");
    }

    /// Builds a hand-crafted, validly-signed `DirectV1` envelope from `sender_identity`
    /// to `recipient`, carrying an arbitrary `WirePayload` -- like `craft_direct_envelope`
    /// above, but for payloads other than a `Chunk` (e.g. a forged `DeliveryAck`).
    fn craft_direct_envelope_to(sender_identity: &Identity, recipient: &MeshNode<MockTransport>, message_type: MessageType, payload: &WirePayload) -> Vec<u8> {
        let verified_recipient = recipient.public_identity().verify().unwrap();
        let session = Session::establish(sender_identity, &verified_recipient).unwrap();
        let plaintext = bincode::serialize(payload).unwrap();
        let created_at = now_millis();
        let mut message_id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut message_id);
        let aad = DirectMessageAadV1::new(
            PROTOCOL_VERSION,
            message_id,
            sender_identity.node_id(),
            recipient.node_id(),
            message_type,
            created_at,
            created_at + 60_000,
            16,
        );
        let direct_ciphertext = encrypt_direct_message(sender_identity, &session, &aad, &plaintext);
        let body = DirectEnvelopeBody { header: DirectCryptoHeaderV1::new(sender_identity), message: direct_ciphertext };
        let ciphertext = bincode::serialize(&body).unwrap();

        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id,
            sender: sender_identity.node_id(),
            recipient: Some(recipient.node_id()),
            message_type,
            encryption_mode: EncryptionMode::DirectV1,
            created_at,
            expires_at: created_at + 60_000,
            max_hops: 16,
            hops_used: 0,
            nonce: [0u8; 24],
            ciphertext,
            signature: Vec::new(),
        };
        envelope.signature = sender_identity.sign(&envelope.signed_payload()).to_vec();
        bincode::serialize(&envelope).unwrap()
    }

    /// Like `make_node_with_identity_and_replay_store`, but also backed by a persistent
    /// `DeliveryStore` -- Milestone 3A tests use this to simulate a *sender* restarting
    /// with its outbound delivery/retry state intact.
    fn make_node_with_identity_and_stores(
        network: &Arc<MockNetwork>,
        name: &str,
        peer_names: Vec<String>,
        max_hops: u8,
        identity: Identity,
        replay_store_path: &std::path::Path,
        delivery_store_path: &std::path::Path,
    ) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register(network, name, peer_names);
        let (replay_store, _) = ReplayStore::open(replay_store_path);
        let (delivery_store, _) = DeliveryStore::open(delivery_store_path);
        MeshNode::new_with_stores(identity, channel_key, transport, max_hops, replay_store, delivery_store, ForwardStore::in_memory(), InboxStore::in_memory())
    }

    /// Milestone 3C: like `make_node_with_identity_and_replay_store`, but also backed
    /// by a persistent `InboxStore` -- tests use this to simulate a *recipient*
    /// restarting with both its replay protection and its durable chat history intact
    /// (the realistic deployment shape: an app persists these together, not one
    /// without the other).
    fn make_node_with_identity_replay_and_inbox_stores(
        network: &Arc<MockNetwork>,
        name: &str,
        peer_names: Vec<String>,
        max_hops: u8,
        identity: Identity,
        replay_store_path: &std::path::Path,
        inbox_store_path: &std::path::Path,
    ) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register(network, name, peer_names);
        let (replay_store, _) = ReplayStore::open(replay_store_path);
        let (inbox_store, _) = InboxStore::open(inbox_store_path, TEST_INBOX_STORAGE_KEY);
        MeshNode::new_with_stores(identity, channel_key, transport, max_hops, replay_store, DeliveryStore::in_memory(), ForwardStore::in_memory(), inbox_store)
    }

    /// Milestone 3B: like `make_node_with_identity_and_replay_store`, but also backed by
    /// a persistent `ForwardStore` and with per-peer send-failure control -- used to
    /// simulate a relay whose flood to some neighbors succeeds while others fail (and,
    /// combined with dropping/recreating the node, a relay restarting with its
    /// per-neighbor forwarding state intact).
    fn make_relay_with_forward_store_and_failing_peers(
        network: &Arc<MockNetwork>,
        name: &str,
        peer_names: Vec<String>,
        max_hops: u8,
        identity: Identity,
        replay_store_path: &std::path::Path,
        forward_store_path: &std::path::Path,
        fail_peers: Arc<Mutex<HashSet<String>>>,
    ) -> MeshNode<MockTransport> {
        let channel_key = ChannelKey::from_passphrase("test-channel");
        let transport = MockTransport::register_with_failing_peers(network, name, peer_names, fail_peers);
        let (replay_store, _) = ReplayStore::open(replay_store_path);
        let (forward_store, _) = ForwardStore::open(forward_store_path);
        MeshNode::new_with_stores(identity, channel_key, transport, max_hops, replay_store, DeliveryStore::in_memory(), forward_store, InboxStore::in_memory())
    }
}

