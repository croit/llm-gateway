// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway-token primitives.
//!
//! - Tokens are opaque random strings prefixed `gwk_`, 32 random bytes encoded
//!   hex (so the wire form looks like `gwk_<64 hex chars>`). 256-bit entropy.
//! - In the DB we store **SHA-256 hex** of the bearer string, not the plaintext
//!   and not an argon2id hash. The token is high-entropy random, so SHA-256 is
//!   enough: collisions need ~2^128 work, brute-force needs ~2^256. argon2id's
//!   cost is wasted on a random opaque token, and lookup must be fast for /v1/*.

use rand::TryRngCore;
use rand::rngs::OsRng;

pub const TOKEN_PREFIX: &str = "gwk_";
pub const TOKEN_BYTES: usize = 32;
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// Mints a fresh token. Returns `(plaintext, sha256_hex)`. The plaintext is
/// shown to the user exactly once; the hash is what gets persisted.
pub fn mint() -> (String, String) {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG must succeed");
    let plaintext = format!("{TOKEN_PREFIX}{}", hex_encode(&bytes));
    let hash = sha256_hex(plaintext.as_bytes());
    (plaintext, hash)
}

/// Validates the surface shape of a bearer string and returns its SHA-256 hex
/// for DB lookup. Rejects anything that doesn't look like our format so we
/// never run a hash + DB query on obvious garbage.
pub fn hash_bearer(bearer: &str) -> Option<String> {
    if !bearer.starts_with(TOKEN_PREFIX) {
        return None;
    }
    let tail = &bearer[TOKEN_PREFIX.len()..];
    if tail.len() != TOKEN_HEX_LEN || !tail.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(sha256_hex(bearer.as_bytes()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex_encode(&sha2::Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_returns_well_formed_token_and_matching_hash() {
        let (plaintext, hash) = mint();
        assert!(plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(plaintext.len(), TOKEN_PREFIX.len() + TOKEN_HEX_LEN);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash_bearer(&plaintext).unwrap(), hash);
    }

    #[test]
    fn mint_is_unique() {
        let (a, _) = mint();
        let (b, _) = mint();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_bearer_rejects_wrong_prefix() {
        assert!(hash_bearer("sk_1234567890abcdef".repeat(8).as_str()).is_none());
        assert!(hash_bearer("").is_none());
    }

    #[test]
    fn hash_bearer_rejects_wrong_length() {
        assert!(hash_bearer("gwk_abc").is_none());
        let long = format!("gwk_{}", "a".repeat(TOKEN_HEX_LEN + 1));
        assert!(hash_bearer(&long).is_none());
    }

    #[test]
    fn hash_bearer_rejects_non_hex() {
        let bad = format!("gwk_{}", "z".repeat(TOKEN_HEX_LEN));
        assert!(hash_bearer(&bad).is_none());
    }

    #[test]
    fn hash_bearer_is_deterministic() {
        let (plaintext, hash) = mint();
        assert_eq!(hash_bearer(&plaintext).unwrap(), hash);
        assert_eq!(hash_bearer(&plaintext).unwrap(), hash);
    }
}
