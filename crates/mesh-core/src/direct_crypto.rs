//! "MeshTalk Direct Encryption v1" -- authenticated encryption for a single 1:1 direct
//! message, using the session keys established in `session.rs`. This is Milestone 2B:
//! actually encrypting message content, not just agreeing on keys.
//!
//! # What this provides
//! - **Confidentiality**: only the two parties holding a [`Session`]'s keys can read the
//!   plaintext (via XChaCha20-Poly1305 AEAD, a 256-bit key with a 192-bit random nonce).
//! - **Integrity of both the ciphertext and the message's authenticated metadata** (see
//!   [`DirectMessageAadV1`]) -- tampering with any authenticated field (protocol
//!   version, encryption version, message id, sender, recipient, message type, created
//!   at, expires at, max hops) makes decryption fail. `hops_used` is deliberately
//!   excluded, since relays must be able to mutate it in transit (mirrors `Envelope`'s
//!   own hop-limit design in `message.rs`).
//! - **Sender authentication independent of the AEAD**: the whole `(AAD || nonce ||
//!   ciphertext)` is *also* Ed25519-signed by the sender's long-term identity. The AEAD
//!   tells the recipient "someone holding our session key produced this"; the signature
//!   additionally tells any node "this was signed by Alice's identity" -- useful for
//!   future relay-abuse prevention/protocol-level authentication independent of who can
//!   decrypt it.
//! - **Fail-closed behavior**: there is no plaintext fallback and no silent downgrade.
//!   If a verified identity/session for the recipient isn't available,
//!   [`try_encrypt_direct_message`] returns [`DirectCryptoError::CannotEncryptForRecipient`]
//!   -- callers must surface that to the user (e.g. "Secure session unavailable"), never
//!   send the message unencrypted, never fall back to the old shared channel key, never
//!   downgrade to an older encryption version.
//!
//! # What this does NOT provide
//! No forward secrecy, no post-compromise security -- inherited from the static session
//! keys this encrypts with (see `session.rs`'s "MeshTalk Static Session v1" doc). No
//! group encryption, no ratcheting: explicitly out of scope for this milestone.
//!
//! # Milestone 2B.1: wired into `MeshNode`
//! `MeshNode::send_text`/`send_file` (see `node.rs`) now use this scheme for direct
//! (recipient-addressed) chat messages, via [`DirectCryptoHeaderV1`]/[`DirectEnvelopeBody`]
//! below -- these make a `DirectV1` envelope **self-contained**: the header carries the
//! sender's X25519 public key and its Ed25519 binding signature *inside the packet
//! itself*, so the recipient can reconstruct and verify the sender's
//! `VerifiedPublicIdentity`, establish a `Session`, and decrypt without any prior live
//! handshake or discovery exchange. This is what makes `DirectV1` compatible with a
//! future store-and-forward design: Bob can decrypt a message from Alice after
//! restarting his app, even if Alice is long offline by then -- everything needed is in
//! the one packet plus Bob's own persisted identity. Broadcast messages and call
//! signaling/frames continue using `EncryptionMode::ChannelV1` unchanged; only
//! `send_text`/`send_file`'s direct (recipient-addressed) path uses `DirectV1` -- see
//! `node.rs`'s `send_one_payload` for exactly how `encryption_mode` is chosen.

use chacha20poly1305::aead::{Aead, AeadCore, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::identity::{verify, Identity, NodeId, X25519Public};
use crate::message::MessageType;
use crate::session::{PublicIdentity, Session, VerifiedPublicIdentity};

/// Version of this specific encryption scheme (distinct from both the envelope wire
/// format's `message::PROTOCOL_VERSION` and the session/key-agreement scheme's own
/// version in `session.rs`) -- bound into the authenticated data so a future
/// `DirectRatchetV2` scheme can never be confused with this one. Frozen: part of the
/// MeshTalk Direct Encryption v1 specification (see [`encode_aad_v1`]).
pub const ENCRYPTION_VERSION: u8 = 1;

/// Domain tag prefixed to the authenticated-data encoding -- see [`encode_aad_v1`].
const AAD_DOMAIN_TAG: &[u8] = b"MESHTALK-AAD-V1";

/// Everything about a direct message that must be authenticated (tamper-evident) but is
/// not itself secret -- passed to the AEAD as associated data, and also covered by the
/// Ed25519 signature (see the module doc). Deliberately excludes `hops_used`, which
/// relays must be able to mutate in transit.
///
/// This is a plain data holder; the actual wire encoding is [`encode_aad_v1`] -- kept as
/// an explicit, hand-written fixed-width byte layout rather than `bincode::serialize`
/// specifically so the exact bytes are a stable, documented part of the protocol (an
/// Android/iOS/future-Rust reimplementation must be able to reproduce them exactly), not
/// an accident of whatever Rust's struct layout or bincode's format happens to do.
#[derive(Clone, Copy)]
pub struct DirectMessageAadV1 {
    pub protocol_version: u8,
    /// Normally always [`ENCRYPTION_VERSION`] -- exposed as a plain field (rather than
    /// hardcoded inside [`encode_aad_v1`]) specifically so "an envelope claiming a
    /// different/unknown encryption version" is representable and testable, and so
    /// [`decrypt_direct_message`] can explicitly reject it up front (see
    /// [`DirectCryptoError::UnknownEncryptionVersion`]) rather than that check existing
    /// only implicitly.
    pub encryption_version: u8,
    pub message_id: [u8; 16],
    pub sender: NodeId,
    pub recipient: NodeId,
    pub message_type: MessageType,
    pub created_at: u64,
    pub expires_at: u64,
    pub max_hops: u8,
}

impl DirectMessageAadV1 {
    /// Builds the AAD for a new outgoing message -- always stamps the current
    /// [`ENCRYPTION_VERSION`]. Use this for real messages; construct the struct directly
    /// (as tests do) only to simulate a malformed/foreign encryption version.
    pub fn new(
        protocol_version: u8,
        message_id: [u8; 16],
        sender: NodeId,
        recipient: NodeId,
        message_type: MessageType,
        created_at: u64,
        expires_at: u64,
        max_hops: u8,
    ) -> Self {
        Self {
            protocol_version,
            encryption_version: ENCRYPTION_VERSION,
            message_id,
            sender,
            recipient,
            message_type,
            created_at,
            expires_at,
            max_hops,
        }
    }
}

/// The frozen MeshTalk Direct Encryption v1 associated-data encoding:
///
/// ```text
/// "MESHTALK-AAD-V1"        (15 bytes, literal ASCII domain tag)
/// || protocol_version      (1 byte)
/// || encryption_version    (1 byte)
/// || message_id            (16 bytes)
/// || sender                (32 bytes)
/// || recipient             (32 bytes)
/// || message_type          (1 byte: 0=Chat, 1=CallSignal, 2=CallFrame)
/// || created_at            (8 bytes, big-endian u64)
/// || expires_at            (8 bytes, big-endian u64)
/// || max_hops              (1 byte)
/// ```
///
/// Fixed-width, big-endian, explicitly ordered -- not `bincode::serialize`, and not
/// dependent on Rust struct layout, so any correct reimplementation of this exact byte
/// layout in another language produces byte-identical output. Do not change this
/// encoding post-deployment; it is as much a frozen part of the protocol as the context
/// strings in `session.rs`.
pub fn encode_aad_v1(aad: &DirectMessageAadV1) -> Vec<u8> {
    let mut buf = Vec::with_capacity(AAD_DOMAIN_TAG.len() + 1 + 1 + 16 + 32 + 32 + 1 + 8 + 8 + 1);
    buf.extend_from_slice(AAD_DOMAIN_TAG);
    buf.push(aad.protocol_version);
    buf.push(aad.encryption_version);
    buf.extend_from_slice(&aad.message_id);
    buf.extend_from_slice(&aad.sender);
    buf.extend_from_slice(&aad.recipient);
    buf.push(aad.message_type.discriminant());
    buf.extend_from_slice(&aad.created_at.to_be_bytes());
    buf.extend_from_slice(&aad.expires_at.to_be_bytes());
    buf.push(aad.max_hops);
    buf
}

/// Everything that can go wrong producing or consuming a [`DirectCiphertext`]. Every
/// variant is a closed failure -- none of them ever result in plaintext being sent
/// unencrypted, an old/weaker scheme silently being substituted, or a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectCryptoError {
    /// No usable verified identity/session exists for this recipient. Callers must fail
    /// closed (see the module doc): never fall back to plaintext, the shared channel
    /// key, or a downgraded encryption version -- surface this to the user instead (e.g.
    /// "Secure session unavailable").
    CannotEncryptForRecipient,
    /// The AAD claims an encryption version this code doesn't implement -- rejected
    /// before ever attempting AEAD, the same way `Envelope::is_supported_version` guards
    /// the outer wire format.
    UnknownEncryptionVersion,
    /// Ed25519 signature over `(AAD || nonce || ciphertext)` didn't verify -- forged,
    /// corrupted, or produced by a different sender than claimed.
    SignatureInvalid,
    /// AEAD authentication failed -- wrong session key, tampered ciphertext/nonce/AAD, or
    /// corrupted data. Deliberately doesn't distinguish *which* of these, so as not to
    /// leak to an attacker which field they successfully vs. unsuccessfully tampered
    /// with.
    DecryptionFailed,
    /// Structurally malformed input (empty/too-short ciphertext, malformed signature
    /// length) -- rejected before ever attempting AEAD, and never panics.
    MalformedInput,
}

impl std::fmt::Display for DirectCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            DirectCryptoError::CannotEncryptForRecipient => "cannot encrypt for recipient: no verified session key available",
            DirectCryptoError::UnknownEncryptionVersion => "unknown encryption version",
            DirectCryptoError::SignatureInvalid => "signature invalid",
            DirectCryptoError::DecryptionFailed => "decryption failed",
            DirectCryptoError::MalformedInput => "malformed input",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for DirectCryptoError {}

/// The output of encrypting one direct message: nonce + ciphertext (AEAD output,
/// includes the Poly1305 tag) + an Ed25519 signature over all of it plus the AAD -- see
/// the module doc for why both AEAD and a separate signature are used.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectCiphertext {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
    /// Ed25519 signature (64 bytes) over `encode_aad_v1(aad) || nonce || ciphertext`,
    /// stored as `Vec<u8>` on the wire for the same reason as `Envelope.signature`.
    pub signature: Vec<u8>,
}

/// The minimum information a `DirectV1` envelope carries *inside itself* so its
/// recipient can reconstruct and verify the sender's [`VerifiedPublicIdentity`] without
/// any prior live handshake or discovery exchange -- this is what makes a `DirectV1`
/// envelope self-contained (see the module doc's "Milestone 2B.1" section) and therefore
/// compatible with an eventual store-and-forward design, where the recipient might not
/// come back online until long after the sender went offline.
///
/// Deliberately contains **only** what's needed to verify the sender's encryption
/// identity -- no local trust/verification state (that's `ContactRecord`'s job, and it
/// must never be put on the wire; see `session.rs`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectCryptoHeaderV1 {
    pub sender_x25519_public: X25519Public,
    /// Domain-separated Ed25519 signature binding `sender_x25519_public` to the
    /// envelope's `sender` `NodeId` -- see `identity::x25519_binding_payload`. The
    /// recipient must verify this (reconstructing a `PublicIdentity` from the envelope's
    /// `sender` plus these two fields, then calling `PublicIdentity::verify()`) before
    /// ever trusting this X25519 key enough to establish a session with it.
    pub sender_x25519_signature: Vec<u8>,
}

impl DirectCryptoHeaderV1 {
    pub fn new(sender_identity: &Identity) -> Self {
        Self {
            sender_x25519_public: sender_identity.x25519_public(),
            sender_x25519_signature: sender_identity.sign_x25519_public().to_vec(),
        }
    }

    /// Reconstructs and verifies the sender's identity from `self` plus the envelope's
    /// own (already-signature-verified) `sender` `NodeId` -- returns `None` if the
    /// X25519 binding doesn't check out, in which case the caller must not proceed to
    /// establish a session or decrypt anything (see `MeshNode::handle_incoming`).
    pub fn verify_sender(&self, sender: NodeId) -> Option<VerifiedPublicIdentity> {
        let public_identity = PublicIdentity {
            node_id: sender,
            x25519_public: self.sender_x25519_public,
            x25519_signature: self.sender_x25519_signature.clone(),
        };
        public_identity.verify().ok()
    }
}

/// What actually goes into an `Envelope.ciphertext` when
/// `Envelope.encryption_mode == EncryptionMode::DirectV1` -- the self-contained header
/// (see [`DirectCryptoHeaderV1`]) plus the AEAD output (see [`DirectCiphertext`]),
/// bincode-serialized together as one opaque blob from the envelope's point of view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectEnvelopeBody {
    pub header: DirectCryptoHeaderV1,
    pub message: DirectCiphertext,
}

/// Minimum possible length of a genuine XChaCha20-Poly1305 ciphertext: even encrypting
/// zero bytes of plaintext still produces the 16-byte Poly1305 authentication tag.
/// Anything shorter is structurally not a valid ciphertext, regardless of the key.
const MIN_CIPHERTEXT_LEN: usize = 16;

/// Encrypts `plaintext` for `session.peer_node_id()`, authenticating `aad` alongside it.
/// `sender_identity` must be the same identity that established `session` as its
/// "local" side -- used only to produce the accompanying Ed25519 signature (see the
/// module doc), not for the AEAD key itself (that's `session.outbound_key()`).
///
/// The nonce is a fresh random 24 bytes from the OS CSPRNG on *every* call -- never
/// derived from a counter, timestamp, or anything that could repeat across a restart.
/// Reusing an XChaCha20-Poly1305 nonce with the same key is catastrophic (it can expose
/// the plaintext and break authentication), so this is generated unconditionally here
/// rather than trusting any caller-supplied value -- there is no parameter a caller
/// could even pass a nonce through.
pub fn encrypt_direct_message(sender_identity: &Identity, session: &Session, aad: &DirectMessageAadV1, plaintext: &[u8]) -> DirectCiphertext {
    let key_bytes = session.outbound_key();
    let key = Key::from_slice(&key_bytes);
    let cipher = <XChaCha20Poly1305 as chacha20poly1305::KeyInit>::new(key);
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad_bytes = encode_aad_v1(aad);
    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad: &aad_bytes })
        .expect("encryption cannot fail for valid inputs");

    let nonce_bytes: [u8; 24] = nonce.into();
    let mut signed_payload = Vec::with_capacity(aad_bytes.len() + 24 + ciphertext.len());
    signed_payload.extend_from_slice(&aad_bytes);
    signed_payload.extend_from_slice(&nonce_bytes);
    signed_payload.extend_from_slice(&ciphertext);
    let signature = sender_identity.sign(&signed_payload).to_vec();

    DirectCiphertext {
        nonce: nonce_bytes,
        ciphertext,
        signature,
    }
}

/// Fail-closed convenience wrapper: verifies `recipient_identity`'s X25519 binding,
/// establishes a session, and encrypts -- returning
/// [`DirectCryptoError::CannotEncryptForRecipient`] the moment any of that fails, rather
/// than ever falling back to something insecure. Most callers should use this instead of
/// manually chaining `PublicIdentity::verify()` + `Session::establish()` +
/// `encrypt_direct_message()` (which remain available directly for callers that already
/// hold a `Session`, e.g. one cached from a previous message in the same conversation).
pub fn try_encrypt_direct_message(
    sender_identity: &Identity,
    recipient_identity: PublicIdentity,
    aad: &DirectMessageAadV1,
    plaintext: &[u8],
) -> Result<DirectCiphertext, DirectCryptoError> {
    let verified = recipient_identity.verify().map_err(|_| DirectCryptoError::CannotEncryptForRecipient)?;
    let session = Session::establish(sender_identity, &verified).ok_or(DirectCryptoError::CannotEncryptForRecipient)?;
    Ok(encrypt_direct_message(sender_identity, &session, aad, plaintext))
}

/// Decrypts and authenticates a direct message. Checks, in order: (1) `aad`'s declared
/// encryption version is one this code understands, (2) the Ed25519 signature over
/// `(AAD || nonce || ciphertext)` against `sender`'s claimed identity, (3) the AEAD tag
/// (which also authenticates every field in `aad`) using `session.inbound_key()`. All
/// three must succeed, or this returns an error -- never partial plaintext, never
/// "probably fine".
pub fn decrypt_direct_message(session: &Session, sender: &NodeId, aad: &DirectMessageAadV1, message: &DirectCiphertext) -> Result<Vec<u8>, DirectCryptoError> {
    if aad.encryption_version != ENCRYPTION_VERSION {
        return Err(DirectCryptoError::UnknownEncryptionVersion);
    }
    if message.ciphertext.len() < MIN_CIPHERTEXT_LEN {
        return Err(DirectCryptoError::MalformedInput);
    }
    let Ok(sig): Result<[u8; 64], _> = message.signature.as_slice().try_into() else {
        return Err(DirectCryptoError::MalformedInput);
    };

    let aad_bytes = encode_aad_v1(aad);
    let mut signed_payload = Vec::with_capacity(aad_bytes.len() + 24 + message.ciphertext.len());
    signed_payload.extend_from_slice(&aad_bytes);
    signed_payload.extend_from_slice(&message.nonce);
    signed_payload.extend_from_slice(&message.ciphertext);
    if !verify(sender, &signed_payload, &sig) {
        return Err(DirectCryptoError::SignatureInvalid);
    }

    let key_bytes = session.inbound_key();
    let key = Key::from_slice(&key_bytes);
    let cipher = <XChaCha20Poly1305 as chacha20poly1305::KeyInit>::new(key);
    let nonce = XNonce::from_slice(&message.nonce);
    cipher
        .decrypt(nonce, Payload { msg: &message.ciphertext, aad: &aad_bytes })
        .map_err(|_| DirectCryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::PROTOCOL_VERSION;
    use crate::session::VerifiedPublicIdentity;
    use rand::RngCore;

    fn verified(identity: &Identity) -> VerifiedPublicIdentity {
        PublicIdentity::new(identity).verify().unwrap()
    }

    fn sample_aad(sender: &Identity, recipient: &Identity) -> DirectMessageAadV1 {
        DirectMessageAadV1::new(PROTOCOL_VERSION, [1u8; 16], sender.node_id(), recipient.node_id(), MessageType::Chat, 1_000, 2_000, 16)
    }

    /// Alice -> Bob: Bob (with a session established from his own perspective) can
    /// decrypt a message Alice encrypted for him.
    #[test]
    fn alice_to_bob_decrypts() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);

        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        let plaintext = decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).unwrap();
        assert_eq!(plaintext, b"hello bob");
    }

    /// Bob -> Alice: the reverse direction works too (not just symmetric by accident).
    #[test]
    fn bob_to_alice_decrypts() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        let aad = sample_aad(&bob, &alice);

        let ciphertext = encrypt_direct_message(&bob, &bob_session, &aad, b"hello alice");

        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let plaintext = decrypt_direct_message(&alice_session, &bob.node_id(), &aad, &ciphertext).unwrap();
        assert_eq!(plaintext, b"hello alice");
    }

    /// Charlie, who is not a party to the Alice/Bob conversation, cannot decrypt it even
    /// though he can see the ciphertext (e.g. as a relay) -- his session with Alice uses
    /// entirely different keys.
    #[test]
    fn charlie_cannot_decrypt_alice_to_bob() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let charlie = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"private to bob");

        let charlie_session = Session::establish(&charlie, &verified(&alice)).unwrap();
        let result = decrypt_direct_message(&charlie_session, &alice.node_id(), &aad, &ciphertext);
        assert_eq!(result, Err(DirectCryptoError::DecryptionFailed));
    }

    /// Using the *wrong* directional key (e.g. Bob tries decrypting with his own
    /// outbound key, meant for messages he sends, instead of his inbound key) must fail
    /// -- proving `Session::inbound_key`/`outbound_key` genuinely are different keys
    /// that matter, not interchangeable.
    #[test]
    fn wrong_directional_key_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        // Alice trying to decrypt her own outgoing message with her own session (which
        // uses her *outbound* key for encryption, but decryption needs the peer's
        // matching inbound view) must fail -- she'd need session.inbound_key(), which is
        // Bob's outbound key, not her own.
        let result = decrypt_direct_message(&alice_session, &alice.node_id(), &aad, &ciphertext);
        assert_eq!(result, Err(DirectCryptoError::DecryptionFailed));
    }

    #[test]
    fn ciphertext_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let mut ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");
        ciphertext.ciphertext[0] ^= 0xFF;

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        // The signature (computed over the original ciphertext) will also now fail,
        // which is fine -- either failure mode proves tampering is detected.
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn nonce_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let mut ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");
        ciphertext.nonce[0] ^= 0xFF;

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn sender_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let mallory = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.sender = mallory.node_id();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn recipient_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let charlie = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.recipient = charlie.node_id();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn message_id_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.message_id = [9u8; 16];
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn message_type_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.message_type = MessageType::CallSignal;
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn created_at_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.created_at += 1;
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn expires_at_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.expires_at += 1;
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn max_hops_mutation_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.max_hops += 1;
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    /// `hops_used` isn't a field on `DirectMessageAadV1` at all -- a relay incrementing
    /// it (as `MeshNode::handle_incoming` does to the outer `Envelope`) cannot possibly
    /// invalidate this AAD's authentication, because it was never part of it in the
    /// first place. Demonstrated by decrypting successfully using the same, unchanged
    /// AAD regardless of how many times the (imagined) surrounding envelope was relayed.
    #[test]
    fn hops_used_is_not_part_of_the_authenticated_data() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob); // max_hops present, hops_used has no field here at all
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        // Decrypts fine "no matter how many hops it took to get here" -- there is
        // nothing to invalidate, by construction.
        let plaintext = decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).unwrap();
        assert_eq!(plaintext, b"hello bob");
    }

    #[test]
    fn malformed_nonce_rejected_safely() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let mut ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");
        ciphertext.nonce = [0xFFu8; 24]; // structurally valid length, but garbage content

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext).is_err());
    }

    #[test]
    fn truncated_ciphertext_rejected_safely() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let mut ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");
        ciphertext.ciphertext.truncate(4); // shorter than even the Poly1305 tag

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert_eq!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext), Err(DirectCryptoError::MalformedInput));
    }

    #[test]
    fn empty_ciphertext_rejected_safely() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let mut ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");
        ciphertext.ciphertext.clear();

        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert_eq!(decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext), Err(DirectCryptoError::MalformedInput));
    }

    #[test]
    fn random_garbage_never_panics() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        let aad = sample_aad(&alice, &bob);

        let mut garbage_ciphertext = vec![0u8; 100];
        OsRng.fill_bytes(&mut garbage_ciphertext);
        let mut garbage_signature = vec![0u8; 64];
        OsRng.fill_bytes(&mut garbage_signature);
        let mut garbage_nonce = [0u8; 24];
        OsRng.fill_bytes(&mut garbage_nonce);

        let garbage = DirectCiphertext {
            nonce: garbage_nonce,
            ciphertext: garbage_ciphertext,
            signature: garbage_signature,
        };
        // Only requirement: must not panic. Virtually certain to fail verification.
        let _ = decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &garbage);
    }

    #[test]
    fn unknown_encryption_version_rejected() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let mut aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hello bob");

        aad.encryption_version = 99;
        let bob_session = Session::establish(&bob, &verified(&alice)).unwrap();
        assert_eq!(
            decrypt_direct_message(&bob_session, &alice.node_id(), &aad, &ciphertext),
            Err(DirectCryptoError::UnknownEncryptionVersion)
        );
    }

    /// A `PublicIdentity` whose binding never verifies (tampered X25519 key) can never
    /// become a `VerifiedPublicIdentity` -- there is no code path to construct one from
    /// it, so `Session::establish` (which requires a `VerifiedPublicIdentity`) can never
    /// be called with it in the first place. This is enforced by the type system, not
    /// merely by a runtime check.
    #[test]
    fn unverified_public_identity_cannot_create_session() {
        let mallory = Identity::generate();
        let mut tampered = PublicIdentity::new(&Identity::generate());
        tampered.x25519_public = mallory.x25519_public();
        assert!(tampered.verify().is_err());
        // (If this compiled with a plain `PublicIdentity` passed to `Session::establish`,
        // that alone would be the bug; it doesn't, by design.)
    }

    /// `try_encrypt_direct_message` fails closed -- for a recipient whose binding
    /// doesn't verify (e.g. corrupted/missing key material), it returns
    /// `CannotEncryptForRecipient` rather than any variant that carries plaintext.
    #[test]
    fn missing_or_invalid_recipient_key_fails_closed() {
        let alice = Identity::generate();
        let mallory = Identity::generate();
        let mut tampered_bob = PublicIdentity::new(&Identity::generate());
        tampered_bob.x25519_public = mallory.x25519_public();

        let aad = sample_aad(&alice, &Identity::generate());
        let result = try_encrypt_direct_message(&alice, tampered_bob, &aad, b"hello bob");
        assert_eq!(result.unwrap_err(), DirectCryptoError::CannotEncryptForRecipient);
    }

    /// Structural proof there's no plaintext fallback: the only two possible outcomes of
    /// `try_encrypt_direct_message` are `Ok(DirectCiphertext)` (always encrypted) or
    /// `Err(DirectCryptoError)` -- there is no third variant, and no way to obtain the
    /// input `plaintext` bytes back out of a failed call.
    #[test]
    fn no_plaintext_fallback_exists() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let aad = sample_aad(&alice, &bob);
        let result = try_encrypt_direct_message(&alice, PublicIdentity::new(&bob), &aad, b"hello bob");
        match result {
            Ok(ciphertext) => assert_ne!(ciphertext.ciphertext, b"hello bob"),
            Err(_) => panic!("expected success for a valid recipient in this test"),
        }
    }

    /// Simulates an app restart: reconstructing an identity purely from its persisted
    /// seed must still let it decrypt a message encrypted (by the other party) against
    /// the pre-restart identity's session -- this is what makes the static-session
    /// scheme actually usable across restarts, matching the persistent-identity
    /// guarantee from Milestone 1.
    #[test]
    fn app_restart_still_permits_decryption_with_restored_identity() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let bob_seed = bob.seed();

        let alice_session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);
        let ciphertext = encrypt_direct_message(&alice, &alice_session, &aad, b"hi bob, see you after restart");

        // Bob's identity is now reconstructed purely from the persisted seed, as if the
        // app had been killed and relaunched.
        let restored_bob = Identity::from_seed(bob_seed);
        let restored_bob_session = Session::establish(&restored_bob, &verified(&alice)).unwrap();

        let plaintext = decrypt_direct_message(&restored_bob_session, &alice.node_id(), &aad, &ciphertext).unwrap();
        assert_eq!(plaintext, b"hi bob, see you after restart");
    }

    /// Encrypting the same plaintext twice must produce different ciphertexts (and
    /// different nonces) -- indirectly proving the nonce really is freshly randomized
    /// per call, not reused or derived from something that could repeat.
    #[test]
    fn same_plaintext_encrypted_twice_produces_different_ciphertext() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let session = Session::establish(&alice, &verified(&bob)).unwrap();
        let aad = sample_aad(&alice, &bob);

        let first = encrypt_direct_message(&alice, &session, &aad, b"same message");
        let second = encrypt_direct_message(&alice, &session, &aad, b"same message");

        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }
}
