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
                    Ok(Some(mesh_core::IncomingEvent::Content(delivered))) => {
                        let sender = delivered.sender;
                        match delivered.content {
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
                    }},
                    Ok(Some(mesh_core::IncomingEvent::Progress(sender, progress))) => {
                        // Only print occasionally so a multi-thousand-chunk transfer doesn't
                        // spam the terminal with a line per chunk.
                        if progress.done_chunks % 50 == 0 || progress.done_chunks == progress.total_chunks {
                            println!(
                                "[{}] receiving attachment: {}/{} chunks",
                                short_id(&sender),
                                progress.done_chunks,
                                progress.total_chunks
                            );
                        }
                    }
                    Ok(Some(mesh_core::IncomingEvent::Call(sender, message))) => {
                        // The CLI demo has no mic/speaker/camera -- just log that call
                        // signaling/frames are flowing (mesh-mobile is where an actual
                        // call gets placed).
                        match message {
                            mesh_core::CallMessage::Signal(signal) => {
                                println!("[{}] call signal: {:?} (can't place calls from the CLI)", short_id(&sender), signal);
                            }
                            mesh_core::CallMessage::Frame(_) => {} // too noisy to log per-frame
                        }
                    }
                    Ok(None) => {} // duplicate, invalid, or not decryptable by us
                    Err(e) => eprintln!("[warn] failed to process incoming packet: {e}"),
                },
                Err(e) => {
                    eprintln!("[warn] recv error: {e}");
                }
            }
        }
    });

    // Task 3: periodically retry forwarding any relayed message that hasn't yet reached
    // every neighbor known when it first arrived (Milestone 3B -- see
    // `mesh_core::forward_store`'s doc). The CLI demo only ever *originates* best-effort
    // broadcasts (no reliable ACK/retry for those -- see `send_reliable_text`'s doc for
    // why that's a `DirectV1`-only concept), but any node can still be relaying
    // `DirectV1` traffic for other devices on the mesh, so this still matters here.
    let node_for_forward_retry = node.clone();
    let forward_retry_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            node_for_forward_retry.retry_pending_forwards().await;
        }
    });

    tokio::select! {
        _ = stdin_task => {}
        _ = recv_task => {}
        _ = forward_retry_task => {}
    }

    Ok(())
}
