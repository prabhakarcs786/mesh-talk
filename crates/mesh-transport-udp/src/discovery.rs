//! LAN auto-discovery: instead of typing in a peer's IP address, devices on the same
//! Wi-Fi network find each other automatically by periodically broadcasting a small
//! announcement packet and listening for others -- the same idea as Bluetooth/Wi-Fi
//! device discovery, but over a UDP broadcast on the local network.
//!
//! Each discovered peer also gets a short numeric "pairing code" that's deterministically
//! derived from both devices' node IDs, so it's identical on both sides without any extra
//! network round-trip -- similar in spirit to Bluetooth's numeric-comparison pairing: the
//! user can glance at both screens and confirm they show the same number before trusting
//! the connection, rather than blindly typing/trusting a bare IP address.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mesh_core::NodeId;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time;

/// Fixed, well-known port used only for discovery broadcasts (separate from each node's
/// own message-relay port, which stays whatever `UdpTransport::bind` was given).
pub const DISCOVERY_PORT: u16 = 45679;

const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
const EXPIRE_AFTER: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct Announcement {
    node_id: NodeId,
    display_name: String,
    /// Port the announcing node's `UdpTransport` is actually listening for messages on.
    service_port: u16,
}

/// A nearby device found via LAN broadcast, ready to be added as a peer with one tap
/// instead of typing its address.
#[derive(Clone, Debug)]
pub struct DiscoveredPeer {
    pub node_id: NodeId,
    pub display_name: String,
    /// `ip:service_port`, ready to pass straight to `UdpTransport::add_peer`.
    pub address: String,
    /// Short numeric code, identical on both devices, for a Bluetooth-style "do these
    /// match?" visual confirmation before connecting.
    pub pairing_code: String,
    last_seen: Instant,
}

pub struct LanDiscovery {
    seen: Arc<Mutex<HashMap<NodeId, DiscoveredPeer>>>,
}

impl LanDiscovery {
    /// Starts broadcasting this node's presence and listening for others. Runs forever in
    /// the background; drop the returned handle (or just let the app exit) to stop.
    pub async fn start(
        local_node_id: NodeId,
        display_name: String,
        service_port: u16,
    ) -> anyhow::Result<Self> {
        // Use socket2 (not tokio's plain UdpSocket::bind) so we can set SO_REUSEADDR/
        // SO_REUSEPORT before binding -- needed so multiple app instances on the *same*
        // machine (e.g. two iOS Simulators during development, or later a hosted test
        // environment) can all listen on the fixed discovery port simultaneously instead
        // of failing with "address already in use".
        let raw_socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        raw_socket.set_reuse_address(true)?;
        #[cfg(unix)]
        raw_socket.set_reuse_port(true)?;
        raw_socket.set_nonblocking(true)?;
        raw_socket.set_broadcast(true)?;
        let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into();
        raw_socket.bind(&bind_addr.into())?;
        let socket = UdpSocket::from_std(raw_socket.into())?;
        let socket = Arc::new(socket);

        let seen: Arc<Mutex<HashMap<NodeId, DiscoveredPeer>>> = Arc::new(Mutex::new(HashMap::new()));

        // Broadcast loop: periodically announce ourselves so others can find us.
        let announce_socket = socket.clone();
        tokio::spawn(async move {
            let announcement = Announcement {
                node_id: local_node_id,
                display_name,
                service_port,
            };
            let Ok(payload) = bincode::serialize(&announcement) else {
                return;
            };
            let destinations = [
                // Real LAN broadcast (reaches other devices on the same Wi-Fi network).
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)), DISCOVERY_PORT),
                // Loopback broadcast, so two app instances on the *same* machine (e.g.
                // two iOS Simulators during development) can also find each other --
                // 255.255.255.255 doesn't reliably loop back to localhost listeners.
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), DISCOVERY_PORT),
            ];
            loop {
                for dest in &destinations {
                    let _ = announce_socket.send_to(&payload, dest).await;
                }
                time::sleep(ANNOUNCE_INTERVAL).await;
            }
        });

        // Listen loop: record announcements from others, keyed by node id.
        let listen_socket = socket.clone();
        let listen_seen = seen.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let Ok((n, from)) = listen_socket.recv_from(&mut buf).await else {
                    continue;
                };
                let Ok(announcement) = bincode::deserialize::<Announcement>(&buf[..n]) else {
                    continue;
                };
                if announcement.node_id == local_node_id {
                    continue; // hearing our own broadcast, ignore
                }

                let address = format!("{}:{}", from.ip(), announcement.service_port);
                let pairing_code = pairing_code(&local_node_id, &announcement.node_id);

                let peer = DiscoveredPeer {
                    node_id: announcement.node_id,
                    display_name: announcement.display_name,
                    address,
                    pairing_code,
                    last_seen: Instant::now(),
                };
                listen_seen.lock().unwrap().insert(announcement.node_id, peer);
            }
        });

        Ok(Self { seen })
    }

    /// Currently visible nearby devices, most-recently-seen first, with anything not
    /// heard from in a while (`EXPIRE_AFTER`) dropped.
    pub fn discovered_peers(&self) -> Vec<DiscoveredPeer> {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        seen.retain(|_, p| now.duration_since(p.last_seen) < EXPIRE_AFTER);
        let mut peers: Vec<_> = seen.values().cloned().collect();
        peers.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        peers
    }
}

/// Deterministic 6-digit code from two node ids -- both sides compute the same value
/// (order-independent), similar to Bluetooth's numeric-comparison pairing display.
fn pairing_code(a: &NodeId, b: &NodeId) -> String {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    let mut input = Vec::with_capacity(64);
    input.extend_from_slice(first);
    input.extend_from_slice(second);
    let hash = blake3::hash(&input);
    let bytes = hash.as_bytes();
    let code = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{code:06}")
}
