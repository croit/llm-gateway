// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! PKCE (RFC 7636 S256) helpers — kept in `cli` rather than `shared` because
//! the CLI is the only PKCE consumer; the gateway only verifies the resulting
//! challenge.
//!
//! Generate a verifier (32 random bytes, base64url-no-pad → 43 chars) and
//! its SHA-256 challenge. Both safe to send across HTTP.

use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::Digest;

const VERIFIER_BYTES: usize = 32;

/// Returns `(verifier, challenge)`.
pub fn new_pair() -> (String, String) {
    let mut buf = [0u8; VERIFIER_BYTES];
    OsRng.try_fill_bytes(&mut buf).expect("OS RNG must succeed");
    let verifier = base64url_no_pad(&buf);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64url_no_pad(&digest);
    (verifier, challenge)
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() >= 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_is_well_formed() {
        let (v, c) = new_pair();
        // 32 bytes → 43 base64url chars.
        assert_eq!(v.len(), 43);
        // SHA-256 = 32 bytes → 43 base64url chars.
        assert_eq!(c.len(), 43);
        for s in [&v, &c] {
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "non-url-safe char in {s}"
            );
        }
    }

    #[test]
    fn pair_is_unique() {
        let (a, _) = new_pair();
        let (b, _) = new_pair();
        assert_ne!(a, b);
    }

    #[test]
    fn challenge_matches_rfc7636_example() {
        // Same example used in the server-side test for the inverse direction.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = sha2::Sha256::digest(verifier.as_bytes());
        let challenge = base64url_no_pad(&digest);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
