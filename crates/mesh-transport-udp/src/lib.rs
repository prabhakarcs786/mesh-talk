//! A `Transport` implementation over plain UDP sockets on a local network. This exists so
//! the mesh routing logic can be developed and tested on a laptop, over loopback or LAN,
//! before wiring up real short-range radios (Bluetooth LE / Wi-Fi Direct) on mobile.
//!
//! Crucially, each node is only configured with the addresses of its *directly reachable*
//! peers (its simulated "radio range") -- exactly like real nodes in a physical chain can
//! only hear their immediate neighbors. Reaching a distant node still works, but only via
//! relay through the nodes in between, courtesy of `mesh_core::node::MeshNode`'s flood
//! routing.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use mesh_core::Transport;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

pub mod discovery;
pub use discovery::{DiscoveredPeer, LanDiscovery};

pub struct UdpTransport {
    socket: Arc<UdpSocket>,
    peer_addrs: Mutex<HashSet<String>>,
}

impl UdpTransport {
    pub async fn bind(listen_addr: &str, peer_addrs: Vec<String>) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind(listen_addr).await?;
        Ok(Self {
            socket: Arc::new(socket),
            peer_addrs: Mutex::new(peer_addrs.into_iter().collect()),
        })
    }

    pub fn local_addr(&self) -> anyhow::Result<std::net::SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Adds a directly-reachable peer discovered at runtime (e.g. via LAN discovery),
    /// instead of requiring it to be known upfront at `bind()` time. No-op if already
    /// present.
    pub async fn add_peer(&self, addr: String) {
        self.peer_addrs.lock().await.insert(addr);
    }
}

#[async_trait]
impl Transport for UdpTransport {
    async fn send_to_peer(&self, peer: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.socket.send_to(&bytes, peer).await?;
        Ok(())
    }

    async fn recv(&self) -> anyhow::Result<Vec<u8>> {
        let mut buf = vec![0u8; 65_535];
        let (n, _from) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    }

    fn peers(&self) -> Vec<String> {
        // `Transport::peers` is synchronous; try_lock is fine here since flooding just
        // re-broadcasts on every new message anyway, so a momentarily stale/empty list
        // under contention isn't a correctness problem.
        self.peer_addrs
            .try_lock()
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default()
    }
}

