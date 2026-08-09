//! The mesh node: ties identity, channel encryption, de-duplication and a pluggable
//! transport together into flood routing with a TTL hop budget. This is the direct
//! implementation of "chain relay" networking: a message from node A reaches node J ten
//! hops away because every node in between decrements the TTL and re-broadcasts it to its
//! own directly-reachable peers.

use std::sync::Mutex;
use std::time::Duration;

use rand::RngCore;

use crate::crypto::ChannelKey;
use crate::call::{AddressedCall, CallFrame, CallMessage, CallSignal, MediaKind};
use crate::identity::{verify, Identity, NodeId};
use crate::message::Envelope;
use crate::payload::{split_into_chunks, Chunk, ContentKind, ReceivedContent, TransferProgress, WirePayload};
use crate::reassembly::{Accepted, Reassembler};
use crate::store::SeenCache;
use crate::transport::Transport;

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

/// One received chunk's worth of news: either its transfer is still incomplete (with a
/// progress snapshot to show the user), or it was the last chunk needed and the full
/// content is ready. Or, a call-related message/frame addressed to us.
pub enum IncomingEvent {
    Progress(NodeId, TransferProgress),
    Content(NodeId, ReceivedContent),
    /// A call signaling message or media frame from `NodeId`, addressed to us -- see
    /// `call.rs`. Messages addressed to *other* nodes are relayed onward (like anything
    /// else) but never surface as an `IncomingEvent`.
    Call(NodeId, CallMessage),
}

pub struct MeshNode<T: Transport> {
    identity: Identity,
    channel_key: ChannelKey,
    seen: Mutex<SeenCache>,
    reassembler: Mutex<Reassembler>,
    transport: T,
    default_ttl: u8,
}

impl<T: Transport> MeshNode<T> {
    pub fn new(identity: Identity, channel_key: ChannelKey, transport: T, default_ttl: u8) -> Self {
        Self {
            identity,
            channel_key,
            seen: Mutex::new(SeenCache::new(4096)),
            reassembler: Mutex::new(Reassembler::new()),
            transport,
            default_ttl,
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
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

    /// Encrypt, sign and flood a plain-text message to `target` -- a direct message to
    /// one specific conversation partner (like a real chat app), not a broadcast to
    /// everyone on the channel. Other nodes still relay it onward without reading it, so
    /// it can still reach `target` multiple hops away.
    pub async fn send_text(&self, target: NodeId, text: &str) -> anyhow::Result<()> {
        let chunks = split_into_chunks(Some(target), ContentKind::Text, None, None, text.as_bytes());
        self.broadcast_chunks(chunks, |_| {}).await
    }

    /// Broadcasts a plain-text message to everyone on the channel (not addressed to any
    /// one conversation partner) -- used by the `mesh-cli` demo, which doesn't do
    /// per-contact chat threads. Mobile apps use `send_text` instead.
    pub async fn broadcast_text(&self, text: &str) -> anyhow::Result<()> {
        let chunks = split_into_chunks(None, ContentKind::Text, None, None, text.as_bytes());
        self.broadcast_chunks(chunks, |_| {}).await
    }

    /// Encrypt, sign and flood a file attachment (image, video, voice note, or generic
    /// file) to `target`, splitting it into chunks that flow through the same relay path
    /// as any other message. `on_progress` is called after every chunk is handed to the
    /// transport (not just at the end) so the caller can show a live send progress bar
    /// for larger attachments instead of the call appearing to hang until it's done.
    pub async fn send_file(
        &self,
        target: NodeId,
        kind: ContentKind,
        file_name: String,
        mime_type: String,
        data: &[u8],
        on_progress: impl FnMut(TransferProgress),
    ) -> anyhow::Result<()> {
        let chunks = split_into_chunks(Some(target), kind, Some(file_name), Some(mime_type), data);
        self.broadcast_chunks(chunks, on_progress).await
    }

    async fn broadcast_chunks(&self, chunks: Vec<Chunk>, mut on_progress: impl FnMut(TransferProgress)) -> anyhow::Result<()> {
        let total_chunks = chunks.len() as u32;
        let transfer_id = chunks.first().map(|c| c.transfer_id).unwrap_or([0u8; 16]);
        let kind = chunks.first().map(|c| c.kind).unwrap_or(ContentKind::Text);
        let last_index = chunks.len().saturating_sub(1);

        for (i, chunk) in chunks.into_iter().enumerate() {
            self.send_one_payload(&WirePayload::Chunk(chunk)).await?;
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
        self.send_one_payload(&WirePayload::Call(AddressedCall { target, message })).await
    }

    async fn send_one_payload(&self, payload: &WirePayload) -> anyhow::Result<()> {
        let plaintext = bincode::serialize(payload)?;
        let (ciphertext, nonce) = self.channel_key.encrypt(&plaintext);
        let mut id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id);
        let sender = self.identity.node_id();

        let mut envelope = Envelope {
            id,
            sender,
            ttl: self.default_ttl,
            nonce,
            ciphertext,
            signature: Vec::new(),
        };
        envelope.signature = self.identity.sign(&envelope.signed_payload()).to_vec();

        // Mark our own message as seen so we ignore it if it loops back to us.
        self.seen.lock().unwrap().check_and_insert(id);
        self.flood(&envelope).await
    }

    async fn flood(&self, envelope: &Envelope) -> anyhow::Result<()> {
        let bytes = bincode::serialize(envelope)?;
        for peer in self.transport.peers() {
            // Best-effort: one unreachable peer shouldn't stop delivery to the others.
            let _ = self.transport.send_to_peer(&peer, bytes.clone()).await;
        }
        Ok(())
    }

    /// Feed one raw incoming packet in. Returns `Some(IncomingEvent::Content(..))` once a
    /// full message (which may have taken several chunks) is ready to show the user, or
    /// `Some(IncomingEvent::Progress(..))` for a chunk that's part of a still-incomplete
    /// transfer (so the caller can show a live progress bar). Already-seen, invalid, or
    /// undecryptable packets return `None` (they may still have been relayed onward).
    pub async fn handle_incoming(&self, raw: Vec<u8>) -> anyhow::Result<Option<IncomingEvent>> {
        let envelope: Envelope = bincode::deserialize(&raw)?;

        let Ok(sig): Result<[u8; 64], _> = envelope.signature.clone().try_into() else {
            return Ok(None); // malformed signature length, drop silently
        };
        if !verify(&envelope.sender, &envelope.signed_payload(), &sig) {
            return Ok(None); // forged or corrupted, drop silently
        }

        let is_new = self.seen.lock().unwrap().check_and_insert(envelope.id);
        if !is_new {
            return Ok(None); // already processed and relayed once, stop the loop
        }

        // Relay onward first (store-and-forward the hop) regardless of whether we can
        // decrypt the payload ourselves, so nodes without the channel key still act as
        // relays for others.
        if envelope.ttl > 0 {
            let mut relayed = envelope.clone();
            relayed.ttl -= 1;
            self.flood(&relayed).await?;
        }

        let Some(plaintext) = self.channel_key.decrypt(&envelope.ciphertext, &envelope.nonce) else {
            return Ok(None);
        };
        let Ok(payload) = bincode::deserialize::<WirePayload>(&plaintext) else {
            return Ok(None);
        };

        match payload {
            WirePayload::Chunk(chunk) => {
                if let Some(target) = chunk.target {
                    if target != self.identity.node_id() {
                        // Addressed to someone else -- already relayed above, nothing
                        // more to do (this is what keeps a conversation private to its
                        // two participants while still letting it travel multiple hops).
                        return Ok(None);
                    }
                }
                let accepted = self.reassembler.lock().unwrap().accept(chunk);
                Ok(Some(match accepted {
                    Accepted::Progress(p) => IncomingEvent::Progress(envelope.sender, p),
                    Accepted::Complete(c) => IncomingEvent::Content(envelope.sender, c),
                }))
            }
            WirePayload::Call(addressed) => {
                if addressed.target != self.identity.node_id() {
                    // Not for us -- it was already relayed above regardless of target, so
                    // there's nothing more to do (this is how a call can, in principle,
                    // reach a target multiple hops away: every intermediate node relays
                    // it without caring who it's addressed to).
                    return Ok(None);
                }
                Ok(Some(IncomingEvent::Call(envelope.sender, addressed.message)))
            }
        }
    }
}
