//! Reassembly buffer: accumulates chunks belonging to the same transfer (which may
//! arrive out of order, or via different relay paths) until all of them are present,
//! then hands back the complete content. Incomplete transfers are dropped after a
//! timeout so a lost chunk can't leak memory forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::payload::{Chunk, ContentKind, ReceivedContent};

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

pub struct Reassembler {
    transfers: HashMap<[u8; 16], PartialTransfer>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            transfers: HashMap::new(),
        }
    }

    /// Feed in one chunk. Returns `Some(content)` once every chunk for its transfer has
    /// arrived; otherwise buffers it and returns `None`.
    pub fn accept(&mut self, chunk: Chunk) -> Option<ReceivedContent> {
        self.evict_stale();

        let entry = self.transfers.entry(chunk.transfer_id).or_insert_with(|| {
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

        if entry.received.len() as u32 != entry.chunk_count {
            return None;
        }

        // Complete! Take ownership and remove the bookkeeping entry.
        let transfer = self.transfers.remove(&chunk.transfer_id)?;
        let mut data = Vec::new();
        for i in 0..transfer.chunk_count {
            data.extend_from_slice(transfer.received.get(&i)?);
        }

        Some(match transfer.kind {
            ContentKind::Text => ReceivedContent::Text(String::from_utf8_lossy(&data).to_string()),
            kind => ReceivedContent::File {
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
