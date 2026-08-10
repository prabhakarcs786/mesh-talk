//! Wire format for a relayed message ("envelope v2"). Signed so any node along the relay
//! chain (or the recipient) can verify who originally sent it and that its authenticated
//! metadata hasn't been tampered with, even though it may pass through several untrusted
//! hops before arriving.
//!
//! # Why a versioned, richer envelope
//! The original envelope only carried an id, sender, ttl, and ciphertext. That made it
//! impossible to add fields later without breaking every node that didn't understand the
//! new shape, and left `ttl` mutable-but-signed (see "Hop-limit integrity" below) and
//! addressing buried inside the encrypted payload (invisible to relays without the
//! channel key). This version fixes both and adds `protocol_version` so future wire
//! format changes have somewhere to hang a compatibility check instead of nodes silently
//! misinterpreting bytes built for a different version.
//!
//! # Metadata privacy trade-off
//! `recipient` is visible (and authenticated) to every relay, not just the final
//! recipient -- this is what lets a future routing layer forward toward a chosen next
//! hop instead of flooding everyone. It means any relay (or passive eavesdropper on the
//! shared channel) can see *who is talking to whom*, even without the channel key to read
//! the content. That's a deliberate, documented trade-off for this phase, not an
//! oversight -- sealed-sender-style recipient hiding is a possible future improvement,
//! not yet implemented. Confidentiality of content still depends on the channel key
//! becoming per-recipient (see `crypto.rs`) -- this change alone does not make chats
//! private, it only makes routing metadata legible.
//!
//! # Hop-limit integrity
//! `max_hops` is part of the signed (immutable) metadata: a malicious relay can no longer
//! rewrite the sender's declared hop budget upward (e.g. turning `max_hops = 1` into
//! `max_hops = 255`) without invalidating the signature -- that was possible with the
//! previous mutable-and-signed `ttl` field, which is the concrete bug this replaces.
//! `hops_used` is deliberately *not* signed, since every relay must be able to increment
//! it.
//!
//! **This is routing safety against normal/buggy nodes, not a security boundary against a
//! malicious relay.** A malicious relay can still reset `hops_used` back to `0` (or any
//! lower value) on a packet it forwards, since nothing binds "hops actually travelled" to
//! anything the sender or earlier relays signed -- there is no per-hop cryptographic
//! proof (that would need each relay to add its own signature/counter, which is future
//! work and not implemented here). Treat `max_hops`/`hops_used` as protection against
//! routing loops and runaway flooding from well-behaved nodes, not as a guarantee against
//! an adversarial one deliberately extending a message's reach.
//!
//! # Metadata privacy: future work, not solved here
//! Beyond the `recipient` trade-off above, nothing here hides *who is on the mesh at
//! all* or *how often two nodes talk*, even from a passive observer without the channel
//! key. Possible future improvements, none implemented yet:
//! - rotating recipient identifiers (so the same conversation doesn't always show the
//!   same `NodeId` pair to relays)
//! - session aliases (a per-conversation pseudonym instead of the long-term `NodeId`)
//! - anonymous routing tokens (sealed-sender-style, hiding `recipient` from relays
//!   entirely and only revealing it to the intended destination)

use serde::{Deserialize, Serialize};

use crate::identity::NodeId;

/// Bumped whenever the wire format changes incompatibly. A node that receives an
/// envelope with a `protocol_version` it doesn't understand drops it rather than guess
/// at a shape it was never built to parse -- see `Envelope::is_supported_version`.
///
/// v3 added `encryption_mode` (see [`EncryptionMode`]), making which crypto scheme
/// produced `ciphertext` an explicit, authenticated part of the envelope instead of
/// something a future reader would have to infer from `recipient.is_some()`.
pub const PROTOCOL_VERSION: u8 = 3;

/// How far a sender's clock is allowed to disagree with ours before we start treating its
/// `expires_at` as meaningful -- phones don't all have perfectly synchronized clocks, and
/// without this a device whose clock is a few minutes fast could have its perfectly valid
/// messages dropped by every relay as "already expired". See `Envelope::is_expired`.
pub const CLOCK_SKEW_TOLERANCE_MS: u64 = 5 * 60 * 1000;

/// What crypto scheme produced an envelope's `ciphertext` -- authenticated (signed) and
/// visible without decrypting, so relays/recipients don't have to guess based on
/// `recipient.is_some()` (which will matter once group messaging exists and
/// `recipient: None` could mean either "old broadcast" or "new group message"). An
/// envelope with an encryption mode this node doesn't recognize fails closed: bincode
/// fails to deserialize an out-of-range enum discriminant into this type at all, so
/// `Envelope` deserialization itself returns an `Err` rather than silently defaulting to
/// some mode -- see `MeshNode::handle_incoming`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptionMode {
    /// Today's original scheme: a shared passphrase-derived `ChannelKey` (see
    /// `crypto.rs`) that everyone on the channel can decrypt -- used for broadcasts
    /// (`recipient: None`) and for call signaling/frames (which still use this mode; see
    /// `MeshNode::send_call`).
    ChannelV1,
    /// "MeshTalk Direct Encryption v1" (see `direct_crypto.rs`): per-recipient
    /// authenticated encryption using session keys derived from the sender's and
    /// recipient's X25519 identities. Used for direct chat messages (`MeshNode::send_text`/
    /// `send_file`) addressed to a specific recipient.
    DirectV1,
}

impl EncryptionMode {
    fn discriminant(self) -> u8 {
        match self {
            EncryptionMode::ChannelV1 => 0,
            EncryptionMode::DirectV1 => 1,
        }
    }
}

/// What kind of payload an envelope carries -- authenticated and visible without
/// decrypting the ciphertext, so a future QoS/priority layer (e.g. "call signaling beats
/// a bulk file chunk") can tell them apart without needing the channel key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// A chat message or file-attachment chunk (see `payload.rs`).
    Chat,
    /// Call invite/accept/reject/end signaling (see `call.rs`).
    CallSignal,
    /// One frame of live call audio/video (see `call.rs`).
    CallFrame,
    /// Milestone 3A: a `DeliveryAck` (see `payload.rs`) -- authenticated confirmation
    /// that a specific `(sender, message_id)` was durably accepted by its recipient.
    DeliveryAck,
}

impl MessageType {
    /// Stable, frozen wire-format discriminant -- also reused by `direct_crypto.rs`'s
    /// authenticated-data encoding (see `direct_crypto::encode_aad_v1`), so both places
    /// stay in sync from a single source of truth instead of risking two independent
    /// (and possibly drifting) copies of this mapping.
    pub(crate) fn discriminant(self) -> u8 {
        match self {
            MessageType::Chat => 0,
            MessageType::CallSignal => 1,
            MessageType::CallFrame => 2,
            MessageType::DeliveryAck => 3,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Wire format version this envelope was built with -- see `PROTOCOL_VERSION`.
    pub protocol_version: u8,
    /// Random id used for de-duplication (and, in a future milestone, to correlate
    /// acks/retries) so the same message isn't relayed forever in a loop.
    pub message_id: [u8; 16],
    pub sender: NodeId,
    /// Who this is addressed to; `None` means "everyone on the channel" (the original
    /// broadcast behavior, still used by the `mesh-cli` demo). See the module doc's
    /// "Metadata privacy trade-off" section.
    pub recipient: Option<NodeId>,
    /// What kind of payload the ciphertext carries -- see `MessageType`.
    pub message_type: MessageType,
    /// What crypto scheme `ciphertext` was produced with -- see `EncryptionMode`.
    pub encryption_mode: EncryptionMode,
    /// Unix milliseconds when the sender created this envelope.
    pub created_at: u64,
    /// Unix milliseconds after which this envelope is considered stale: relays drop it
    /// instead of forwarding it further (see `MeshNode::handle_incoming`). Full
    /// store-and-forward (holding a message until its destination is reachable) is a
    /// later milestone; this only bounds how long a live-flood message keeps circulating.
    pub expires_at: u64,
    /// Sender-authenticated hop budget -- see the module doc's "Hop-limit integrity"
    /// section. Can't be tampered upward by a relay without invalidating the signature.
    pub max_hops: u8,
    /// How many hops this envelope has actually travelled so far. Deliberately *not*
    /// covered by the signature (every relay must be able to increment it) -- see the
    /// module doc.
    pub hops_used: u8,
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
    /// Ed25519 signature (64 bytes), stored as Vec<u8> on the wire since serde's derive
    /// only auto-implements fixed-size arrays up to length 32.
    pub signature: Vec<u8>,
}

impl Envelope {
    /// The bytes that are signed -- everything the sender authenticates. Deliberately
    /// excludes `hops_used`, which every relay must be able to mutate; see the module
    /// doc's "Hop-limit integrity" section.
    pub fn signed_payload(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 16 + 32 + 33 + 2 + 8 + 8 + 1 + 24 + self.ciphertext.len());
        buf.push(self.protocol_version);
        buf.extend_from_slice(&self.message_id);
        buf.extend_from_slice(&self.sender);
        match self.recipient {
            Some(recipient) => {
                buf.push(1);
                buf.extend_from_slice(&recipient);
            }
            None => buf.push(0),
        }
        buf.push(self.message_type.discriminant());
        buf.push(self.encryption_mode.discriminant());
        buf.extend_from_slice(&self.created_at.to_le_bytes());
        buf.extend_from_slice(&self.expires_at.to_le_bytes());
        buf.push(self.max_hops);
        buf.extend_from_slice(&self.nonce);
        buf.extend_from_slice(&self.ciphertext);
        buf
    }

    /// Whether this envelope's declared hop budget has been used up -- relays should
    /// stop forwarding once this is true (the intended recipient can still process it).
    /// See the module doc's "Hop-limit integrity" section for what this does and does
    /// not protect against.
    pub fn hop_budget_exhausted(&self) -> bool {
        self.hops_used >= self.max_hops
    }

    /// Whether this envelope is from a wire format this node understands. Only one
    /// version exists today, so this is always true in practice -- it exists so a future
    /// breaking format change has a place to plug in a real check instead of nodes
    /// silently misparsing bytes built for a different version.
    pub fn is_supported_version(&self) -> bool {
        self.protocol_version == PROTOCOL_VERSION
    }

    /// Whether this envelope should be treated as stale given the current time (unix
    /// milliseconds). Allows `CLOCK_SKEW_TOLERANCE_MS` of slack so a sender/relay whose
    /// clock runs a little fast or slow doesn't have otherwise-valid messages dropped --
    /// see the module-level `CLOCK_SKEW_TOLERANCE_MS` doc.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms > self.expires_at.saturating_add(CLOCK_SKEW_TOLERANCE_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn sample_envelope(sender: &Identity, max_hops: u8, hops_used: u8) -> Envelope {
        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: [1u8; 16],
            sender: sender.node_id(),
            recipient: Some([2u8; 32]),
            message_type: MessageType::Chat,
            encryption_mode: EncryptionMode::ChannelV1,
            created_at: 1_000,
            expires_at: 2_000,
            max_hops,
            hops_used,
            nonce: [3u8; 24],
            ciphertext: vec![9, 9, 9],
            signature: Vec::new(),
        };
        envelope.signature = sender.sign(&envelope.signed_payload()).to_vec();
        envelope
    }

    fn verify_envelope(envelope: &Envelope) -> bool {
        let Ok(sig): Result<[u8; 64], _> = envelope.signature.clone().try_into() else {
            return false;
        };
        crate::identity::verify(&envelope.sender, &envelope.signed_payload(), &sig)
    }

    #[test]
    fn valid_envelope_verifies() {
        let sender = Identity::generate();
        let envelope = sample_envelope(&sender, 16, 0);
        assert!(verify_envelope(&envelope));
    }

    #[test]
    fn incrementing_hops_used_does_not_invalidate_signature() {
        // A relay must be able to bump hops_used without the sender's signature over
        // *the rest of the envelope* becoming invalid -- hops_used isn't signed.
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        envelope.hops_used += 1;
        assert!(verify_envelope(&envelope));
    }

    #[test]
    fn malicious_relay_resetting_hops_used_does_not_invalidate_signature() {
        // Documents a known, accepted limitation (see the module doc's "Hop-limit
        // integrity" section): a malicious relay CAN reset hops_used back down to let a
        // message flood further than the sender intended, and the signature still
        // verifies, since hops_used was deliberately left mutable/unsigned. This is
        // routing safety against normal nodes, not a security boundary against an
        // adversarial relay -- this test exists so that limitation stays documented and
        // intentional rather than being "discovered" later as a surprise.
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 5, 4);
        envelope.hops_used = 0;
        assert!(verify_envelope(&envelope));
    }

    #[test]
    fn tampering_with_max_hops_invalidates_signature() {
        // This is the concrete exploit the redesign closes: a relay rewriting the
        // sender's declared hop budget upward must invalidate the signature.
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 1, 0);
        envelope.max_hops = 255;
        assert!(!verify_envelope(&envelope));
    }

    #[test]
    fn tampering_with_recipient_invalidates_signature() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        envelope.recipient = Some([7u8; 32]);
        assert!(!verify_envelope(&envelope));
    }

    #[test]
    fn tampering_with_encryption_mode_invalidates_signature() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        envelope.encryption_mode = EncryptionMode::DirectV1;
        assert!(!verify_envelope(&envelope));
    }

    #[test]
    fn hop_budget_exhausted_reflects_hops_used_vs_max_hops() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 2, 0);
        assert!(!envelope.hop_budget_exhausted());
        envelope.hops_used = 1;
        assert!(!envelope.hop_budget_exhausted());
        envelope.hops_used = 2;
        assert!(envelope.hop_budget_exhausted());
    }

    #[test]
    fn unsupported_version_is_detected() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        envelope.protocol_version = PROTOCOL_VERSION + 1;
        assert!(!envelope.is_supported_version());
    }

    /// `bincode` encodes a fieldless enum's variant as a leading `u32` index by default.
    /// `EncryptionMode` only has 2 variants (0, 1) today -- an out-of-range index (e.g. a
    /// future/unknown mode, or corrupted data) must fail to deserialize rather than
    /// silently landing on some variant or panicking. This is what makes "unknown
    /// encryption modes fail closed" true at the wire-format level, not just by
    /// convention.
    #[test]
    fn deserializing_out_of_range_encryption_mode_discriminant_fails_safely() {
        let bytes = bincode::serialize(&99u32).unwrap();
        let result: Result<EncryptionMode, _> = bincode::deserialize(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn expiry_check_allows_a_sender_clock_running_fast_within_tolerance() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        // The sender's clock is fast enough that expires_at is still in the future from
        // our perspective right up to the tolerance boundary -- must NOT be dropped.
        envelope.expires_at = 10_000;
        let now = envelope.expires_at + CLOCK_SKEW_TOLERANCE_MS;
        assert!(!envelope.is_expired(now));
    }

    #[test]
    fn expiry_check_drops_messages_past_the_clock_skew_tolerance() {
        let sender = Identity::generate();
        let mut envelope = sample_envelope(&sender, 16, 0);
        envelope.expires_at = 10_000;
        let now = envelope.expires_at + CLOCK_SKEW_TOLERANCE_MS + 1;
        assert!(envelope.is_expired(now));
    }
}

