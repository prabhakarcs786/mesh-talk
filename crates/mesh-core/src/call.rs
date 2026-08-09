//! Real-time voice/video calling: call signaling (invite/accept/reject/end) and live
//! media frames (audio/video), sent through the same signed + encrypted relay network as
//! chat messages and file attachments -- but *not* through the chunk/reassembly system in
//! `payload.rs`/`reassembly.rs`, since a live call needs each frame delivered (or dropped)
//! as soon as possible, not buffered until every piece of some larger transfer arrives.
//!
//! Unlike chat messages and file attachments (which are meant for everyone on the
//! channel), every call message/frame carries an explicit `target` node id. A node that
//! isn't the target still relays the packet onward as usual (so a call can, in
//! principle, be set up and carried across multiple hops, same as everything else in
//! this mesh) but doesn't otherwise act on it -- this keeps call audio/video from being
//! decoded or surfaced to the UI on every device sharing the channel passphrase, even
//! though (like the rest of the channel) they technically *could* decrypt it.
//!
//! There's no jitter buffer, no forward error correction, and no retransmission here --
//! same trade-off as the rest of this mesh. A dropped frame is just a dropped frame (a
//! brief audio glitch, a skipped video frame); the call keeps going.

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::identity::NodeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Audio,
    Video,
}

/// Call signaling: setting up, answering, or tearing down a call. Small and infrequent,
/// unlike `CallFrame`s (which flow continuously for the duration of a call).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum CallSignal {
    /// "I'm calling you." `video` is whether this is a video call (voice-only otherwise).
    Invite { call_id: [u8; 16], video: bool },
    /// "I'm picking up."
    Accept { call_id: [u8; 16] },
    /// "I'm declining" (or busy).
    Reject { call_id: [u8; 16] },
    /// Hang up -- sent by either side, at any point before or during the call.
    End { call_id: [u8; 16] },
}

impl CallSignal {
    pub fn call_id(&self) -> [u8; 16] {
        match self {
            CallSignal::Invite { call_id, .. }
            | CallSignal::Accept { call_id }
            | CallSignal::Reject { call_id }
            | CallSignal::End { call_id } => *call_id,
        }
    }
}

/// One frame of live audio or video, belonging to a specific call. No reassembly, no
/// retransmission, no acknowledgement.
#[derive(Clone, Serialize, Deserialize)]
pub struct CallFrame {
    pub call_id: [u8; 16],
    pub media: MediaKind,
    /// Monotonically increasing per (call_id, media) so the receiver could detect
    /// drops/reordering if it wants to; not required to play frames back.
    pub sequence: u32,
    pub data: Vec<u8>,
}

/// What's actually carried by an `AddressedCall`: either signaling or one media frame.
#[derive(Clone, Serialize, Deserialize)]
pub enum CallMessage {
    Signal(CallSignal),
    Frame(CallFrame),
}

/// A call-related message together with who it's addressed to -- this is the type that
/// gets encrypted and flooded onto the mesh (see `MeshNode::call_invite` etc).
#[derive(Clone, Serialize, Deserialize)]
pub struct AddressedCall {
    pub target: NodeId,
    pub message: CallMessage,
}

/// A fresh random call id, unique enough to not collide between concurrent/sequential
/// calls to different peers.
pub fn random_call_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut id);
    id
}
