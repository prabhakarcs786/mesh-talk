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
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use mesh_core::Transport;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

pub mod discovery;
pub use discovery::{DiscoveredPeer, LanDiscovery};

/// Larger than the OS default (which can be as little as ~200KB) so a burst of many
/// small chunks from a multi-MB photo/video attachment has room to sit in the kernel's
/// receive queue instead of being dropped before this process gets a chance to read it.
const SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

pub struct UdpTransport {
    socket: Arc<UdpSocket>,
    peer_addrs: Mutex<HashSet<String>>,
}

impl UdpTransport {
    pub async fn bind(listen_addr: &str, peer_addrs: Vec<String>) -> anyhow::Result<Self> {
        let addr = SocketAddr::from_str(listen_addr)?;
        let raw_socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        raw_socket.set_nonblocking(true)?;
        // Best-effort: a platform that refuses a bigger buffer still works, just with
        // more risk of drops under a big burst, so don't fail bind() over it.
        let _ = raw_socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES);
        let _ = raw_socket.set_send_buffer_size(SOCKET_BUFFER_BYTES);
        raw_socket.bind(&addr.into())?;
        let socket = UdpSocket::from_std(raw_socket.into())?;
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

