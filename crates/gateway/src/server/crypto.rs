// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! At-rest encryption for per-user MCP OAuth tokens and admin-stored connector
//! client secrets.
//!
//! These are dynamic, per-user/per-connector secrets that can't live in env
//! vars the way the gateway's other credentials do, so they're stored in the
//! database as AES-256-GCM ciphertext. Each value is encrypted under a fresh
//! random 96-bit nonce; the DB layer keeps the `(nonce, ciphertext)` pair
//! opaquely and never sees plaintext.
//!
//! Key material comes from `$GATEWAY_MCP_KEY` (64 hex chars = 32 bytes) when
//! set; otherwise it is derived from the session secret via HMAC-SHA256 so a
//! deployment that already configured `$GATEWAY_SESSION_KEY` gets stable,
//! restart-surviving encryption for free. With neither configured (dev), an
//! ephemeral key is used and a warning logged — stored connections won't
//! decrypt after a restart and the user simply reconnects.

// `aes-gcm` 0.10 pulls `generic-array` 0.14 via `aead`/`crypto-common`, whose
// `GenericArray` re-export carries an "upgrade to generic-array 1.x"
// deprecation we can't act on without bumping the whole crypto stack. Scope the
// allow to this small, self-contained module so `clippy -D warnings` stays
// clean; revisit when `aes-gcm` moves to generic-array 1.x.
#![allow(deprecated)]

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use hmac::{Hmac, Mac};
use rand::TryRngCore;
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed (wrong key, or value was stored under a different key)")]
    Decrypt,
    #[error("generating nonce: {0}")]
    Nonce(String),
}

/// A loaded encryption key wrapped behind AES-256-GCM. Cheap to clone (holds a
/// 32-byte key); share it via `Arc` in `AppState`.
#[derive(Clone)]
pub struct Crypto {
    key: [u8; 32],
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.write_str("Crypto(<key elided>)")
    }
}

/// One encrypted value: a 96-bit nonce and the GCM ciphertext (which includes
/// the auth tag). Both are stored as SQLite BLOBs.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl Crypto {
    /// Build from explicit 32-byte key material (used by tests).
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// A random, process-lifetime key. Used as the `AppState::new` default so
    /// the type is always present; production overrides it via
    /// [`Crypto::from_env_or_session`]. Stored secrets sealed under an
    /// ephemeral key won't survive a restart — acceptable for tests/dev.
    pub fn ephemeral() -> Self {
        let mut key = [0u8; 32];
        // OsRng failing is catastrophic and vanishingly rare; fall back to a
        // fixed key rather than panic so a misconfigured host still boots.
        if rand::rngs::OsRng.try_fill_bytes(&mut key).is_err() {
            key = [0u8; 32];
        }
        Self { key }
    }

    /// Resolve the key: `$GATEWAY_MCP_KEY` (64 hex chars) wins; otherwise
    /// derive a stable key from the session secret; if that's all-zero
    /// (ephemeral session key path) we still derive deterministically from it
    /// so the process is internally consistent for its lifetime.
    pub fn from_env_or_session(session_secret: &[u8; 32]) -> Self {
        if let Ok(raw) = std::env::var("GATEWAY_MCP_KEY")
            && !raw.is_empty()
        {
            match hex_decode(&raw) {
                Some(bytes) if bytes.len() == 32 => {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    return Self { key };
                }
                _ => {
                    tracing::warn!(
                        "GATEWAY_MCP_KEY must be 64 hex chars (32 bytes); ignoring it and \
                         deriving the MCP encryption key from the session secret instead"
                    );
                }
            }
        }
        // HKDF-lite: HMAC-SHA256(session_secret, domain-separation label).
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(session_secret)
            .expect("HMAC accepts any key length");
        mac.update(b"croit-llm-gateway/mcp-token-encryption/v1");
        let derived = mac.finalize().into_bytes();
        let mut key = [0u8; 32];
        key.copy_from_slice(&derived);
        Self { key }
    }

    /// Encrypt `plaintext` under a fresh random nonce.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| CryptoError::Encrypt)?;
        let mut nonce_bytes = [0u8; 12];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|e| CryptoError::Nonce(e.to_string()))?;
        // The nonce GenericArray size is inferred (U12) from `encrypt`'s
        // expected `&Nonce<Aes256Gcm>` argument, so we never name the alias.
        let nonce = GenericArray::from(nonce_bytes);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::Encrypt)?;
        Ok(Sealed {
            nonce: nonce_bytes.to_vec(),
            ciphertext,
        })
    }

    /// Convenience: seal a string.
    pub fn seal_str(&self, plaintext: &str) -> Result<Sealed, CryptoError> {
        self.seal(plaintext.as_bytes())
    }

    /// Decrypt a `(nonce, ciphertext)` pair.
    pub fn open(&self, nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce_arr: [u8; 12] = nonce.try_into().map_err(|_| CryptoError::Decrypt)?;
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| CryptoError::Decrypt)?;
        let nonce = GenericArray::from(nonce_arr);
        cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| CryptoError::Decrypt)
    }

    /// Convenience: decrypt to a UTF-8 string.
    pub fn open_str(&self, nonce: &[u8], ciphertext: &[u8]) -> Result<String, CryptoError> {
        let bytes = self.open(nonce, ciphertext)?;
        String::from_utf8(bytes).map_err(|_| CryptoError::Decrypt)
    }
}

/// Decode a lowercase/uppercase hex string into bytes. `None` on odd length or
/// a non-hex digit. Local copy so the crypto module is self-contained.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto() -> Crypto {
        Crypto::from_key([7u8; 32])
    }

    #[test]
    fn round_trips_a_token() {
        let c = crypto();
        let sealed = c.seal_str("ya29.secret-access-token").unwrap();
        // Nonce is 96-bit; ciphertext carries the 16-byte GCM tag so it's
        // strictly longer than the plaintext.
        assert_eq!(sealed.nonce.len(), 12);
        assert!(sealed.ciphertext.len() > "ya29.secret-access-token".len());
        let back = c.open_str(&sealed.nonce, &sealed.ciphertext).unwrap();
        assert_eq!(back, "ya29.secret-access-token");
    }

    #[test]
    fn nonces_differ_per_seal() {
        let c = crypto();
        let a = c.seal_str("same").unwrap();
        let b = c.seal_str("same").unwrap();
        assert_ne!(a.nonce, b.nonce, "each seal must use a fresh nonce");
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let a = Crypto::from_key([1u8; 32]);
        let b = Crypto::from_key([2u8; 32]);
        let sealed = a.seal_str("secret").unwrap();
        assert!(b.open(&sealed.nonce, &sealed.ciphertext).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let c = crypto();
        let mut sealed = c.seal_str("secret").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(c.open(&sealed.nonce, &sealed.ciphertext).is_err());
    }

    #[test]
    fn derivation_from_session_is_stable() {
        let secret = [9u8; 32];
        let a = Crypto::from_env_or_session(&secret);
        let b = Crypto::from_env_or_session(&secret);
        let sealed = a.seal_str("x").unwrap();
        // Same session secret → same derived key → b can open a's ciphertext.
        assert_eq!(b.open_str(&sealed.nonce, &sealed.ciphertext).unwrap(), "x");
    }

    #[test]
    fn bad_nonce_length_rejected() {
        let c = crypto();
        assert!(c.open(&[0u8; 8], &[0u8; 32]).is_err());
    }
}
