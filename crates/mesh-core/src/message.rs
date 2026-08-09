//! Wire format for a relayed message. Signed so any node along the relay chain (or the
//! recipient) can verify who originally sent it, even though it may pass through several
//! untrusted hops before arriving.

use crate::identity::NodeId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Random id used for de-duplication so the same message isn't relayed forever.
    pub id: [u8; 16],
    pub sender: NodeId,
    /// Hop budget. Decremented at each relay; dropped when it reaches 0.
    pub ttl: u8,
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
    /// Ed25519 signature (64 bytes), stored as Vec<u8> on the wire since serde's derive
    /// only auto-implements fixed-size arrays up to length 32.
    pub signature: Vec<u8>,
}

impl Envelope {
    /// The bytes that are signed: everything except the signature itself.
    pub fn signed_payload(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + 32 + 24 + self.ciphertext.len());
        buf.extend_from_slice(&self.id);
        buf.extend_from_slice(&self.sender);
        buf.extend_from_slice(&self.nonce);
        buf.extend_from_slice(&self.ciphertext);
        buf
    }
}
