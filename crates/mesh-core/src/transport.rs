//! Radio-agnostic transport abstraction. Implement this trait for whatever physical link
//! is available: UDP over LAN (this repo's first implementation, for testing), Bluetooth
//! LE, Wi-Fi Direct, or LoRa on mobile. The mesh routing logic in `node.rs` never needs to
//! know which one is in use.

use async_trait::async_trait;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Send raw bytes to one directly-reachable peer (i.e. within radio range).
    async fn send_to_peer(&self, peer: &str, bytes: Vec<u8>) -> anyhow::Result<()>;

    /// Block until the next raw packet arrives from any peer.
    async fn recv(&self) -> anyhow::Result<Vec<u8>>;

    /// The set of peers currently considered directly reachable ("in range").
    fn peers(&self) -> Vec<String>;
}
