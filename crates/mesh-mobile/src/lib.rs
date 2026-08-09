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
use std::sync::{Arc, Mutex};

use mesh_core::{short_id, ChannelKey, Identity, MeshNode};
use mesh_transport_udp::UdpTransport;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MeshError {
    #[error("network error: {0}")]
    Network(String),
}

/// A single decrypted message received from the mesh, ready to show in a UI.
#[derive(uniffi::Record)]
pub struct ReceivedMessage {
    /// Short hex prefix of the sender's public key -- stable per-device identity.
    pub sender_id: String,
    pub text: String,
}

/// A running mesh node. Construct one per app session; keep it alive for as long as the
/// app wants to stay part of the mesh (e.g. behind a singleton or view-model on the
/// Swift/Kotlin side).
#[derive(uniffi::Object)]
pub struct MeshClient {
    runtime: tokio::runtime::Runtime,
    node: Arc<MeshNode<UdpTransport>>,
    inbox: Arc<Mutex<VecDeque<ReceivedMessage>>>,
    display_name: String,
    node_id: String,
}

const INBOX_CAPACITY: usize = 500;

#[uniffi::export]
impl MeshClient {
    /// Starts a new mesh node.
    ///
    /// - `display_name`: shown alongside your messages.
    /// - `listen_addr`: local address to listen on, e.g. "0.0.0.0:9001".
    /// - `peer_addrs`: addresses of directly-reachable peers, e.g. those on the same
    ///   Wi-Fi hotspot -- this is your simulated "radio range" (see the BLE transport
    ///   roadmap for genuinely radio-range-limited discovery instead of a fixed list).
    /// - `channel_passphrase`: only devices using the same passphrase can read messages.
    /// - `ttl`: max hop count a message can travel before being dropped.
    #[uniffi::constructor]
    pub fn new(
        display_name: String,
        listen_addr: String,
        peer_addrs: Vec<String>,
        channel_passphrase: String,
        ttl: u8,
    ) -> Result<Arc<Self>, MeshError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| MeshError::Network(e.to_string()))?;

        let identity = Identity::generate();
        let node_id = short_id(&identity.node_id());
        let channel_key = ChannelKey::from_passphrase(&channel_passphrase);

        let transport = runtime
            .block_on(UdpTransport::bind(&listen_addr, peer_addrs))
            .map_err(|e| MeshError::Network(e.to_string()))?;

        let node = Arc::new(MeshNode::new(identity, channel_key, transport, ttl));
        let inbox: Arc<Mutex<VecDeque<ReceivedMessage>>> = Arc::new(Mutex::new(VecDeque::new()));

        let recv_node = node.clone();
        let recv_inbox = inbox.clone();
        runtime.spawn(async move {
            loop {
                let raw = match recv_node.recv_raw().await {
                    Ok(raw) => raw,
                    Err(_) => break, // transport closed; stop the background loop
                };
                if let Ok(Some((sender, plaintext))) = recv_node.handle_incoming(raw).await {
                    let message = ReceivedMessage {
                        sender_id: short_id(&sender),
                        text: String::from_utf8_lossy(&plaintext).to_string(),
                    };
                    let mut inbox = recv_inbox.lock().unwrap();
                    inbox.push_back(message);
                    if inbox.len() > INBOX_CAPACITY {
                        inbox.pop_front();
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            runtime,
            node,
            inbox,
            display_name,
            node_id,
        }))
    }

    /// This device's short node id (derived from its public key).
    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    /// Encrypts, signs and floods a message onto the mesh. Returns `false` if it could
    /// not be sent (e.g. no reachable peers right now); the message is not retried.
    pub fn send(&self, text: String) -> bool {
        let payload = format!("{}: {}", self.display_name, text);
        self.runtime
            .block_on(self.node.broadcast(payload.as_bytes()))
            .is_ok()
    }

    /// Non-blocking: returns the next received message if one is waiting, or `None`.
    /// Call this from a UI timer/poll loop (e.g. every 200-500ms) rather than blocking a
    /// UI thread on it.
    pub fn poll_message(&self) -> Option<ReceivedMessage> {
        self.inbox.lock().unwrap().pop_front()
    }
}
