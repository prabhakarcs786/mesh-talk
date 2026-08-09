//! Content model for messages: plain text or a file attachment (image, video, voice
//! note, or generic file), split into chunks so large attachments can flow through the
//! same relay network as text -- no single UDP datagram, and no single BLE notification,
//! has to carry an entire multi-megabyte file.
//!
//! Every message, however small, goes through this chunking path (a short text message
//! is simply a "transfer" with one chunk) so there's only one code path to get right.

use serde::{Deserialize, Serialize};

use crate::call::AddressedCall;
use crate::identity::NodeId;

/// What's actually encrypted and put in an `Envelope`'s ciphertext: either one chunk of a
/// chat message/file transfer (reassembled via `reassembly.rs`), or a call signaling
/// message/media frame (handled directly, no reassembly -- see `call.rs`).
#[derive(Clone, Serialize, Deserialize)]
pub enum WirePayload {
    Chunk(Chunk),
    Call(AddressedCall),
}


/// What kind of content a transfer carries, mainly so the receiving UI knows how to
/// render it (show an image, a video thumbnail, a voice-note play button, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentKind {
    Text,
    Image,
    Video,
    Voice,
    File,
}

/// Bytes belonging to one chunk of a transfer, plus enough metadata (repeated on every
/// chunk, since it's cheap) to reassemble and label the whole transfer once all chunks
/// have arrived.
#[derive(Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Who this message/attachment is addressed to -- like calls (see `call.rs`), a
    /// direct message to one specific conversation partner rather than a broadcast to
    /// everyone on the channel. Nodes that aren't the target still relay it onward (so
    /// conversations can, in principle, reach someone multiple hops away) but don't
    /// otherwise surface it -- this is what makes per-contact chat threads possible
    /// instead of one shared channel-wide feed. `None` means "everyone on the channel"
    /// (the original broadcast behavior, still used by the `mesh-cli` demo).
    pub target: Option<NodeId>,
    pub transfer_id: [u8; 16],
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub kind: ContentKind,
    /// Present for file attachments; `None` for plain text.
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub data: Vec<u8>,
}

/// Fully reassembled content, ready to hand to the application layer.
pub enum ReceivedContent {
    Text(String),
    File {
        /// Matches the `transfer_id` on the `TransferProgress` updates seen while this
        /// attachment was still arriving, so a UI can correlate a completed attachment
        /// with the progress bar it should now remove.
        transfer_id: [u8; 16],
        name: String,
        mime: String,
        kind: ContentKind,
        data: Vec<u8>,
    },
}

/// Progress toward one transfer, reported either while *sending* (as chunks are handed
/// off to the transport) or while *receiving* (as chunks are reassembled) -- lets a UI
/// show a live "42 / 500 chunks" progress bar instead of an attachment appearing to do
/// nothing until it's 100% complete, which is especially noticeable for larger photos and
/// videos that take a few seconds (or more, over several hops) to fully arrive.
#[derive(Clone, Copy, Debug)]
pub struct TransferProgress {
    pub transfer_id: [u8; 16],
    pub kind: ContentKind,
    pub done_chunks: u32,
    pub total_chunks: u32,
}

impl TransferProgress {
    pub fn is_complete(&self) -> bool {
        self.done_chunks >= self.total_chunks
    }
}

/// Keep individual chunk payloads well under typical network MTUs (~1500 bytes for
/// Ethernet/Wi-Fi) so a chunk fits in one IP packet without fragmentation -- larger
/// chunks were observed to be silently dropped in practice even on loopback. A 200KB
/// image is ~200 chunks at this size; a multi-MB video will be many more (and, since the
/// mesh has no retransmission, less likely to arrive complete over many hops -- expect
/// this to work best for images and short voice notes today).
pub const CHUNK_SIZE: usize = 1024;

/// Splits raw bytes into the chunks for one transfer with a freshly-generated transfer id,
/// addressed to `target` (the one conversation partner who should act on it), or `None`
/// to broadcast to everyone on the channel.
pub fn split_into_chunks(
    target: Option<NodeId>,
    kind: ContentKind,
    file_name: Option<String>,
    mime_type: Option<String>,
    data: &[u8],
) -> Vec<Chunk> {
    use rand::RngCore;
    let mut transfer_id = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut transfer_id);

    let pieces: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(CHUNK_SIZE).collect()
    };
    let chunk_count = pieces.len() as u32;

    pieces
        .into_iter()
        .enumerate()
        .map(|(i, piece)| Chunk {
            target,
            transfer_id,
            chunk_index: i as u32,
            chunk_count,
            kind,
            file_name: file_name.clone(),
            mime_type: mime_type.clone(),
            data: piece.to_vec(),
        })
        .collect()
}
