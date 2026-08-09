//! Reassembly buffer: accumulates chunks belonging to the same transfer (which may
//! arrive out of order, or via different relay paths) until all of them are present,
//! then hands back the complete content. Incomplete transfers are dropped after a
//! timeout so a lost chunk can't leak memory forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::payload::{Chunk, ContentKind, ReceivedContent, TransferProgress};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_IN_FLIGHT_TRANSFERS: usize = 64;

struct PartialTransfer {
    chunk_count: u32,
    kind: ContentKind,
    file_name: Option<String>,
    mime_type: Option<String>,
    received: HashMap<u32, Vec<u8>>,
    last_seen: Instant,
}

/// What accepting one incoming chunk resulted in: either the transfer it belongs to is
/// still incomplete (with a progress snapshot the caller can surface to the UI), or this
/// was the last missing chunk and the whole thing is ready.
pub enum Accepted {
    Progress(TransferProgress),
    Complete(ReceivedContent),
}

pub struct Reassembler {
    transfers: HashMap<[u8; 16], PartialTransfer>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            transfers: HashMap::new(),
        }
    }

    /// Feed in one chunk. Returns `Accepted::Complete` once every chunk for its transfer
    /// has arrived; otherwise buffers it and returns `Accepted::Progress`.
    pub fn accept(&mut self, chunk: Chunk) -> Accepted {
        self.evict_stale();

        let transfer_id = chunk.transfer_id;
        let kind = chunk.kind;
        let total_chunks = chunk.chunk_count;

        let entry = self.transfers.entry(transfer_id).or_insert_with(|| {
            PartialTransfer {
                chunk_count: chunk.chunk_count,
                kind: chunk.kind,
                file_name: chunk.file_name.clone(),
                mime_type: chunk.mime_type.clone(),
                received: HashMap::new(),
                last_seen: Instant::now(),
            }
        });

        entry.last_seen = Instant::now();
        entry.received.insert(chunk.chunk_index, chunk.data);
        let done_chunks = entry.received.len() as u32;

        if done_chunks != entry.chunk_count {
            return Accepted::Progress(TransferProgress {
                transfer_id,
                kind,
                done_chunks,
                total_chunks,
            });
        }

        // Complete! Take ownership and remove the bookkeeping entry.
        let transfer = self.transfers.remove(&transfer_id).expect("just inserted above");
        let mut data = Vec::new();
        for i in 0..transfer.chunk_count {
            if let Some(piece) = transfer.received.get(&i) {
                data.extend_from_slice(piece);
            }
        }

        Accepted::Complete(match transfer.kind {
            ContentKind::Text => ReceivedContent::Text(String::from_utf8_lossy(&data).to_string()),
            kind => ReceivedContent::File {
                transfer_id,
                name: transfer.file_name.unwrap_or_else(|| "file".to_string()),
                mime: transfer.mime_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                kind,
                data,
            },
        })
    }

    fn evict_stale(&mut self) {
        let now = Instant::now();
        self.transfers
            .retain(|_, t| now.duration_since(t.last_seen) < TRANSFER_TIMEOUT);
        // Extra safety valve: if something pathological floods us with transfer ids,
        // don't grow unbounded even within the timeout window.
        if self.transfers.len() > MAX_IN_FLIGHT_TRANSFERS {
            if let Some((&oldest_id, _)) = self.transfers.iter().min_by_key(|(_, t)| t.last_seen) {
                self.transfers.remove(&oldest_id);
            }
        }
    }
}
