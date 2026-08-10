//! Identity: every node has an Ed25519 keypair. The public key (32 bytes) is the NodeId,
//! used to sign messages so relays and recipients can verify authenticity even though
//! the message may have hopped through several untrusted intermediate nodes.
//!
//! # Two keypairs, one seed
//! Every identity also has an X25519 key-agreement keypair (see [`Identity::x25519_public`]),
//! used to derive a private shared secret with another node (see `session.rs`) --
//! Ed25519 and X25519 solve different problems and are deliberately not the same key:
//! Ed25519 proves *who said this*, X25519 establishes *a secret only the two of us know*.
//!
//! Rather than generating and persisting a *second* secret alongside the Ed25519 seed,
//! the X25519 secret is derived deterministically from that same seed (via
//! `blake3::derive_key` with a fixed, distinct domain-separation string). This means
//! restoring the Ed25519 identity from its persisted seed (see [`Identity::from_seed`])
//! automatically restores the same X25519 keypair too, with no changes needed to the
//! iOS Keychain / Android Keystore persistence code from the identity-persistence work --
//! there is exactly one secret to protect, not two.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519StaticSecret};

pub type NodeId = [u8; 32];
pub type Sig = [u8; 64];
/// An X25519 public key, as exchanged with another node for key agreement -- see
/// [`Identity::x25519_public`] and `session.rs`.
pub type X25519Public = [u8; 32];

/// Domain-separation string for deriving the X25519 secret from the Ed25519 seed. Fixed
/// and never reused for anything else, per BLAKE3's key-derivation guidance -- changing
/// this would derive different X25519 keys for every existing identity, so treat it as
/// part of the wire-compatible protocol ("MeshTalk Session KDF v1"), not an
/// implementation detail to casually edit.
const X25519_DERIVATION_CONTEXT: &str = "meshtalk 2026 x25519-static-key v1";

/// Domain-separation prefix for the signature binding an X25519 public key to the
/// Ed25519 identity that owns it (see [`Identity::sign_x25519_public`] and
/// [`x25519_binding_payload`]). Without this prefix, a signature produced for some other
/// purpose over the same 32 bytes (accidentally, or by a future protocol change) could be
/// misinterpreted as a valid binding -- see `session::PublicIdentity::verify_binding`'s
/// "tampered binding domain" test for the concrete scenario this closes.
const X25519_BINDING_DOMAIN: &str = "meshtalk/x25519-binding/v1";

/// The exact bytes that get signed (and later re-verified) to bind an X25519 public key
/// to a `NodeId` -- `domain_separator || NodeId || X25519PublicKey`. Exposed so
/// `session::PublicIdentity::verify_binding` can recompute the identical payload the
/// signer actually signed, rather than duplicating the domain separator string in two
/// places where they could drift out of sync.
pub fn x25519_binding_payload(node_id: &NodeId, x25519_public: &X25519Public) -> Vec<u8> {
    let mut buf = Vec::with_capacity(X25519_BINDING_DOMAIN.len() + 32 + 32);
    buf.extend_from_slice(X25519_BINDING_DOMAIN.as_bytes());
    buf.extend_from_slice(node_id);
    buf.extend_from_slice(x25519_public);
    buf
}

pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    /// Reconstructs the same identity (same `node_id`) from a previously-persisted
    /// 32-byte seed -- see [`Identity::seed`]. Lets an app keep the same `NodeId` across
    /// restarts instead of generating a brand-new one every launch, which otherwise
    /// silently breaks anything that assumed identity was stable (contacts, per-recipient
    /// encryption, trust verification, etc).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    /// The 32-byte seed backing this identity. Callers should persist this somewhere
    /// secure (iOS Keychain, Android Keystore-backed storage) and pass it back into
    /// [`Identity::from_seed`] on the next launch to keep the same `NodeId` -- treat it
    /// like a private key, because that's exactly what it is.
    pub fn seed(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn node_id(&self) -> NodeId {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn sign(&self, data: &[u8]) -> Sig {
        self.signing_key.sign(data).to_bytes()
    }

    /// This identity's X25519 key-agreement secret, derived deterministically from the
    /// Ed25519 seed -- see the module doc's "Two keypairs, one seed" section. Never
    /// exposed directly; everything callers need is [`Identity::x25519_public`],
    /// [`Identity::sign_x25519_public`], and [`Identity::derive_shared_secret`].
    fn x25519_secret(&self) -> X25519StaticSecret {
        let derived = blake3::derive_key(X25519_DERIVATION_CONTEXT, &self.seed());
        X25519StaticSecret::from(derived)
    }

    /// This identity's X25519 public key -- share this (along with
    /// [`Identity::sign_x25519_public`]'s signature over it) so another node can verify
    /// it genuinely belongs to your `node_id` and derive a shared secret with you. See
    /// `session::PublicIdentity`.
    pub fn x25519_public(&self) -> X25519Public {
        X25519PublicKey::from(&self.x25519_secret()).to_bytes()
    }

    /// Signs this identity's own X25519 public key with the long-term Ed25519 identity,
    /// cryptographically binding "the key-agreement key for this `node_id` is this X25519
    /// key". The signed payload is domain-separated (see [`x25519_binding_payload`]) so
    /// this signature can never be confused with a signature produced for some other
    /// purpose. Without this binding, a malicious relay could hand a peer a *different*
    /// X25519 public key while claiming it belongs to a legitimate `node_id`, and that
    /// peer would derive a shared secret with the attacker instead of the real party --
    /// see `session::PublicIdentity::verify_binding`.
    pub fn sign_x25519_public(&self) -> Sig {
        self.sign(&x25519_binding_payload(&self.node_id(), &self.x25519_public()))
    }

    /// Derives the X25519 Diffie-Hellman shared secret with `their_x25519_public`, or
    /// `None` if it's *non-contributory* (RFC 7748's term for a degenerate DH result --
    /// e.g. the classic "all-zero shared secret" attack, where a peer supplies a
    /// low-order public key specifically to force a predictable result regardless of
    /// your own secret). Legitimate X25519 public keys essentially never produce this;
    /// seeing it means the supplied key is malicious or corrupted, and the session must
    /// not be established.
    ///
    /// The returned secret is still *raw* DH output -- deliberately not exposed as
    /// something to encrypt with directly. It must always be run through a KDF that also
    /// binds both parties' identities (see `session::SessionKeyPair::derive`), never used
    /// as an encryption key as-is.
    pub fn derive_shared_secret(&self, their_x25519_public: &X25519Public) -> Option<[u8; 32]> {
        let their_public = X25519PublicKey::from(*their_x25519_public);
        let shared = self.x25519_secret().diffie_hellman(&their_public);
        if !shared.was_contributory() {
            return None;
        }
        Some(shared.to_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// This is the core guarantee persistent identity depends on: reconstructing an
    /// `Identity` from a previously-saved seed must produce the exact same `NodeId`, or
    /// every restart would silently become a new, unrecognized device.
    #[test]
    fn seed_round_trip_preserves_node_id() {
        let original = Identity::generate();
        let seed = original.seed();
        let restored = Identity::from_seed(seed);
        assert_eq!(original.node_id(), restored.node_id());
    }

    /// A restored identity must be able to produce signatures the *original* identity's
    /// public key (and thus anyone who already trusted it, e.g. saved contacts) can
    /// verify -- otherwise persistence would be pointless.
    #[test]
    fn signatures_from_restored_identity_verify_against_original_node_id() {
        let original = Identity::generate();
        let seed = original.seed();
        let restored = Identity::from_seed(seed);
        let message = b"hello mesh";
        let sig = restored.sign(message);
        assert!(verify(&original.node_id(), message, &sig));
    }

    /// Simulates "identity reset": generating fresh identities must not collide in
    /// practice, since that's what makes a reset actually produce a different `NodeId`.
    #[test]
    fn freshly_generated_identities_are_distinct() {
        let a = Identity::generate();
        let b = Identity::generate();
        assert_ne!(a.node_id(), b.node_id());
    }

    /// Different seeds must produce different identities (sanity check that `from_seed`
    /// isn't accidentally ignoring its input).
    #[test]
    fn different_seeds_produce_different_node_ids() {
        let a = Identity::from_seed([1u8; 32]);
        let b = Identity::from_seed([2u8; 32]);
        assert_ne!(a.node_id(), b.node_id());
    }

    /// The same seed must always reconstruct to the same identity -- this is what makes
    /// restart persistence deterministic rather than best-effort.
    #[test]
    fn same_seed_always_produces_same_node_id() {
        let a = Identity::from_seed([42u8; 32]);
        let b = Identity::from_seed([42u8; 32]);
        assert_eq!(a.node_id(), b.node_id());
    }
}
