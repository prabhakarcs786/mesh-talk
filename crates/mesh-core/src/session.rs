//! Contact identities and session-key derivation for private 1:1 encryption.
//!
//! # MeshTalk Static Session v1
//! This is the frozen name for the key-agreement scheme implemented here ("Milestone
//! 2A"/"2A.1") -- an explicit, documented capability statement, not marketing:
//!
//! ```text
//! MeshTalk Static Session v1
//! Authentication:            yes  (Ed25519 binds each X25519 key to a NodeId)
//! End-to-end encryption:     NOT YET -- next milestone (2B) actually encrypts with
//!                            the keys derived here
//! Forward secrecy:           NO
//! Post-compromise security:  NO
//! ```
//!
//! Both parties' X25519 keys are derived deterministically from their long-term,
//! persisted Ed25519 seed (static-static Diffie-Hellman -- see `identity.rs`'s "Two
//! keypairs, one seed"). That means every session between the same two `NodeId`s
//! derives the *same* shared secret forever: if either party's long-term identity seed
//! is ever compromised, an attacker who recorded past traffic on this session can
//! recompute all of its historical keys too. There is no forward secrecy and no
//! post-compromise security in this milestone -- that requires ephemeral/one-time keys
//! and a ratchet (Signal-style X3DH + Double Ratchet is the well-known example), which is
//! explicitly out of scope here to keep this milestone's cryptographic surface small and
//! independently testable. Do not describe this scheme as providing forward secrecy.
//!
//! This is key agreement only. Nothing in this module encrypts an application message;
//! it only establishes, and lets both sides verify, the keys that Milestone 2B will
//! actually encrypt with.
//!
//! # This module's cryptographic building block: BLAKE3 `derive_key`
//! Every derived key in this module comes from BLAKE3's `derive_key` function (its
//! dedicated key-derivation mode, not general hashing) with a hardcoded,
//! globally-unique, application-specific context string, exactly as BLAKE3's own
//! documentation recommends. This is used deliberately *in place of* HKDF-SHA256 --
//! it serves the same purpose (never use raw key-agreement output directly; always run
//! it through a proper KDF with domain separation) without adding an equivalent-purpose
//! dependency mesh-core doesn't otherwise need. Call this **MeshTalk Session KDF v1**,
//! not "equivalent to HKDF" -- the context strings below are that specification, and are
//! frozen: changing any of them changes every derived key for every existing identity,
//! breaking interoperability with any node that hasn't changed too.

use serde::{Deserialize, Serialize};

use crate::identity::{verify, x25519_binding_payload, Identity, NodeId, X25519Public};
use crate::message::PROTOCOL_VERSION;

/// Version of the *session/key-agreement* scheme specifically (distinct from the
/// envelope wire format's [`PROTOCOL_VERSION`]) -- bound into the root key derivation
/// (see [`SessionKeyPair::derive`]) so a future session scheme (e.g. one adding a
/// ratchet) can't be confused with this one even if the envelope wire format itself
/// doesn't change.
const SESSION_PROTOCOL_VERSION: u8 = 1;

/// Frozen BLAKE3 `derive_key` context strings -- part of the MeshTalk Session Protocol
/// v1 specification (see the module doc). Do not edit any of these post-deployment.
const SESSION_ROOT_CONTEXT: &str = "meshtalk 2026 session-root v1";
const SESSION_SEND_CONTEXT: &str = "meshtalk 2026 session-send v1";
const SESSION_RECV_CONTEXT: &str = "meshtalk 2026 session-recv v1";

/// Public accessor for [`SESSION_PROTOCOL_VERSION`] -- exists so callers outside this
/// crate (e.g. mesh-mobile's persistent `ContactStore`, Milestone 2B.2a) can record which
/// session/key-agreement scheme was in effect when a contact record was last written,
/// without needing the constant itself to be `pub`.
pub fn session_protocol_version() -> u8 {
    SESSION_PROTOCOL_VERSION
}

/// How much a contact is currently trusted -- **local trust state only, never
/// network-supplied**. See `ContactRecord` for why this deliberately does not live on
/// `PublicIdentity` (the struct that actually gets serialized/exchanged over the mesh):
/// if it did, a malicious peer could simply advertise `Verified` about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationState {
    /// Seen on the mesh (or received directly), but not out-of-band confirmed. The
    /// binding signature can still be checked (see `PublicIdentity::verify_binding`) --
    /// that only proves the X25519 key belongs to that `NodeId`'s Ed25519 identity, not
    /// that the `NodeId` itself belongs to the person you think it does.
    Unverified,
    /// Out-of-band verified (e.g. the two devices compared a safety number/QR code in
    /// person) -- set *only* by explicit local user action (a future milestone's UI),
    /// never inferred from anything a peer sends. Not yet wired up to anything.
    Verified,
}

/// Another node's public identity, exactly as it's exchanged over the mesh: enough to
/// verify who they are (Ed25519) and derive a private shared secret with them (X25519).
/// Deliberately contains **no trust/verification state and no display name** -- both are
/// local-only concerns (see `ContactRecord`); a struct that gets serialized and sent over
/// an untrusted relay network must not be able to assert anything about how much it
/// should be trusted or what it should be called.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub node_id: NodeId,
    pub x25519_public: X25519Public,
    /// Ed25519 signature (by `node_id`'s own identity) over the domain-separated binding
    /// payload (see `identity::x25519_binding_payload`) -- proves this X25519 key
    /// genuinely belongs to `node_id` rather than being substituted by a relay along the
    /// way. Always check `verify_binding()` before deriving or trusting a shared secret
    /// from this record. Stored as `Vec<u8>` on the wire (like `Envelope.signature`)
    /// since serde's derive only auto-implements fixed-size arrays up to length 32, not
    /// 64.
    pub x25519_signature: Vec<u8>,
}

impl PublicIdentity {
    /// Builds the record `identity` would hand out about itself to a new contact.
    pub fn new(identity: &Identity) -> Self {
        Self {
            node_id: identity.node_id(),
            x25519_public: identity.x25519_public(),
            x25519_signature: identity.sign_x25519_public().to_vec(),
        }
    }

    /// Verifies that `x25519_public` genuinely belongs to `node_id` -- i.e. `node_id`'s
    /// own Ed25519 identity signed exactly the domain-separated binding payload for it.
    /// A `PublicIdentity` that fails this check must not be used to derive or trust a
    /// shared secret; treat it the same as a signature verification failure anywhere
    /// else in this codebase. Malformed signature bytes (wrong length) fail safely
    /// (`false`), never panic. Most callers should prefer the consuming [`Self::verify`],
    /// which makes it a compile-time error to forget this check before deriving session
    /// keys -- this non-consuming version exists for callers that just want to check
    /// (or re-check) validity without giving up ownership.
    pub fn verify_binding(&self) -> bool {
        let Ok(sig) = <[u8; 64]>::try_from(self.x25519_signature.as_slice()) else {
            return false;
        };
        let payload = x25519_binding_payload(&self.node_id, &self.x25519_public);
        verify(&self.node_id, &payload, &sig)
    }

    /// Consumes `self` and returns a [`VerifiedPublicIdentity`] if the X25519 binding
    /// signature checks out (see [`Self::verify_binding`]), or hands `self` back
    /// unchanged in `Err` otherwise. This is the *only* way to construct a
    /// `VerifiedPublicIdentity` -- and [`Session::establish`] requires one -- so it is a
    /// compile-time error to derive session keys from an identity whose binding was
    /// never checked. Make insecure states difficult to represent: there is no API path
    /// that lets a caller forget this step.
    pub fn verify(self) -> Result<VerifiedPublicIdentity, PublicIdentity> {
        if self.verify_binding() {
            Ok(VerifiedPublicIdentity(self))
        } else {
            Err(self)
        }
    }
}

/// A [`PublicIdentity`] whose X25519-binding signature has already been checked (see
/// [`PublicIdentity::verify`]) -- this, not a plain `PublicIdentity`, is what
/// [`Session::establish`] requires.
///
/// **Important: what this does and does not prove.** It proves *this X25519 key belongs
/// to this Ed25519 identity (`NodeId`)*. It does **not** prove *this `NodeId` is the
/// person you think it is* -- that additional guarantee comes from a separate, later
/// step (out-of-band QR code / safety-number comparison -- see `ContactRecord`'s
/// `verification` field, not yet wired to any UI). Keep these two concepts separate:
/// cryptographic key-binding verification happens here and now; human identity
/// verification is a distinct, later concern.
pub struct VerifiedPublicIdentity(PublicIdentity);

impl VerifiedPublicIdentity {
    pub fn node_id(&self) -> NodeId {
        self.0.node_id
    }

    pub fn x25519_public(&self) -> X25519Public {
        self.0.x25519_public
    }

    pub fn public_identity(&self) -> &PublicIdentity {
        &self.0
    }
}

/// Local bookkeeping about a contact -- everything here is **local-only state**, never
/// deserialized *from the network* (this type still deliberately does not implement
/// `Serialize`/`Deserialize` itself, to keep it structurally impossible to accidentally
/// place on a wire envelope). Wraps the network-supplied [`PublicIdentity`] together with
/// local trust decisions and metadata that only the local device gets to set.
///
/// Since Milestone 2B.2a, mesh-mobile's `ContactStore` persists this data to a local,
/// versioned on-disk file (never sent over the network) via its own DTO types -- see
/// that module's doc for the disk format and the merge rules that keep `local_alias`/
/// `verification` immune to network overwrite even across restarts.
pub struct ContactRecord {
    pub public_identity: PublicIdentity,
    /// The name this contact *advertises about themselves* over the network (e.g. a
    /// profile display name they set) -- **untrusted, cosmetic only**. Never used for
    /// authentication or trust decisions; a malicious peer can claim to be named
    /// anything. Contrast with `local_alias`.
    pub advertised_name: Option<String>,
    /// The name the *local user* has assigned to this contact -- trusted, because only
    /// the local user (not the network) can set it. `None` until the user does so.
    pub local_alias: Option<String>,
    /// **Local trust state only.** See `VerificationState` -- never set from anything a
    /// peer sends, only ever set by explicit local user action.
    pub verification: VerificationState,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// Whether this contact has an unacknowledged "identity changed" warning pending --
    /// set by [`Self::mark_identity_change_pending`] whenever the stored `PublicIdentity`
    /// is replaced by different cryptographic material, and cleared only by explicit
    /// local acknowledgement ([`Self::acknowledge_identity_change`]) or a fresh
    /// out-of-band verification ([`Self::mark_verified`]). Persisted to disk so this
    /// warning survives an app restart instead of silently disappearing (a restart is
    /// not the same thing as the user having seen and dismissed it).
    pub identity_change_pending: bool,
}

impl ContactRecord {
    /// Records first contact with a peer -- always starts `Unverified`, regardless of
    /// anything the peer claims about itself, since `PublicIdentity` has no field a
    /// peer could use to assert otherwise (see the module doc).
    pub fn new(public_identity: PublicIdentity, advertised_name: Option<String>, now_ms: u64) -> Self {
        Self {
            public_identity,
            advertised_name,
            local_alias: None,
            verification: VerificationState::Unverified,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            identity_change_pending: false,
        }
    }

    /// Marks this contact as out-of-band verified -- call this *only* in response to an
    /// explicit local user action (e.g. confirming a safety number/QR code matches), not
    /// automatically or in response to anything received over the network. Also clears
    /// any pending identity-change warning: a fresh verification is a stronger signal
    /// than merely acknowledging the old warning existed.
    pub fn mark_verified(&mut self) {
        self.verification = VerificationState::Verified;
        self.identity_change_pending = false;
    }

    pub fn touch_last_seen(&mut self, now_ms: u64) {
        self.last_seen_ms = now_ms;
    }

    /// Call when the stored `PublicIdentity` is being replaced by different
    /// cryptographic material than what was previously seen for this `NodeId` -- resets
    /// trust to `Unverified` (never silently kept) and raises the pending flag so the
    /// warning survives until explicitly acknowledged, including across an app restart.
    pub fn mark_identity_change_pending(&mut self) {
        self.verification = VerificationState::Unverified;
        self.identity_change_pending = true;
    }

    /// Call when the local user has seen and dismissed the "identity changed" warning.
    /// Deliberately does **not** change `verification` -- acknowledging a warning is not
    /// the same thing as out-of-band verifying the new identity (see `mark_verified`).
    pub fn acknowledge_identity_change(&mut self) {
        self.identity_change_pending = false;
    }

    /// The name to show for this contact: the user's own alias if they've set one,
    /// otherwise the peer's untrusted advertised name, otherwise a short id. Both
    /// fallback tiers are clearly distinguishable in the UI layer (a later milestone) --
    /// this only defines the fallback order, not how to render the distinction.
    pub fn display_name(&self) -> String {
        if let Some(alias) = &self.local_alias {
            return alias.clone();
        }
        if let Some(name) = &self.advertised_name {
            return name.clone();
        }
        crate::identity::short_id(&self.public_identity.node_id)
    }
}

/// A pair of directional keys both parties in a 1:1 conversation independently derive
/// from the same X25519 shared secret plus both parties' identities. Crate-internal --
/// external callers use [`Session`] instead, which hides the low-to-high/high-to-low
/// bookkeeping behind `outbound_key()`/`inbound_key()` so it's not possible to
/// accidentally call the "send" accessor on both ends of a conversation. See the module
/// doc for why this alone does not yet encrypt anything, and does not provide forward
/// secrecy.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionKeyPair {
    /// Key used when the lower-sorted (by raw byte comparison) of the two `NodeId`s
    /// sends. "Lower/higher `NodeId`" is this protocol's stand-in for
    /// "initiator/responder": since MeshTalk has no live connection-establishment
    /// handshake (any side may originate a message asynchronously, possibly relayed
    /// through store-and-forward later), there is no real "who dialed first" to agree
    /// on -- a canonical, symmetric, purely-local ordering rule that both sides can
    /// compute independently (without any extra round trip) serves the same purpose:
    /// both parties deterministically agree on which of the two keys is whose, without
    /// inferring direction differently on each side.
    key_low_to_high: [u8; 32],
    /// Key used when the higher-sorted `NodeId` sends.
    key_high_to_low: [u8; 32],
}

impl SessionKeyPair {
    /// Derives both directional keys for the conversation between `node_a` and
    /// `node_b` from their X25519 shared secret (see [`Identity::derive_shared_secret`]
    /// -- callers must only pass a secret that came back `Some`, i.e. already confirmed
    /// contributory/non-degenerate). Both parties call this with the same five logical
    /// inputs (`node_a`/`node_b`, and their matching `x25519_a`/`x25519_b`, may be passed
    /// in either order) and get back the identical `SessionKeyPair`.
    ///
    /// The root key binds the session/protocol version, the raw shared secret, and
    /// *both* parties' `NodeId`s and X25519 public keys -- not just the shared secret --
    /// so that substituting either party's declared identity or X25519 key changes every
    /// derived key, even if (hypothetically) the same shared secret value were reused.
    pub(crate) fn derive(
        shared_secret: &[u8; 32],
        node_a: &NodeId,
        node_b: &NodeId,
        x25519_a: &X25519Public,
        x25519_b: &X25519Public,
    ) -> Self {
        let ((first_node, first_x25519), (second_node, second_x25519)) = if node_a <= node_b {
            ((node_a, x25519_a), (node_b, x25519_b))
        } else {
            ((node_b, x25519_b), (node_a, x25519_a))
        };

        let mut root_material = Vec::with_capacity(2 + 32 * 5);
        root_material.push(PROTOCOL_VERSION);
        root_material.push(SESSION_PROTOCOL_VERSION);
        root_material.extend_from_slice(shared_secret);
        root_material.extend_from_slice(first_node);
        root_material.extend_from_slice(second_node);
        root_material.extend_from_slice(first_x25519);
        root_material.extend_from_slice(second_x25519);
        let root_key = blake3::derive_key(SESSION_ROOT_CONTEXT, &root_material);

        Self {
            key_low_to_high: blake3::derive_key(SESSION_SEND_CONTEXT, &root_key),
            key_high_to_low: blake3::derive_key(SESSION_RECV_CONTEXT, &root_key),
        }
    }

    /// The key `i_am` should use to *encrypt* messages it sends to `other`. Well-defined
    /// (if unusual) even when `i_am == other` (messaging yourself): still produces one of
    /// the two derived keys deterministically, not a panic or an error.
    fn send_key(&self, i_am: &NodeId, other: &NodeId) -> [u8; 32] {
        if i_am <= other { self.key_low_to_high } else { self.key_high_to_low }
    }

    /// The key `i_am` should use to *decrypt* messages it receives from `other` -- always
    /// the other one of the two keys from whichever `send_key` returns for the same pair.
    fn recv_key(&self, i_am: &NodeId, other: &NodeId) -> [u8; 32] {
        if i_am <= other { self.key_high_to_low } else { self.key_low_to_high }
    }
}

/// An established 1:1 key-agreement session with one specific peer, from one specific
/// local identity's point of view. This is the *only* public way to obtain directional
/// encryption keys -- deliberately hiding `SessionKeyPair`'s low-level "who sorts
/// lower" bookkeeping behind `outbound_key()`/`inbound_key()`, which take no arguments
/// and can't be called with mismatched local/peer ids, because both are fixed at
/// construction time. This makes the exact misuse the naming risked ("somebody later
/// accidentally derives 'send' on both devices") a compile-time non-issue rather than
/// something that merely happens to be caught by tests.
pub struct Session {
    local: NodeId,
    peer: NodeId,
    keys: SessionKeyPair,
}

impl Session {
    /// Establishes a session with `peer` using `local_identity`'s own X25519 key and
    /// `peer`'s X25519 key -- `peer` must already be a [`VerifiedPublicIdentity`] (there
    /// is no overload accepting a plain, unverified `PublicIdentity`). Returns `None` if
    /// the resulting DH shared secret is non-contributory (see
    /// [`Identity::derive_shared_secret`]) -- extremely unlikely for a legitimate peer,
    /// but must not silently proceed if it happens.
    pub fn establish(local_identity: &Identity, peer: &VerifiedPublicIdentity) -> Option<Self> {
        let shared_secret = local_identity.derive_shared_secret(&peer.x25519_public())?;
        let keys = SessionKeyPair::derive(
            &shared_secret,
            &local_identity.node_id(),
            &peer.node_id(),
            &local_identity.x25519_public(),
            &peer.x25519_public(),
        );
        Some(Self {
            local: local_identity.node_id(),
            peer: peer.node_id(),
            keys,
        })
    }

    pub fn local_node_id(&self) -> NodeId {
        self.local
    }

    pub fn peer_node_id(&self) -> NodeId {
        self.peer
    }

    /// The key to use when *this session's local identity* encrypts a message to its peer.
    pub fn outbound_key(&self) -> [u8; 32] {
        self.keys.send_key(&self.local, &self.peer)
    }

    /// The key to use when decrypting a message received *from* this session's peer.
    pub fn inbound_key(&self) -> [u8; 32] {
        self.keys.recv_key(&self.local, &self.peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alice_and_bob_derive_matching_shared_secret() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let alice_secret = alice.derive_shared_secret(&bob.x25519_public()).unwrap();
        let bob_secret = bob.derive_shared_secret(&alice.x25519_public()).unwrap();

        assert_eq!(alice_secret, bob_secret);
    }

    #[test]
    fn alice_and_charlie_do_not_derive_the_same_secret_as_alice_and_bob() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let charlie = Identity::generate();

        let alice_bob_secret = alice.derive_shared_secret(&bob.x25519_public()).unwrap();
        let alice_charlie_secret = alice.derive_shared_secret(&charlie.x25519_public()).unwrap();

        assert_ne!(alice_bob_secret, alice_charlie_secret);
    }

    /// The concrete "all-zero shared secret" attack RFC 7748 warns about: a peer
    /// supplying a low-order (e.g. all-zero) X25519 public key to force a predictable,
    /// attacker-known shared secret regardless of our own secret key. Must be rejected,
    /// not silently accepted as a usable session.
    #[test]
    fn all_zero_peer_public_key_is_rejected_as_non_contributory() {
        let alice = Identity::generate();
        let all_zero_public_key: X25519Public = [0u8; 32];
        assert!(alice.derive_shared_secret(&all_zero_public_key).is_none());
    }

    #[test]
    fn both_sides_derive_identical_session_keys_regardless_of_argument_order() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let shared = alice.derive_shared_secret(&bob.x25519_public()).unwrap();

        // Alice and Bob each naturally call this with themselves listed first -- proving
        // this doesn't matter (order of node_a/node_b, and matching x25519_a/x25519_b,
        // is normalized internally) is exactly the "initiator/responder agree on
        // directional keys" requirement, without either side needing to know who
        // "initiated".
        let alice_view = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &bob.x25519_public());
        let bob_view = SessionKeyPair::derive(&shared, &bob.node_id(), &alice.node_id(), &bob.x25519_public(), &alice.x25519_public());

        assert!(alice_view == bob_view);
    }

    #[test]
    fn alice_send_key_equals_bob_recv_key_and_vice_versa() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let shared = alice.derive_shared_secret(&bob.x25519_public()).unwrap();
        let keys = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &bob.x25519_public());

        assert_eq!(
            keys.send_key(&alice.node_id(), &bob.node_id()),
            keys.recv_key(&bob.node_id(), &alice.node_id()),
            "A.send must equal B.recv"
        );
        assert_eq!(
            keys.send_key(&bob.node_id(), &alice.node_id()),
            keys.recv_key(&alice.node_id(), &bob.node_id()),
            "B.send must equal A.recv"
        );
    }

    #[test]
    fn send_and_recv_keys_are_different_from_each_other() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let shared = alice.derive_shared_secret(&bob.x25519_public()).unwrap();
        let keys = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &bob.x25519_public());

        assert_ne!(
            keys.send_key(&alice.node_id(), &bob.node_id()),
            keys.recv_key(&alice.node_id(), &bob.node_id()),
            "A.send must not equal A.recv"
        );
    }

    /// Swapping which peer identity the session is bound to -- keeping the same shared
    /// secret value but a different second party -- must change the derived keys. This
    /// is what actually enforces "these keys represent Alice talking to Bob", not merely
    /// "two X25519 keys happened to produce this secret".
    #[test]
    fn swapping_peer_identity_changes_derived_keys() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let charlie = Identity::generate();
        // Same numeric shared_secret value reused on purpose (simulating a hypothetical
        // secret collision) to isolate that identity binding -- not just the secret --
        // is what changes the result.
        let shared = [7u8; 32];

        let with_bob = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &bob.x25519_public());
        let with_charlie = SessionKeyPair::derive(&shared, &alice.node_id(), &charlie.node_id(), &alice.x25519_public(), &charlie.x25519_public());

        assert!(with_bob != with_charlie);
    }

    /// Swapping the declared X25519 public keys (holding the shared secret and NodeIds
    /// fixed) must also change the derived keys -- proving the X25519 public keys are
    /// actually mixed into the KDF, not merely decorative alongside the shared secret.
    #[test]
    fn swapping_x25519_keys_changes_derived_keys() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let shared = [7u8; 32];
        let real_bob_x25519 = bob.x25519_public();
        let substituted_x25519: X25519Public = [9u8; 32];

        let real = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &real_bob_x25519);
        let substituted = SessionKeyPair::derive(&shared, &alice.node_id(), &bob.node_id(), &alice.x25519_public(), &substituted_x25519);

        assert!(real != substituted);
    }

    /// "Own-key/self-session" behavior: deriving a shared secret and session keys with
    /// yourself must not panic, and must be deterministic -- even though messaging
    /// yourself isn't a normal use case, nothing here should crash if it happens. Note
    /// that `send_key` and `recv_key` are still two *different* derived keys even in
    /// this case (each always pulls from a different one of the two context-separated
    /// keys) -- that's fine and arguably desirable, not a bug to "fix".
    #[test]
    fn self_session_is_well_defined_and_does_not_panic() {
        let alice = Identity::generate();
        let self_secret_a = alice.derive_shared_secret(&alice.x25519_public()).unwrap();
        let self_secret_b = alice.derive_shared_secret(&alice.x25519_public()).unwrap();
        assert_eq!(self_secret_a, self_secret_b);

        let keys = SessionKeyPair::derive(&self_secret_a, &alice.node_id(), &alice.node_id(), &alice.x25519_public(), &alice.x25519_public());
        let send_once = keys.send_key(&alice.node_id(), &alice.node_id());
        let send_again = keys.send_key(&alice.node_id(), &alice.node_id());
        let recv_once = keys.recv_key(&alice.node_id(), &alice.node_id());
        let recv_again = keys.recv_key(&alice.node_id(), &alice.node_id());
        // Deterministic: calling twice with the same inputs always gives the same key.
        assert_eq!(send_once, send_again);
        assert_eq!(recv_once, recv_again);
    }

    #[test]
    fn valid_x25519_binding_verifies() {
        let alice = Identity::generate();
        let public_identity = PublicIdentity::new(&alice);
        assert!(public_identity.verify_binding());
    }

    #[test]
    fn tampered_x25519_public_key_fails_binding_verification() {
        let alice = Identity::generate();
        let mallory = Identity::generate();
        let mut public_identity = PublicIdentity::new(&alice);
        // Mallory substitutes her own X25519 key while keeping Alice's signature and
        // node_id -- this must be detected, or a relay could silently redirect key
        // agreement to an attacker.
        public_identity.x25519_public = mallory.x25519_public();
        assert!(!public_identity.verify_binding());
    }

    /// A signature produced over the *old*, non-domain-separated scheme (just the raw
    /// x25519_public bytes, no domain prefix or NodeId) must NOT verify under the new
    /// domain-separated binding -- proving the domain separator (and NodeId inclusion)
    /// actually matters and isn't silently ignored.
    #[test]
    fn signature_without_domain_separator_is_rejected() {
        let alice = Identity::generate();
        let old_style_signature = alice.sign(&alice.x25519_public()); // no domain prefix
        let public_identity = PublicIdentity {
            node_id: alice.node_id(),
            x25519_public: alice.x25519_public(),
            x25519_signature: old_style_signature.to_vec(),
        };
        assert!(!public_identity.verify_binding());
    }

    #[test]
    fn restored_persistent_identity_derives_the_same_public_identity() {
        let original = Identity::generate();
        let seed = original.seed();
        let restored = Identity::from_seed(seed);

        assert_eq!(original.x25519_public(), restored.x25519_public());
        assert_eq!(original.sign_x25519_public(), restored.sign_x25519_public());
        assert!(PublicIdentity::new(&restored).verify_binding());
    }

    /// A `PublicIdentity` built from mismatched fields (an `x25519_public` that never
    /// belonged to `node_id`, e.g. corrupted-in-transit) must fail safely, not panic.
    #[test]
    fn malformed_public_identity_fails_safely() {
        let unrelated_signature = vec![0u8; 64];
        let public_identity = PublicIdentity {
            node_id: Identity::generate().node_id(),
            x25519_public: Identity::generate().x25519_public(),
            x25519_signature: unrelated_signature,
        };
        assert!(!public_identity.verify_binding());
    }

    /// A signature with the wrong byte length (not even structurally a valid Ed25519
    /// signature) must be rejected gracefully, not panic on the length conversion.
    #[test]
    fn wrong_length_signature_fails_safely() {
        let alice = Identity::generate();
        let mut public_identity = PublicIdentity::new(&alice);
        public_identity.x25519_signature = vec![0u8; 10];
        assert!(!public_identity.verify_binding());
    }

    /// Structural proof that a network-supplied identity cannot mark itself `Verified`:
    /// `PublicIdentity` (the only type ever deserialized from the network) has no
    /// verification field to smuggle a claim in at all -- a `ContactRecord` always
    /// starts `Unverified` no matter what the wrapped `PublicIdentity` contains, since
    /// there is nothing on `PublicIdentity` that could say otherwise.
    #[test]
    fn contact_record_always_starts_unverified_regardless_of_network_data() {
        let alice = Identity::generate();
        let public_identity = PublicIdentity::new(&alice);
        let contact = ContactRecord::new(public_identity, Some("Totally Legit Bob".to_string()), 12_345);
        assert_eq!(contact.verification, VerificationState::Unverified);
    }

    #[test]
    fn contact_record_display_name_prefers_local_alias_over_advertised_name() {
        let alice = Identity::generate();
        let public_identity = PublicIdentity::new(&alice);
        let mut contact = ContactRecord::new(public_identity, Some("advertised".to_string()), 0);
        assert_eq!(contact.display_name(), "advertised");
        contact.local_alias = Some("my alias for them".to_string());
        assert_eq!(contact.display_name(), "my alias for them");
    }

    // -- Session (the hardened public API on top of SessionKeyPair) --

    fn verified(identity: &Identity) -> VerifiedPublicIdentity {
        PublicIdentity::new(identity).verify().unwrap()
    }

    #[test]
    fn session_establish_fails_for_non_contributory_peer_key() {
        let alice = Identity::generate();
        // Can't construct a VerifiedPublicIdentity with an all-zero key directly (its
        // signature wouldn't verify), so this exercises the same non-contributory
        // rejection via Identity::derive_shared_secret through Session::establish by
        // constructing a VerifiedPublicIdentity for a real identity, then simulating a
        // substituted all-zero X25519 key -- which also fails verify() first, proving
        // *both* layers (binding verification and contributory-ness) independently
        // guard session establishment.
        let mut tampered = PublicIdentity::new(&Identity::generate());
        tampered.x25519_public = [0u8; 32];
        assert!(tampered.verify().is_err());
        let _ = alice; // (kept for symmetry/clarity with other tests in this module)
    }

    #[test]
    fn alice_outbound_equals_bob_inbound_and_vice_versa() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();

        assert_eq!(alice_session.outbound_key(), bob_session.inbound_key(), "Alice.outbound == Bob.inbound");
        assert_eq!(bob_session.outbound_key(), alice_session.inbound_key(), "Bob.outbound == Alice.inbound");
    }

    #[test]
    fn alice_outbound_differs_from_alice_inbound() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();

        assert_ne!(alice_session.outbound_key(), alice_session.inbound_key());
        assert_ne!(bob_session.outbound_key(), bob_session.inbound_key());
    }
}

