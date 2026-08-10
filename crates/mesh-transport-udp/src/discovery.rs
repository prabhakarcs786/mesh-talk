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
//!
//! # Milestone 2B.2: identity discovery
//! Announcements also carry the sender's X25519 public key and its Ed25519 binding
//! signature (see `mesh_core::identity::x25519_binding_payload`) -- this is what lets
//! `MeshNode::send_text`/`send_file` ("MeshTalk Direct Encryption v1") actually be used
//! for real mobile chats: without this, a device has no way to obtain a discovered
//! peer's `PublicIdentity` at all. The binding is verified *before* an announcement is
//! ever stored in `discovered_peers()` -- a malformed/invalid one is dropped outright,
//! never surfaced to the caller. This proves cryptographic key ownership only; it does
//! **not** mean the peer has been human-verified (that's a separate, later milestone --
//! see `mesh_core::session::VerificationState`, which deliberately never travels over
//! the wire).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mesh_core::{NodeId, PublicIdentity, X25519Public, PROTOCOL_VERSION};
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
    /// Untrusted, cosmetic name the peer advertises about itself -- see
    /// `mesh_core::session::ContactRecord::advertised_name` for why this is never
    /// treated as verified/trusted.
    display_name: String,
    /// Port the announcing node's `UdpTransport` is actually listening for messages on.
    service_port: u16,
    x25519_public: X25519Public,
    /// Ed25519 signature binding `x25519_public` to `node_id` -- see
    /// `mesh_core::identity::x25519_binding_payload`. Verified before this announcement
    /// is ever stored; see the listen loop in `LanDiscovery::start`.
    x25519_signature: Vec<u8>,
    /// This node's wire-format/session protocol capability -- lets a future version
    /// gracefully handle talking to an older/newer peer instead of just guessing.
    protocol_version: u8,
}

/// A nearby device found via LAN broadcast, ready to be added as a peer with one tap
/// instead of typing its address. `public_identity`'s X25519 binding has *already been
/// verified* by the time a `DiscoveredPeer` exists -- see the listen loop in
/// `LanDiscovery::start`, which drops anything that doesn't verify before it's ever
/// stored.
#[derive(Clone, Debug)]
pub struct DiscoveredPeer {
    pub node_id: NodeId,
    pub display_name: String,
    /// `ip:service_port`, ready to pass straight to `UdpTransport::add_peer`.
    pub address: String,
    /// Short numeric code, identical on both devices, for a Bluetooth-style "do these
    /// match?" visual confirmation before connecting.
    pub pairing_code: String,
    /// This peer's cryptographic identity -- pass to `MeshNode::send_text`/`send_file`
    /// to actually send them a "MeshTalk Direct Encryption v1" message. Already
    /// binding-verified (see the struct doc), but **not** the same as human/contact
    /// verification -- see the module doc.
    pub public_identity: PublicIdentity,
    pub protocol_version: u8,
    last_seen: Instant,
}

pub struct LanDiscovery {
    seen: Arc<Mutex<HashMap<NodeId, DiscoveredPeer>>>,
}

impl LanDiscovery {
    /// Starts broadcasting this node's presence and listening for others. Runs forever in
    /// the background; drop the returned handle (or just let the app exit) to stop.
    pub async fn start(
        local_identity: PublicIdentity,
        display_name: String,
        service_port: u16,
    ) -> anyhow::Result<Self> {
        let local_node_id = local_identity.node_id;
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
                x25519_public: local_identity.x25519_public,
                x25519_signature: local_identity.x25519_signature.clone(),
                protocol_version: PROTOCOL_VERSION,
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

                let public_identity = PublicIdentity {
                    node_id: announcement.node_id,
                    x25519_public: announcement.x25519_public,
                    x25519_signature: announcement.x25519_signature,
                };
                // Reject a malformed/invalid identity outright -- never store it, even
                // temporarily. This proves the X25519 key genuinely belongs to this
                // NodeId's Ed25519 identity; it does not mean the NodeId itself belongs
                // to whoever the display name claims (see the module doc).
                if !public_identity.verify_binding() {
                    log::warn!(
                        "mesh-transport-udp: rejected discovery announcement from {} (node_id={}) -- invalid X25519 binding",
                        from,
                        mesh_core::short_id(&announcement.node_id)
                    );
                    continue;
                }

                let address = format!("{}:{}", from.ip(), announcement.service_port);
                let pairing_code = pairing_code(&local_node_id, &announcement.node_id);
                log::debug!(
                    "mesh-transport-udp: discovered peer node_id={} address={} protocol_version={}",
                    mesh_core::short_id(&announcement.node_id),
                    address,
                    announcement.protocol_version
                );

                let peer = DiscoveredPeer {
                    node_id: announcement.node_id,
                    display_name: announcement.display_name,
                    address,
                    pairing_code,
                    public_identity,
                    protocol_version: announcement.protocol_version,
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
