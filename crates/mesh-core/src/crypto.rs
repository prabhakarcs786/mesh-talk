//! Symmetric channel encryption. A "channel" is like a walkie-talkie frequency: everyone
//! who knows the passphrase can read messages on it. This keeps the MVP simple; a future
//! version can add per-recipient X25519 key exchange for private 1:1 messages.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    Key, XChaCha20Poly1305, XNonce,
};
use rand::RngCore;

pub struct ChannelKey(XChaCha20Poly1305);

impl ChannelKey {
    pub fn from_passphrase(passphrase: &str) -> Self {
        let hash = blake3::hash(passphrase.as_bytes());
        let key = Key::from_slice(hash.as_bytes());
        Self(XChaCha20Poly1305::new(key))
    }

    /// Encrypts plaintext, returning (ciphertext, nonce).
    pub fn encrypt(&self, plaintext: &[u8]) -> (Vec<u8>, [u8; 24]) {
        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .0
            .encrypt(nonce, plaintext)
            .expect("encryption cannot fail for valid inputs");
        (ciphertext, nonce_bytes)
    }

    pub fn decrypt(&self, ciphertext: &[u8], nonce_bytes: &[u8; 24]) -> Option<Vec<u8>> {
        let nonce = XNonce::from_slice(nonce_bytes);
        self.0.decrypt(nonce, ciphertext).ok()
    }
}
