//! Identity: every node has an Ed25519 keypair. The public key (32 bytes) is the NodeId,
//! used to sign messages so relays and recipients can verify authenticity even though
//! the message may have hopped through several untrusted intermediate nodes.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

pub type NodeId = [u8; 32];
pub type Sig = [u8; 64];

pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    pub fn node_id(&self) -> NodeId {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn sign(&self, data: &[u8]) -> Sig {
        self.signing_key.sign(data).to_bytes()
    }
}

pub fn verify(node_id: &NodeId, data: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(node_id) else {
        return false;
    };
    let signature = Signature::from_bytes(sig);
    vk.verify(data, &signature).is_ok()
}

pub fn short_id(node_id: &NodeId) -> String {
    hex_prefix(node_id, 6)
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    bytes
        .iter()
        .take(n)
        .map(|b| format!("{:02x}", b))
        .collect()
}
