// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Message Encryption for Web Push (RFC 8291) with the `aes128gcm` content
//! coding (RFC 8188).
//!
//! Given a subscription's `p256dh` (the user agent's P-256 public key) and
//! `auth` secret, produce the single-record encrypted body a push service
//! delivers verbatim to the browser's service worker:
//!
//! 1. Generate an ephemeral P-256 keypair (the "application server" key).
//! 2. ECDH(ephemeral_private, ua_public) → shared secret.
//! 3. `IKM = HKDF(salt=auth, ikm=ecdh, info="WebPush: info\0"‖ua_pub‖as_pub, 32)`.
//! 4. With a random 16-byte `salt`:
//!    `CEK = HKDF(salt, IKM, "Content-Encoding: aes128gcm\0", 16)` and
//!    `NONCE = HKDF(salt, IKM, "Content-Encoding: nonce\0", 12)`.
//! 5. Encrypt `plaintext‖0x02` (the RFC 8188 last-record delimiter) under
//!    AES-128-GCM.
//! 6. Frame it: `salt(16) ‖ rs(u32) ‖ idlen(u8=65) ‖ as_pub(65) ‖ ciphertext`.
//!
//! Only the AES-GCM half touches the deprecated `generic-array` 0.14 re-export
//! (same story as `server::crypto`), so the allow is scoped to this module.
#![allow(deprecated)]

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit};
use hkdf::Hkdf;
use p256::PublicKey;
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand::{Rng, TryRngCore};
use sha2::Sha256;

/// The `rs` (record size) we advertise. Our payloads are a few hundred bytes
/// at most, so a single 4 KiB record always suffices.
const RECORD_SIZE: u32 = 4096;

/// Length of an uncompressed P-256 point (`0x04 ‖ X ‖ Y`).
const POINT_LEN: usize = 65;

#[derive(Debug, thiserror::Error)]
pub enum EncryptError {
    #[error("client p256dh is not a valid P-256 public key")]
    BadClientKey,
    #[error("generating ephemeral key / salt: {0}")]
    Rng(String),
    #[error("HKDF expand failed")]
    Hkdf,
    #[error("AES-128-GCM encryption failed")]
    Aead,
}

/// Encrypt `plaintext` for a subscription. `ua_public` is the raw 65-byte
/// client key (decoded from `p256dh`); `auth` is the raw 16-byte client auth
/// secret. Returns the full `aes128gcm` body to POST to the push endpoint.
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, EncryptError> {
    // Fresh ephemeral keypair. A uniformly random 32-byte scalar is a valid
    // secret key with overwhelming probability; the loop covers the ~2^-32
    // (really ~2^-128) chance of hitting 0 or ≥ n.
    let mut ephemeral = None;
    for _ in 0..8 {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|e| EncryptError::Rng(e.to_string()))?;
        if let Ok(sk) = SecretKey::from_slice(&bytes) {
            ephemeral = Some(sk);
            break;
        }
    }
    let ephemeral = ephemeral.ok_or_else(|| EncryptError::Rng("no valid scalar".into()))?;

    // RFC 8188 content-encoding salt: 16 random bytes from OsRng, generated as
    // a returned value rather than by filling a zero buffer. The buffer-fill
    // form (`let mut salt = [0u8; 16]; OsRng.try_fill_bytes(&mut salt)`) leaves
    // an all-zero literal in the salt's data-flow, which static analysis (CodeQL
    // `rust/hard-coded-cryptographic-value`) reports as a hard-coded salt — it
    // doesn't model the in-place `&mut` overwrite. Generating the value keeps
    // the salt provably literal-free. `unwrap_err()` adapts the fallible
    // `OsRng` into an infallible `RngCore` that panics only on a catastrophic
    // OS-RNG failure (see `server::crypto`).
    let salt: [u8; 16] = rand::rngs::OsRng.unwrap_err().random();

    encrypt_with(ua_public, auth, plaintext, &ephemeral, salt)
}

/// The deterministic core, split out so tests can pin the ephemeral key + salt
/// and round-trip the result. Callers use [`encrypt`].
fn encrypt_with(
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
    ephemeral: &SecretKey,
    salt: [u8; 16],
) -> Result<Vec<u8>, EncryptError> {
    let ua_key = PublicKey::from_sec1_bytes(ua_public).map_err(|_| EncryptError::BadClientKey)?;
    let as_public_point = ephemeral.public_key().to_encoded_point(false);
    let as_public = as_public_point.as_bytes();

    // ECDH → the raw shared X coordinate.
    let shared = p256::ecdh::diffie_hellman(ephemeral.to_nonzero_scalar(), ua_key.as_affine());

    // Step 1: combine the ECDH secret with the client auth secret, binding
    // both public keys into the info string (RFC 8291 §3.4).
    let mut key_info = Vec::with_capacity(14 + POINT_LEN + POINT_LEN);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes().as_slice())
        .expand(&key_info, &mut ikm)
        .map_err(|_| EncryptError::Hkdf)?;

    // Step 2: the RFC 8188 content-encryption key + nonce, salted with our
    // random per-message salt.
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| EncryptError::Hkdf)?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| EncryptError::Hkdf)?;

    // Single record: plaintext followed by the last-record delimiter 0x02.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);

    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| EncryptError::Aead)?;
    let ciphertext = cipher
        .encrypt(&GenericArray::from(nonce), record.as_slice())
        .map_err(|_| EncryptError::Aead)?;

    // Frame: salt(16) ‖ rs(u32 BE) ‖ idlen(u8) ‖ keyid(as_public) ‖ ciphertext.
    let mut body = Vec::with_capacity(16 + 4 + 1 + POINT_LEN + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(POINT_LEN as u8);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decrypt an `aes128gcm` Web Push body from the receiver's side, so the
    /// test can assert `decrypt(encrypt(m)) == m` end-to-end. This mirrors what
    /// the browser does: parse the header, ECDH with the *client* private key
    /// against the ephemeral key in the body, re-derive CEK/NONCE, AES-GCM-open,
    /// then strip the 0x02 delimiter.
    fn decrypt(body: &[u8], ua_secret: &SecretKey, auth: &[u8]) -> Vec<u8> {
        let salt = &body[0..16];
        let idlen = body[20] as usize;
        assert_eq!(idlen, POINT_LEN, "keyid should be an uncompressed point");
        let as_public = &body[21..21 + idlen];
        let ciphertext = &body[21 + idlen..];

        let as_key = PublicKey::from_sec1_bytes(as_public).unwrap();
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_key.as_affine());

        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let mut key_info = Vec::new();
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(ua_public.as_bytes());
        key_info.extend_from_slice(as_public);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes().as_slice())
            .expand(&key_info, &mut ikm)
            .unwrap();

        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();

        let cipher = Aes128Gcm::new_from_slice(&cek).unwrap();
        let mut plain = cipher
            .decrypt(&GenericArray::from(nonce), ciphertext)
            .unwrap();
        assert_eq!(plain.pop(), Some(0x02), "last-record delimiter");
        plain
    }

    /// A fixed client keypair + auth secret standing in for a browser
    /// subscription.
    fn client() -> (SecretKey, [u8; 16]) {
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        (sk, [3u8; 16])
    }

    #[test]
    fn round_trips_a_payload() {
        let (ua_secret, auth) = client();
        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let msg = br#"{"title":"Turn complete","body":"Your answer is ready"}"#;
        let body = encrypt(ua_public.as_bytes(), &auth, msg).unwrap();
        // Header is salt(16)+rs(4)+idlen(1)+point(65) = 86 bytes, then a
        // non-empty GCM ciphertext (payload+delimiter+16-byte tag).
        assert!(body.len() > 86 + 16);
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        let got = decrypt(&body, &ua_secret, &auth);
        assert_eq!(got, msg);
    }

    #[test]
    fn distinct_messages_use_distinct_salts_and_bodies() {
        let (ua_secret, auth) = client();
        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let a = encrypt(ua_public.as_bytes(), &auth, b"same").unwrap();
        let b = encrypt(ua_public.as_bytes(), &auth, b"same").unwrap();
        assert_ne!(&a[0..16], &b[0..16], "fresh random salt per message");
        assert_ne!(a, b);
        // Both still decrypt back to the plaintext.
        assert_eq!(decrypt(&a, &ua_secret, &auth), b"same");
        assert_eq!(decrypt(&b, &ua_secret, &auth), b"same");
    }

    #[test]
    fn wrong_client_key_cannot_decrypt() {
        let (ua_secret, auth) = client();
        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let body = encrypt(ua_public.as_bytes(), &auth, b"secret").unwrap();
        // A different receiver key derives a different CEK → GCM tag check fails.
        let other = SecretKey::from_slice(&[9u8; 32]).unwrap();
        let as_public = &body[21..21 + POINT_LEN];
        let as_key = PublicKey::from_sec1_bytes(as_public).unwrap();
        let shared = p256::ecdh::diffie_hellman(other.to_nonzero_scalar(), as_key.as_affine());
        let ua_pub = other.public_key().to_encoded_point(false);
        let mut key_info = Vec::new();
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(ua_pub.as_bytes());
        key_info.extend_from_slice(as_public);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes().as_slice())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let hk = Hkdf::<Sha256>::new(Some(&body[0..16]), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
        let cipher = Aes128Gcm::new_from_slice(&cek).unwrap();
        assert!(
            cipher
                .decrypt(&GenericArray::from(nonce), &body[21 + POINT_LEN..])
                .is_err()
        );
    }

    #[test]
    fn rejects_a_bad_client_key() {
        let err = encrypt(&[0u8; 10], &[0u8; 16], b"x").unwrap_err();
        assert!(matches!(err, EncryptError::BadClientKey));
    }
}
