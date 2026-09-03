// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! At-rest encryption for per-user MCP OAuth tokens, admin-stored connector
//! client secrets, and upstream backend API keys.
//!
//! These are dynamic, admin/per-user-managed secrets that can't live in env
//! vars the way the gateway's other credentials do — an operator adds a backend
//! (with its key) through the admin UI at runtime, so the key must persist in
//! the database as AES-256-GCM ciphertext rather than a process env var. Each value is encrypted under a fresh
//! random 96-bit nonce; the DB layer keeps the `(nonce, ciphertext)` pair
//! opaquely and never sees plaintext.
//!
//! Key material comes from `$GATEWAY_ENCRYPTION_KEY` (64 hex chars = 32 bytes)
//! when set; otherwise it is derived from the session secret via HMAC-SHA256 so
//! a deployment that already configured `$GATEWAY_SESSION_KEY` gets stable,
//! restart-surviving encryption for free. With neither configured (dev), an
//! ephemeral key is used and a warning logged — stored secrets won't decrypt
//! after a restart (reconnect / re-enter them).

// `aes-gcm` 0.10 pulls `generic-array` 0.14 via `aead`/`crypto-common`, whose
// `GenericArray` re-export carries an "upgrade to generic-array 1.x"
// deprecation we can't act on without bumping the whole crypto stack. Scope the
// allow to this small, self-contained module so `clippy -D warnings` stays
// clean; revisit when `aes-gcm` moves to generic-array 1.x.
#![allow(deprecated)]

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
    /// The key the *previous* derivation label produced from the same session
    /// secret, tried by [`Crypto::open`] when the current key fails.
    ///
    /// The label was renamed from `mcp-token-encryption/v1` to
    /// `at-rest-encryption/v1` when at-rest sealing grew beyond MCP tokens, and
    /// that shipped with no migration: every value sealed before it — backend
    /// API keys, connector client secrets, each user's OAuth tokens — became
    /// undecryptable, and the release notes told operators to re-enter them.
    /// Re-entering an upstream credential is not a migration path, so the old
    /// key is kept and tried as a fallback. [`Self::is_legacy_sealed`] lets a
    /// caller notice and re-seal.
    ///
    /// `None` when the key came from `$GATEWAY_ENCRYPTION_KEY`: an explicit key
    /// is used verbatim, so no label was ever involved and there is nothing to
    /// fall back to.
    legacy: Option<[u8; 32]>,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.write_str("Crypto(<key elided>)")
    }
}

/// One encrypted value: a 96-bit nonce and the GCM ciphertext (which includes
/// the auth tag). Both are stored as SQLite BLOBs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Domain-separation label for the at-rest key.
///
/// Changing it derives a different key and orphans everything already sealed,
/// so it is not a knob: a deliberate rotation means moving the current value to
/// [`LEGACY_LABEL`] and writing the re-seal pass, not editing this in place.
pub(crate) const LABEL: &[u8] = b"croit-llm-gateway/at-rest-encryption/v1";

/// The label this key had while at-rest sealing was only used for MCP tokens.
/// Kept so values sealed under it stay readable — see [`Crypto::legacy`].
pub(crate) const LEGACY_LABEL: &[u8] = b"croit-llm-gateway/mcp-token-encryption/v1";

/// HKDF-lite: HMAC-SHA256(session_secret, label).
pub(crate) fn derive(session_secret: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(label);
    let derived = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&derived);
    key
}

fn open_with(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let nonce_arr: [u8; 12] = nonce.try_into().map_err(|_| CryptoError::Decrypt)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::Decrypt)?;
    cipher
        .decrypt(&GenericArray::from(nonce_arr), ciphertext)
        .map_err(|_| CryptoError::Decrypt)
}

impl Crypto {
    /// Build from explicit 32-byte key material (used by tests).
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key, legacy: None }
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
        Self { key, legacy: None }
    }

    /// Resolve the key: `$GATEWAY_ENCRYPTION_KEY` (64 hex chars) wins; otherwise
    /// derive a stable key from the session secret; if that's all-zero
    /// (ephemeral session key path) we still derive deterministically from it
    /// so the process is internally consistent for its lifetime.
    pub fn from_env_or_session(session_secret: &[u8; 32]) -> Self {
        if let Ok(raw) = std::env::var("GATEWAY_ENCRYPTION_KEY")
            && !raw.is_empty()
        {
            match hex_decode(&raw) {
                Some(bytes) if bytes.len() == 32 => {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    return Self { key, legacy: None };
                }
                _ => {
                    tracing::warn!(
                        "GATEWAY_ENCRYPTION_KEY must be 64 hex chars (32 bytes); ignoring it and \
                         deriving the at-rest encryption key from the session secret instead"
                    );
                }
            }
        }
        Self::from_session(session_secret)
    }

    /// The derived path on its own, without consulting the environment.
    ///
    /// HKDF-lite: HMAC-SHA256(session_secret, [`LABEL`]). The label names this
    /// key's purpose and is what separates it from any other key derived from
    /// the same session secret. [`LEGACY_LABEL`] is derived alongside it so
    /// values written before the rename still open — see [`Self::legacy`].
    pub fn from_session(session_secret: &[u8; 32]) -> Self {
        Self {
            key: derive(session_secret, LABEL),
            legacy: Some(derive(session_secret, LEGACY_LABEL)),
        }
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
        if let Ok(plain) = open_with(&self.key, nonce, ciphertext) {
            return Ok(plain);
        }
        // Sealed before the derivation label was renamed. Opening it is the
        // whole point — the alternative shipped once already, and it was
        // "re-enter every upstream credential".
        match self.legacy {
            Some(legacy) => open_with(&legacy, nonce, ciphertext),
            None => Err(CryptoError::Decrypt),
        }
    }

    /// Whether `nonce`/`ciphertext` needs the pre-rename key, i.e. whether it
    /// should be re-sealed. `false` for anything the current key opens, and for
    /// anything neither key opens (that is not a migration, it is a wrong key).
    pub fn is_legacy_sealed(&self, nonce: &[u8], ciphertext: &[u8]) -> bool {
        if open_with(&self.key, nonce, ciphertext).is_ok() {
            return false;
        }
        self.legacy
            .is_some_and(|legacy| open_with(&legacy, nonce, ciphertext).is_ok())
    }

    /// [`Self::is_legacy_sealed`] for the `"<nonce>.<ciphertext>"` string form.
    pub fn is_legacy_sealed_string(&self, stored: &str) -> bool {
        let Some((nonce, ciphertext)) = stored.split_once('.') else {
            return false;
        };
        let Some(nonce) = URL_SAFE_NO_PAD.decode(nonce).ok() else {
            return false;
        };
        let Some(ciphertext) = URL_SAFE_NO_PAD.decode(ciphertext).ok() else {
            return false;
        };
        self.is_legacy_sealed(&nonce, &ciphertext)
    }

    /// Convenience: decrypt to a UTF-8 string.
    pub fn open_str(&self, nonce: &[u8], ciphertext: &[u8]) -> Result<String, CryptoError> {
        let bytes = self.open(nonce, ciphertext)?;
        String::from_utf8(bytes).map_err(|_| CryptoError::Decrypt)
    }

    /// Seal a string into the single-column `"<nonce>.<ciphertext>"` form
    /// (base64url, unpadded) used by every secret that lives in an
    /// `app_settings` row rather than in its own pair of BLOB columns —
    /// the VAPID private key, the Brave API key, the OIDC client secret.
    pub fn seal_to_string(&self, plaintext: &str) -> Result<String, CryptoError> {
        self.seal_bytes_to_string(plaintext.as_bytes())
    }

    /// [`Self::seal_to_string`] for material that is not UTF-8 — a raw private
    /// key, say. Same stored shape, so the two are interchangeable at rest.
    pub fn seal_bytes_to_string(&self, plaintext: &[u8]) -> Result<String, CryptoError> {
        let sealed = self.seal(plaintext)?;
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&sealed.nonce),
            URL_SAFE_NO_PAD.encode(&sealed.ciphertext)
        ))
    }

    /// Inverse of [`Self::seal_to_string`]. `None` covers both a malformed
    /// value and one sealed under a different key — callers treat either as
    /// "not configured", which is the actionable truth, and log once.
    pub fn open_from_string(&self, stored: &str) -> Option<String> {
        let bytes = self.open_bytes_from_string(stored)?;
        String::from_utf8(bytes).ok()
    }

    /// Inverse of [`Self::seal_bytes_to_string`].
    pub fn open_bytes_from_string(&self, stored: &str) -> Option<Vec<u8>> {
        let (nonce, ciphertext) = stored.split_once('.')?;
        let nonce = URL_SAFE_NO_PAD.decode(nonce).ok()?;
        let ciphertext = URL_SAFE_NO_PAD.decode(ciphertext).ok()?;
        self.open(&nonce, &ciphertext).ok()
    }
}

/// Decode a lowercase/uppercase hex string into bytes. `None` on odd length or
/// a non-hex digit. The inverse of [`hex_encode`], and the one home for both.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex. The one home for it — `auth::token` and `server::setup` both
/// hash identifiers into this form, and a second implementation is a second
/// thing to get subtly wrong.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

/// Lowercase-hex SHA-256. Used wherever a secret is stored as a digest rather
/// than as itself — API tokens, webhook secrets, the setup recovery token.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex_encode(&Sha256::digest(bytes))
}

/// `n` random bytes, lowercase hex — the shape every opaque credential in the
/// gateway takes. Panics only if the OS RNG fails, which is not a condition
/// any caller can sensibly handle.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG must succeed");
    hex_encode(&bytes)
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

    /// A value sealed before the derivation label was renamed must still open.
    ///
    /// This is the regression that matters: `7e7ec42` renamed the label with no
    /// migration, so every backend API key, connector client secret and stored
    /// OAuth token from before it became undecryptable — and the release notes
    /// answered "re-enter them". Verified against a real database before this
    /// fix: the July-era `backends.api_key_ct` row opened under
    /// `mcp-token-encryption/v1` and not under the current label.
    #[test]
    fn a_value_sealed_under_the_previous_label_still_opens() {
        let session = [42u8; 32];
        // What the pre-rename build would have written.
        let old = Crypto::from_key(derive(&session, LEGACY_LABEL));
        let sealed = old.seal(b"sk-upstream-secret").unwrap();

        // What this build derives — the current label, legacy kept as fallback.
        let now = Crypto {
            key: derive(&session, LABEL),
            legacy: Some(derive(&session, LEGACY_LABEL)),
        };
        assert_ne!(now.key, old.key, "the labels must derive different keys");
        assert_eq!(
            now.open(&sealed.nonce, &sealed.ciphertext).unwrap(),
            b"sk-upstream-secret",
            "a legacy-sealed value must still be readable"
        );
        assert!(
            now.is_legacy_sealed(&sealed.nonce, &sealed.ciphertext),
            "and must be reported as needing a re-seal"
        );
    }

    #[test]
    fn a_value_sealed_under_the_current_label_needs_no_reseal() {
        let session = [42u8; 32];
        let now = Crypto {
            key: derive(&session, LABEL),
            legacy: Some(derive(&session, LEGACY_LABEL)),
        };
        let sealed = now.seal(b"fresh").unwrap();
        assert_eq!(
            now.open(&sealed.nonce, &sealed.ciphertext).unwrap(),
            b"fresh"
        );
        assert!(!now.is_legacy_sealed(&sealed.nonce, &sealed.ciphertext));
    }

    /// The fallback must not weaken the failure case: a genuinely wrong key
    /// still fails, and is not misreported as a migration.
    #[test]
    fn an_unrelated_key_is_not_mistaken_for_a_legacy_seal() {
        let stranger = Crypto::from_key([9u8; 32]);
        let sealed = stranger.seal(b"other deployment").unwrap();

        let session = [42u8; 32];
        let now = Crypto {
            key: derive(&session, LABEL),
            legacy: Some(derive(&session, LEGACY_LABEL)),
        };
        assert!(now.open(&sealed.nonce, &sealed.ciphertext).is_err());
        assert!(!now.is_legacy_sealed(&sealed.nonce, &sealed.ciphertext));
    }

    /// An explicit `$GATEWAY_ENCRYPTION_KEY` is used verbatim, so there is no
    /// label and nothing to fall back to — a legacy value must NOT open, or the
    /// fallback would be silently widening which keys can read a database.
    #[test]
    fn an_explicit_key_has_no_legacy_fallback() {
        let session = [42u8; 32];
        let old = Crypto::from_key(derive(&session, LEGACY_LABEL));
        let sealed = old.seal(b"secret").unwrap();

        let explicit = Crypto::from_key([1u8; 32]);
        assert!(explicit.legacy.is_none());
        assert!(explicit.open(&sealed.nonce, &sealed.ciphertext).is_err());
    }

    #[test]
    fn the_string_form_reports_a_legacy_seal_too() {
        let session = [42u8; 32];
        let old = Crypto::from_key(derive(&session, LEGACY_LABEL));
        let stored = old.seal_to_string("vapid-ish").unwrap();

        let now = Crypto {
            key: derive(&session, LABEL),
            legacy: Some(derive(&session, LEGACY_LABEL)),
        };
        assert_eq!(now.open_from_string(&stored).as_deref(), Some("vapid-ish"));
        assert!(now.is_legacy_sealed_string(&stored));
        assert!(!now.is_legacy_sealed_string("not-even-two-parts"));
    }
}
