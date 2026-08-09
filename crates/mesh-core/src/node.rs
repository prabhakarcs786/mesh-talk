//! The mesh node: ties identity, channel encryption, de-duplication and a pluggable
//! transport together into flood routing with a TTL hop budget. This is the direct
//! implementation of "chain relay" networking: a message from node A reaches node J ten
//! hops away because every node in between decrements the TTL and re-broadcasts it to its
//! own directly-reachable peers.

use std::sync::Mutex;

use rand::RngCore;

use crate::crypto::ChannelKey;
use crate::identity::{verify, Identity, NodeId};
use crate::message::Envelope;
use crate::store::SeenCache;
use crate::transport::Transport;

pub struct MeshNode<T: Transport> {
    identity: Identity,
    channel_key: ChannelKey,
    seen: Mutex<SeenCache>,
    transport: T,
    default_ttl: u8,
}

impl<T: Transport> MeshNode<T> {
    pub fn new(identity: Identity, channel_key: ChannelKey, transport: T, default_ttl: u8) -> Self {
        Self {
            identity,
            channel_key,
            seen: Mutex::new(SeenCache::new(4096)),
            transport,
            default_ttl,
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    /// Block until the next raw packet arrives on the underlying transport.
    pub async fn recv_raw(&self) -> anyhow::Result<Vec<u8>> {
        self.transport.recv().await
    }

    /// Encrypt, sign and flood a new message onto the mesh.
    pub async fn broadcast(&self, plaintext: &[u8]) -> anyhow::Result<()> {
        let (ciphertext, nonce) = self.channel_key.encrypt(plaintext);
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

    /// Feed one raw incoming packet in. Returns `Some((sender, plaintext))` when it's a new
    /// message meant to be shown to the user. Already-seen or invalid packets return `None`
    /// (they may still have been relayed onward).
    pub async fn handle_incoming(&self, raw: Vec<u8>) -> anyhow::Result<Option<(NodeId, Vec<u8>)>> {
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

        let plaintext = self.channel_key.decrypt(&envelope.ciphertext, &envelope.nonce);
        Ok(plaintext.map(|p| (envelope.sender, p)))
    }
}
