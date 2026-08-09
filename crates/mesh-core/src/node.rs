//! The mesh node: ties identity, channel encryption, de-duplication and a pluggable
//! transport together into flood routing with a TTL hop budget. This is the direct
//! implementation of "chain relay" networking: a message from node A reaches node J ten
//! hops away because every node in between decrements the TTL and re-broadcasts it to its
//! own directly-reachable peers.

use std::sync::Mutex;
use std::time::Duration;

use rand::RngCore;

use crate::crypto::ChannelKey;
use crate::identity::{verify, Identity, NodeId};
use crate::message::Envelope;
use crate::payload::{split_into_chunks, Chunk, ContentKind, ReceivedContent};
use crate::reassembly::Reassembler;
use crate::store::SeenCache;
use crate::transport::Transport;

/// Delay between sending successive chunks of the same transfer -- see the comment in
/// `broadcast_chunks` for why this matters for larger attachments.
const CHUNK_SEND_PACING: Duration = Duration::from_millis(2);

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

    /// Encrypt, sign and flood a plain-text message onto the mesh.
    pub async fn broadcast_text(&self, text: &str) -> anyhow::Result<()> {
        let chunks = split_into_chunks(ContentKind::Text, None, None, text.as_bytes());
        self.broadcast_chunks(chunks).await
    }

    /// Encrypt, sign and flood a file attachment (image, video, voice note, or generic
    /// file) onto the mesh, splitting it into chunks that flow through the same relay
    /// path as any other message.
    pub async fn broadcast_file(
        &self,
        kind: ContentKind,
        file_name: String,
        mime_type: String,
        data: &[u8],
    ) -> anyhow::Result<()> {
        let chunks = split_into_chunks(kind, Some(file_name), Some(mime_type), data);
        self.broadcast_chunks(chunks).await
    }

    async fn broadcast_chunks(&self, chunks: Vec<Chunk>) -> anyhow::Result<()> {
        let last_index = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.into_iter().enumerate() {
            self.broadcast_one_chunk(&chunk).await?;
            // Small pacing delay between chunks: firing hundreds/thousands of UDP
            // datagrams back-to-back in a tight loop (a multi-MB photo or video is
            // thousands of 1KB chunks) can overrun the receiver's socket buffer and the
            // network's own buffering, causing silent drops -- and since reassembly needs
            // *every* chunk to complete (no retransmission), losing even one means the
            // whole attachment never arrives. This was easy to miss with small test files
            // (a few dozen chunks) but reproduces reliably with real multi-MB photos/videos.
            if i != last_index {
                tokio::time::sleep(CHUNK_SEND_PACING).await;
            }
        }
        Ok(())
    }

    async fn broadcast_one_chunk(&self, chunk: &Chunk) -> anyhow::Result<()> {
        let plaintext = bincode::serialize(chunk)?;
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

    /// Feed one raw incoming packet in. Returns `Some((sender, content))` once a full
    /// message (which may have taken several chunks) is ready to show the user.
    /// Already-seen, invalid, or still-incomplete packets return `None` (they may still
    /// have been relayed onward).
    pub async fn handle_incoming(
        &self,
        raw: Vec<u8>,
    ) -> anyhow::Result<Option<(NodeId, ReceivedContent)>> {
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
        let Ok(chunk) = bincode::deserialize::<Chunk>(&plaintext) else {
            return Ok(None);
        };

        let content = self.reassembler.lock().unwrap().accept(chunk);
        Ok(content.map(|c| (envelope.sender, c)))
    }
}
