use std::sync::Arc;

use clap::Parser;
use mesh_core::{short_id, ChannelKey, Identity, MeshNode};
use mesh_transport_udp::UdpTransport;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Offline mesh chat demo: run several instances (optionally on different machines on the
/// same LAN) and only give each one its immediate neighbors as `--peers`. Messages still
/// reach every node in the mesh, relayed hop by hop through the nodes in between.
#[derive(Parser)]
struct Args {
    /// Display name shown next to your messages.
    #[arg(long)]
    name: String,

    /// Address to listen on, e.g. 127.0.0.1:9001
    #[arg(long)]
    listen: String,

    /// Comma-separated addresses of directly reachable peers, e.g. 127.0.0.1:9002,127.0.0.1:9003
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Shared channel passphrase; only nodes with the same passphrase can read messages.
    #[arg(long, default_value = "mesh-demo")]
    channel: String,

    /// Max number of hops a message may travel before it is dropped.
    #[arg(long, default_value_t = 16)]
    ttl: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let identity = Identity::generate();
    let channel_key = ChannelKey::from_passphrase(&args.channel);
    let transport = UdpTransport::bind(&args.listen, args.peers.clone()).await?;

    println!("== meshtalk ==");
    println!("name:      {}", args.name);
    println!("node id:   {}", short_id(&identity.node_id()));
    println!("listening: {}", args.listen);
    println!("peers:     {}", if args.peers.is_empty() { "(none)".to_string() } else { args.peers.join(", ") });
    println!("channel:   {}", args.channel);
    println!("type a message and press enter to broadcast it into the mesh. ctrl+c to quit.\n");

    let node = Arc::new(MeshNode::new(identity, channel_key, transport, args.ttl));

    // Task 1: read lines from stdin and broadcast them.
    let node_for_stdin = node.clone();
    let name = args.name.clone();
    let stdin_task = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let payload = format!("{name}: {line}");
            if let Err(e) = node_for_stdin.broadcast_text(&payload).await {
                eprintln!("[warn] failed to broadcast: {e}");
            }
        }
    });

    // Task 2: receive raw packets and hand them to the node for verify/dedup/relay/decrypt.
    let node_for_recv = node.clone();
    let recv_task = tokio::spawn(async move {
        loop {
            match node_for_recv.recv_raw().await {
                Ok(raw) => match node_for_recv.handle_incoming(raw).await {
                    Ok(Some((sender, content))) => match content {
                        mesh_core::ReceivedContent::Text(text) => {
                            println!("[{}] {}", short_id(&sender), text);
                        }
                        mesh_core::ReceivedContent::File { name, mime, data, .. } => {
                            println!(
                                "[{}] (attachment: {name}, {mime}, {} bytes -- can't render in a terminal)",
                                short_id(&sender),
                                data.len()
                            );
                        }
                    },
                    Ok(None) => {} // duplicate, invalid, or not decryptable by us
                    Err(e) => eprintln!("[warn] failed to process incoming packet: {e}"),
                },
                Err(e) => {
                    eprintln!("[warn] recv error: {e}");
                }
            }
        }
    });

    tokio::select! {
        _ = stdin_task => {}
        _ = recv_task => {}
    }

    Ok(())
}
