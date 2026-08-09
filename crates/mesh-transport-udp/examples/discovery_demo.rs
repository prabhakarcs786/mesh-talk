//! Real, working proof that LAN discovery finds peers and computes matching pairing
//! codes on both sides -- run two instances in separate terminals:
//!
//!   cargo run -p mesh-transport-udp --example discovery_demo -- alice
//!   cargo run -p mesh-transport-udp --example discovery_demo -- bob
use mesh_core::{short_id, Identity};
use mesh_transport_udp::LanDiscovery;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let name = std::env::args().nth(1).unwrap_or_else(|| "anon".to_string());
    let identity = Identity::generate();
    println!("{name}: node id = {}", short_id(&identity.node_id()));

    let discovery = LanDiscovery::start(identity.node_id(), name.clone(), 9001).await?;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let peers = discovery.discovered_peers();
        if peers.is_empty() {
            println!("{name}: no peers discovered yet...");
        }
        for peer in peers {
            println!(
                "{name}: found '{}' at {} -- pairing code {}",
                peer.display_name, peer.address, peer.pairing_code
            );
        }
    }
}
