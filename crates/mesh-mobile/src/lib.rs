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
use mesh_transport_udp::{LanDiscovery, UdpTransport};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MeshError {
    #[error("network error: {0}")]
    Network(String),
}

/// What kind of attachment a message carries -- mirrors `mesh_core::ContentKind`, kept as
/// a separate type here since UniFFI's derive macros need to be applied where a type is
/// defined, not on a type imported from another crate.
#[derive(uniffi::Enum)]
pub enum AttachmentKind {
    Image,
    Video,
    Voice,
    File,
}

impl From<AttachmentKind> for mesh_core::ContentKind {
    fn from(kind: AttachmentKind) -> Self {
        match kind {
            AttachmentKind::Image => mesh_core::ContentKind::Image,
            AttachmentKind::Video => mesh_core::ContentKind::Video,
            AttachmentKind::Voice => mesh_core::ContentKind::Voice,
            AttachmentKind::File => mesh_core::ContentKind::File,
        }
    }
}

impl From<mesh_core::ContentKind> for AttachmentKind {
    fn from(kind: mesh_core::ContentKind) -> Self {
        match kind {
            mesh_core::ContentKind::Image => AttachmentKind::Image,
            mesh_core::ContentKind::Video => AttachmentKind::Video,
            mesh_core::ContentKind::Voice => AttachmentKind::Voice,
            // Plain text never reaches here (see ReceivedContent::Text handling below);
            // anything else generic falls back to File.
            _ => AttachmentKind::File,
        }
    }
}

/// A file attachment (image, video, voice note, or generic file) carried by a message.
#[derive(uniffi::Record)]
pub struct FileAttachment {
    /// Matches the `transferId` on the `TransferProgressUpdate`s seen while this
    /// attachment was arriving, so the UI can remove the right progress bar once this
    /// shows up in `pollMessage()`.
    pub transfer_id: String,
    pub kind: AttachmentKind,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// A single message received from the mesh, ready to show in a UI. Exactly one of
/// `text`/`attachment` is populated.
#[derive(uniffi::Record)]
pub struct ReceivedMessage {
    /// Full hex-encoded id of the conversation partner this message belongs to (the
    /// sender, since every message the mobile apps send/receive now is a direct message
    /// to/from one specific peer) -- lets the UI group messages into per-contact chat
    /// threads instead of one shared feed.
    pub peer_id: String,
    /// Short hex prefix of the sender's public key -- stable per-device identity.
    pub sender_id: String,
    pub text: Option<String>,
    pub attachment: Option<FileAttachment>,
}

/// Whether a `TransferProgressUpdate` describes a file this device is sending out, or one
/// it's in the middle of receiving.
#[derive(uniffi::Enum)]
pub enum TransferDirection {
    Sending,
    Receiving,
}

/// A live progress snapshot for one in-flight attachment transfer -- poll this
/// periodically (like `pollMessage`) to drive a progress bar instead of the attachment
/// only appearing once it's 100% there. `doneChunks == totalChunks` marks completion (for
/// a received attachment, the fully reassembled `ReceivedMessage` also arrives via
/// `pollMessage` around the same time; for a sent one, this is the only signal you get).
#[derive(uniffi::Record)]
pub struct TransferProgressUpdate {
    /// Stable per-transfer id (hex-encoded), so a UI can track multiple in-flight
    /// transfers -- e.g. sending one photo while still receiving another -- separately.
    pub transfer_id: String,
    pub kind: AttachmentKind,
    pub direction: TransferDirection,
    pub done_chunks: u32,
    pub total_chunks: u32,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Parses a hex string back into a fixed-size byte array (the inverse of `hex_encode`).
/// Returns `None` if the string is the wrong length or contains non-hex characters --
/// e.g. a UI passing back a garbled/edited node id.
fn hex_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Parses the (remote node id, call id) pair used by every call-signaling/frame method.
fn parse_target_and_call(remote_node_id: &str, call_id: &str) -> Option<(mesh_core::NodeId, [u8; 16])> {
    Some((hex_decode::<32>(remote_node_id)?, hex_decode::<16>(call_id)?))
}

/// How many times to (re-)send a call-signaling message like accept/reject/end, and how
/// long to wait between attempts. These are single UDP packets with no acknowledgement,
/// so a lost one otherwise leaves the other side stuck (e.g. still "ringing" after the
/// caller already hung up, with nothing in this app to time that out on its own).
const SIGNAL_RETRY_COUNT: usize = 3;
const SIGNAL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Sends a call signal (accept/reject/end -- never an invite, see below) a few times a
/// short moment apart for reliability.
///
/// Deliberately not used for `Invite`: the receiving side treats *every* incoming invite
/// as a new ring (showing the incoming-call banner, or auto-declining if already on a
/// call), so repeating an invite could look like a second call attempt. Accept/Reject/End
/// don't have that problem -- the receiving side's handling of each is idempotent, so a
/// duplicate that arrives after the first one was already acted on is just a no-op.
async fn send_signal_with_retries(
    node: &MeshNode<UdpTransport>,
    target: mesh_core::NodeId,
    signal: mesh_core::call::CallSignal,
) {
    for attempt in 0..SIGNAL_RETRY_COUNT {
        let result = match signal {
            mesh_core::call::CallSignal::Accept { call_id } => node.call_accept(target, call_id).await,
            mesh_core::call::CallSignal::Reject { call_id } => node.call_reject(target, call_id).await,
            mesh_core::call::CallSignal::End { call_id } => node.call_end(target, call_id).await,
            mesh_core::call::CallSignal::Invite { call_id, video } => node.call_invite(target, call_id, video).await,
        };
        let _ = result;
        if attempt + 1 != SIGNAL_RETRY_COUNT {
            tokio::time::sleep(SIGNAL_RETRY_DELAY).await;
        }
    }
}

fn to_progress_update(
    progress: mesh_core::TransferProgress,
    direction: TransferDirection,
) -> TransferProgressUpdate {
    TransferProgressUpdate {
        transfer_id: hex_encode(&progress.transfer_id),
        kind: progress.kind.into(),
        direction,
        done_chunks: progress.done_chunks,
        total_chunks: progress.total_chunks,
    }
}

/// A nearby device found via LAN auto-discovery, ready to connect to with one tap
/// instead of typing its IP address -- similar to a Bluetooth/Wi-Fi device picker.
#[derive(uniffi::Record)]
pub struct DiscoveredPeer {
    /// Short display id (see `short_id`).
    pub node_id: String,
    /// Full hex-encoded node id -- pass this to `startCall`/`acceptCall`/etc to address
    /// call signaling and media frames to this specific peer.
    pub full_node_id: String,
    pub display_name: String,
    pub address: String,
    /// Short numeric code, identical on both devices, so the user can visually confirm
    /// they're pairing with the right device (Bluetooth-style numeric comparison).
    pub pairing_code: String,
}

/// Whether a `CallFrameUpdate` carries a chunk of audio or a video frame.
#[derive(uniffi::Enum)]
pub enum CallMediaKind {
    Audio,
    Video,
}

impl From<CallMediaKind> for mesh_core::MediaKind {
    fn from(kind: CallMediaKind) -> Self {
        match kind {
            CallMediaKind::Audio => mesh_core::MediaKind::Audio,
            CallMediaKind::Video => mesh_core::MediaKind::Video,
        }
    }
}

impl From<mesh_core::MediaKind> for CallMediaKind {
    fn from(kind: mesh_core::MediaKind) -> Self {
        match kind {
            mesh_core::MediaKind::Audio => CallMediaKind::Audio,
            mesh_core::MediaKind::Video => CallMediaKind::Video,
        }
    }
}

/// What kind of call-signaling news a `CallEvent` carries.
#[derive(uniffi::Enum)]
pub enum CallEventKind {
    /// Someone is calling us.
    IncomingInvite { video: bool },
    /// The other side picked up.
    Accepted,
    /// The other side declined.
    Rejected,
    /// The other side hung up (or the call was cancelled before being answered).
    Ended,
}

/// A call signaling update -- poll this (like `pollMessage`) to drive incoming-call UI,
/// and to know when the other side answers/declines/hangs up.
#[derive(uniffi::Record)]
pub struct CallEvent {
    /// Hex-encoded call id, shared by both sides of this call.
    pub call_id: String,
    /// Full hex node id of the other party -- pass this back to `acceptCall`/
    /// `rejectCall`/`endCall`/`sendCallFrame` to address them.
    pub remote_node_id: String,
    /// Short display id of the other party.
    pub remote_short_id: String,
    pub kind: CallEventKind,
}

/// One frame of live audio or video from an ongoing call -- poll this frequently (e.g.
/// every 20-40ms) from a dedicated playback loop, separately from `pollMessage`/
/// `pollTransferProgress`, since call audio in particular needs low, steady latency.
#[derive(uniffi::Record)]
pub struct CallFrameUpdate {
    pub call_id: String,
    pub remote_node_id: String,
    pub media: CallMediaKind,
    pub sequence: u32,
    pub data: Vec<u8>,
}

/// A running mesh node. Construct one per app session; keep it alive for as long as the
/// app wants to stay part of the mesh (e.g. behind a singleton or view-model on the
/// Swift/Kotlin side).
#[derive(uniffi::Object)]
pub struct MeshClient {
    runtime: tokio::runtime::Runtime,
    node: Arc<MeshNode<UdpTransport>>,
    inbox: Arc<Mutex<VecDeque<ReceivedMessage>>>,
    /// Live progress for in-flight sends and receives -- see `TransferProgressUpdate`.
    progress: Arc<Mutex<VecDeque<TransferProgressUpdate>>>,
    /// Call signaling news (incoming invites, accept/reject/end) -- see `CallEvent`.
    call_events: Arc<Mutex<VecDeque<CallEvent>>>,
    /// Live audio/video frames for ongoing calls -- see `CallFrameUpdate`.
    call_frames: Arc<Mutex<VecDeque<CallFrameUpdate>>>,
    discovery: Mutex<Option<LanDiscovery>>,
    display_name: String,
    node_id: mesh_core::NodeId,
    node_id_str: String,
}

const INBOX_CAPACITY: usize = 500;
/// Bounded like the inbox; progress updates arrive far more often (once per chunk) so
/// this is generous, but a burst of many concurrent transfers still can't grow it
/// unbounded.
const PROGRESS_CAPACITY: usize = 2000;
const CALL_EVENT_CAPACITY: usize = 100;
/// Call frames arrive continuously (dozens per second) while a call is active. Bounded so
/// a UI that falls behind on polling (e.g. backgrounded) doesn't grow this unboundedly --
/// old, stale audio/video frames aren't worth keeping around anyway.
const CALL_FRAME_CAPACITY: usize = 500;

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

        let raw_node_id = identity.node_id();
        let node = Arc::new(MeshNode::new(identity, channel_key, transport, ttl));
        let inbox: Arc<Mutex<VecDeque<ReceivedMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
        let progress: Arc<Mutex<VecDeque<TransferProgressUpdate>>> = Arc::new(Mutex::new(VecDeque::new()));
        let call_events: Arc<Mutex<VecDeque<CallEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let call_frames: Arc<Mutex<VecDeque<CallFrameUpdate>>> = Arc::new(Mutex::new(VecDeque::new()));

        let recv_node = node.clone();
        let recv_inbox = inbox.clone();
        let recv_progress = progress.clone();
        let recv_call_events = call_events.clone();
        let recv_call_frames = call_frames.clone();
        runtime.spawn(async move {
            loop {
                let raw = match recv_node.recv_raw().await {
                    Ok(raw) => raw,
                    Err(_) => break, // transport closed; stop the background loop
                };
                match recv_node.handle_incoming(raw).await {
                    Ok(Some(mesh_core::IncomingEvent::Content(sender, content))) => {
                        let peer_id = hex_encode(&sender);
                        let message = match content {
                            mesh_core::ReceivedContent::Text(text) => ReceivedMessage {
                                peer_id,
                                sender_id: short_id(&sender),
                                text: Some(text),
                                attachment: None,
                            },
                            mesh_core::ReceivedContent::File { transfer_id, name, mime, kind, data } => ReceivedMessage {
                                peer_id,
                                sender_id: short_id(&sender),
                                text: None,
                                attachment: Some(FileAttachment {
                                    transfer_id: hex_encode(&transfer_id),
                                    kind: kind.into(),
                                    name,
                                    mime,
                                    data,
                                }),
                            },
                        };
                        let mut inbox = recv_inbox.lock().unwrap();
                        inbox.push_back(message);
                        if inbox.len() > INBOX_CAPACITY {
                            inbox.pop_front();
                        }
                    }
                    Ok(Some(mesh_core::IncomingEvent::Progress(_sender, p))) => {
                        let mut progress = recv_progress.lock().unwrap();
                        progress.push_back(to_progress_update(p, TransferDirection::Receiving));
                        if progress.len() > PROGRESS_CAPACITY {
                            progress.pop_front();
                        }
                    }
                    Ok(Some(mesh_core::IncomingEvent::Call(sender, message))) => match message {
                        mesh_core::CallMessage::Signal(signal) => {
                            let (call_id, kind) = match signal {
                                mesh_core::CallSignal::Invite { call_id, video } => {
                                    (call_id, CallEventKind::IncomingInvite { video })
                                }
                                mesh_core::CallSignal::Accept { call_id } => (call_id, CallEventKind::Accepted),
                                mesh_core::CallSignal::Reject { call_id } => (call_id, CallEventKind::Rejected),
                                mesh_core::CallSignal::End { call_id } => (call_id, CallEventKind::Ended),
                            };
                            let mut events = recv_call_events.lock().unwrap();
                            events.push_back(CallEvent {
                                call_id: hex_encode(&call_id),
                                remote_node_id: hex_encode(&sender),
                                remote_short_id: short_id(&sender),
                                kind,
                            });
                            if events.len() > CALL_EVENT_CAPACITY {
                                events.pop_front();
                            }
                        }
                        mesh_core::CallMessage::Frame(frame) => {
                            let mut frames = recv_call_frames.lock().unwrap();
                            frames.push_back(CallFrameUpdate {
                                call_id: hex_encode(&frame.call_id),
                                remote_node_id: hex_encode(&sender),
                                media: frame.media.into(),
                                sequence: frame.sequence,
                                data: frame.data,
                            });
                            if frames.len() > CALL_FRAME_CAPACITY {
                                frames.pop_front();
                            }
                        }
                    },
                    Ok(None) => {} // duplicate, invalid, or not decryptable by us
                    Err(_) => {} // malformed packet; drop and keep listening
                }
            }
        });

        Ok(Arc::new(Self {
            runtime,
            node,
            inbox,
            progress,
            call_events,
            call_frames,
            discovery: Mutex::new(None),
            display_name,
            node_id: raw_node_id,
            node_id_str: node_id,
        }))
    }

    /// This device's short node id (derived from its public key).
    pub fn node_id(&self) -> String {
        self.node_id_str.clone()
    }

    /// This device's full hex-encoded node id -- give this to someone (or exchange it via
    /// discovery's `fullNodeId` on `DiscoveredPeer`) so they can address a call to you via
    /// `startCall`.
    pub fn full_node_id(&self) -> String {
        hex_encode(&self.node_id)
    }

    /// Sends a direct text message to `remote_node_id` -- one specific conversation
    /// partner, not a broadcast to everyone on the channel, so per-contact chat threads
    /// work. Returns `false` if `remote_node_id` isn't valid hex, or the send couldn't be
    /// started (e.g. no reachable peers right now); the message is not retried.
    pub fn send(&self, remote_node_id: String, text: String) -> bool {
        let Some(target) = hex_decode::<32>(&remote_node_id) else { return false };
        self.runtime.block_on(self.node.send_text(target, &text)).is_ok()
    }

    /// Sends a direct file attachment (image, video, voice note, or generic file) to
    /// `remote_node_id`, split into chunks so it flows through the same relay path as any
    /// other message. Large attachments (especially video) may not reliably arrive over
    /// many hops -- the mesh has no retransmission -- so this works best for images and
    /// short voice notes.
    ///
    /// Returns immediately (`true` means "accepted and started", not "fully delivered") --
    /// the actual send happens in the background so a large attachment doesn't block the
    /// caller for the several seconds it might take. Poll `pollTransferProgress()` to
    /// track it (the final update, where `doneChunks == totalChunks`, means it's been
    /// fully handed off to the network).
    pub fn send_file(&self, remote_node_id: String, data: Vec<u8>, file_name: String, mime_type: String, kind: AttachmentKind) -> bool {
        let Some(target) = hex_decode::<32>(&remote_node_id) else { return false };
        let node = self.node.clone();
        let progress = self.progress.clone();
        let core_kind: mesh_core::ContentKind = kind.into();
        self.runtime.spawn(async move {
            let progress_for_callback = progress.clone();
            let result = node
                .send_file(target, core_kind, file_name, mime_type, &data, move |p| {
                    let mut progress = progress_for_callback.lock().unwrap();
                    progress.push_back(to_progress_update(p, TransferDirection::Sending));
                    if progress.len() > PROGRESS_CAPACITY {
                        progress.pop_front();
                    }
                })
                .await;
            if result.is_err() {
                // Best-effort: nothing else to do with a send failure since this already
                // runs detached from the caller; the lack of further progress updates is
                // itself the (imperfect) signal that something went wrong.
            }
        });
        true
    }

    /// Non-blocking: returns the next received message if one is waiting, or `None`.
    /// Call this from a UI timer/poll loop (e.g. every 200-500ms) rather than blocking a
    /// UI thread on it.
    pub fn poll_message(&self) -> Option<ReceivedMessage> {
        self.inbox.lock().unwrap().pop_front()
    }

    /// Non-blocking: returns the next transfer-progress update if one is waiting, or
    /// `None`. Call this from the same UI timer/poll loop as `pollMessage` to drive a live
    /// progress bar for sends and receives.
    pub fn poll_transfer_progress(&self) -> Option<TransferProgressUpdate> {
        self.progress.lock().unwrap().pop_front()
    }

    /// Calls another node directly (must already be a reachable peer, e.g. via
    /// discovery/`addPeer`, or reachable through one that is). Returns the new call's
    /// hex-encoded id (hang on to it -- you'll need it for `endCall`/`sendCallFrame`), or
    /// `None` if `remote_node_id` isn't valid hex.
    ///
    /// This only sends the invite; wait for a `CallEvent` with `kind == .accepted` (via
    /// `pollCallEvent`) before starting to send audio/video frames.
    pub fn start_call(&self, remote_node_id: String, video: bool) -> Option<String> {
        let target: mesh_core::NodeId = hex_decode(&remote_node_id)?;
        let call_id = mesh_core::call::random_call_id();
        let node = self.node.clone();
        self.runtime.spawn(async move {
            let _ = node.call_invite(target, call_id, video).await;
        });
        Some(hex_encode(&call_id))
    }

    /// Answers an incoming call (from the `remoteNodeId`/`callId` on the `CallEvent` that
    /// reported it). Start sending audio/video frames right after calling this.
    ///
    /// Sends the signal a few times a short moment apart (like `reject`/`end`) since it's
    /// a single UDP packet with no acknowledgement -- if it's lost, the other side is
    /// stuck showing "ringing" until it times out on its own, which nothing in this app
    /// currently does. Repeating it is cheap and harmless (the receiving side's handling
    /// of `Accepted`/`Rejected`/`Ended` is idempotent -- a repeat after the first one's
    /// already been acted on is simply a no-op).
    pub fn accept_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::Accept { call_id }).await;
        });
        true
    }

    /// Declines an incoming call. See `acceptCall` for why this retries a few times.
    pub fn reject_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::Reject { call_id }).await;
        });
        true
    }

    /// Ends a call in progress, or cancels one that hasn't been answered yet. See
    /// `acceptCall` for why this retries a few times.
    pub fn end_call(&self, remote_node_id: String, call_id: String) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let node = self.node.clone();
        self.runtime.spawn(async move {
            send_signal_with_retries(&node, target, mesh_core::call::CallSignal::End { call_id }).await;
        });
        true
    }

    /// Sends one frame of live audio or video to `remote_node_id` for the given call.
    /// Fire-and-forget, like the rest of the mesh -- no acknowledgement, no retry. Call
    /// this from a dedicated capture loop (e.g. every ~20ms for audio).
    ///
    /// Unlike `sendFile`, this blocks briefly (a single encrypt+sign+socket-send, well
    /// under a millisecond) rather than spawning a detached background task -- frames
    /// must go out in the order the caller sends them, and spawning a separate task per
    /// frame lets the async runtime interleave/reorder them across worker threads
    /// (observed directly: frames arrived out of order in testing once this used
    /// `runtime.spawn` per frame instead of blocking).
    pub fn send_call_frame(
        &self,
        remote_node_id: String,
        call_id: String,
        media: CallMediaKind,
        sequence: u32,
        data: Vec<u8>,
    ) -> bool {
        let Some((target, call_id)) = parse_target_and_call(&remote_node_id, &call_id) else { return false };
        let core_media: mesh_core::MediaKind = media.into();
        self.runtime
            .block_on(self.node.send_call_frame(target, call_id, core_media, sequence, data))
            .is_ok()
    }

    /// Non-blocking: returns the next call signaling update (incoming invite, or the
    /// other side accepting/rejecting/ending), or `None`. Call this from a UI timer/poll
    /// loop, same cadence as `pollMessage`.
    pub fn poll_call_event(&self) -> Option<CallEvent> {
        self.call_events.lock().unwrap().pop_front()
    }

    /// Non-blocking: returns the next incoming audio/video frame for an active call, or
    /// `None`. Call this from a dedicated, frequently-polled loop (e.g. every ~20ms)
    /// separate from `pollMessage`/`pollTransferProgress`, since call audio especially
    /// needs steady, low latency.
    pub fn poll_call_frame(&self) -> Option<CallFrameUpdate> {
        self.call_frames.lock().unwrap().pop_front()
    }

    /// Starts broadcasting this device's presence on the local Wi-Fi network and
    /// listening for others -- like turning on Bluetooth/Wi-Fi discovery, instead of
    /// requiring the user to type in an IP address. Safe to call more than once (only the
    /// first call actually starts it).
    pub fn start_discovery(&self) -> Result<(), MeshError> {
        let mut discovery = self.discovery.lock().unwrap();
        if discovery.is_some() {
            return Ok(()); // already running
        }
        let port = self
            .node
            .transport()
            .local_addr()
            .map_err(|e| MeshError::Network(e.to_string()))?
            .port();
        let started = self
            .runtime
            .block_on(LanDiscovery::start(self.node_id, self.display_name.clone(), port))
            .map_err(|e| MeshError::Network(e.to_string()))?;
        *discovery = Some(started);
        Ok(())
    }

    /// Nearby devices found via LAN discovery, ready to connect to with one tap. Call
    /// `start_discovery()` first; call this from a UI timer/poll loop (e.g. every 1-2s).
    pub fn discovered_peers(&self) -> Vec<DiscoveredPeer> {
        let discovery = self.discovery.lock().unwrap();
        let Some(discovery) = discovery.as_ref() else {
            return Vec::new();
        };
        discovery
            .discovered_peers()
            .into_iter()
            .map(|p| DiscoveredPeer {
                node_id: short_id(&p.node_id),
                full_node_id: hex_encode(&p.node_id),
                display_name: p.display_name,
                address: p.address,
                pairing_code: p.pairing_code,
            })
            .collect()
    }

    /// Adds a discovered (or manually entered) peer address as a directly-reachable relay
    /// target, without needing to reconnect/restart the client.
    pub fn add_peer(&self, address: String) {
        self.runtime.block_on(self.node.transport().add_peer(address));
    }
}

