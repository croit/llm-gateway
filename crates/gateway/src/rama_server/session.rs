// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Hand-rolled session management for the rama server.
//!
//! Replaces `tower-sessions` + `tower-sessions-sqlx-store` with the bare
//! minimum we actually use:
//!
//! - A `sessions` table mapping a random id to a user id and expiry.
//! - A signed cookie format `id.hmac-b64url` — HMAC-SHA256 of the id with
//!   the gateway secret key. Tampering with the id invalidates the HMAC;
//!   the gateway treats the request as anonymous.
//!
//! What we deliberately don't do (and what tower-sessions did):
//! - Sliding expiration. Sessions have a fixed TTL set at creation.
//! - Multiple stores / driver abstraction. SQLite is the only backend.
//! - Cookie payloads with arbitrary user-supplied keys. Just `user_id`.
//!
//! If we ever need any of that we'll add it here, not pull in the crate.

use std::time::Duration;

use hmac::{Hmac, Mac};
use jiff::{SignedDuration, Timestamp};
use rama::http::HeaderMap;
use rama::http::header::COOKIE;
use rand::TryRngCore;
use sha2::Sha256;
use thiserror::Error;

use crate::server::db::Pool;

type HmacSha256 = Hmac<Sha256>;

/// Name of the cookie carrying the session payload.
pub const COOKIE_NAME: &str = "id";

/// Default session lifetime — seven days.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("session secret must be exactly 32 bytes (got {0})")]
    BadSecretLength(usize),
    #[error("cookie HMAC invalid")]
    BadSignature,
    #[error("cookie payload missing `.` separator")]
    Malformed,
}

#[derive(Clone)]
pub struct SessionStore {
    db: Pool,
    secret: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub expires_at: Timestamp,
    /// Browser-reported IANA timezone, set by `POST /api/v0/me/timezone`
    /// after the first authed page load runs `app.js`. None until the
    /// browser tells us, or for sessions created before the migration.
    pub timezone: Option<String>,
}

impl SessionStore {
    /// `secret` is the raw HMAC key — 32 bytes, sourced from
    /// `$GATEWAY_SESSION_KEY` (hex-decoded) at boot.
    pub fn new(db: Pool, secret: [u8; 32]) -> Self {
        Self { db, secret }
    }

    /// Mints a fresh session for `user_id` with the default TTL,
    /// persists it, and returns it. Caller serialises the id into a
    /// cookie via [`Self::sign`].
    pub async fn create(&self, user_id: &str) -> Result<Session, SessionError> {
        self.create_with_ttl(user_id, DEFAULT_TTL).await
    }

    pub async fn create_with_ttl(
        &self,
        user_id: &str,
        ttl: Duration,
    ) -> Result<Session, SessionError> {
        let id = random_session_id();
        let now = Timestamp::now();
        let expires =
            now + SignedDuration::try_from(ttl).unwrap_or(SignedDuration::from_hours(24 * 7));
        sqlx::query(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(now.to_string())
        .bind(expires.to_string())
        .execute(&self.db)
        .await?;
        Ok(Session {
            id,
            user_id: user_id.to_string(),
            expires_at: expires,
            timezone: None,
        })
    }

    /// Looks up a session by id, returning it iff present **and** not
    /// expired. Expired rows are left for a future GC pass; we just hide
    /// them at read time so a clock-skew gap doesn't grant access.
    pub async fn lookup(&self, id: &str) -> Result<Option<Session>, SessionError> {
        let row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT user_id, created_at, expires_at, timezone FROM sessions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id, _created, expires_at, timezone)) = row else {
            return Ok(None);
        };
        let expires_at: Timestamp = expires_at.parse().map_err(|_| SessionError::Malformed)?;
        if expires_at < Timestamp::now() {
            return Ok(None);
        }
        Ok(Some(Session {
            id: id.to_string(),
            user_id,
            expires_at,
            timezone,
        }))
    }

    /// Updates the per-session timezone. Called by
    /// `POST /api/v0/me/timezone` after `app.js` reads
    /// `Intl.DateTimeFormat().resolvedOptions().timeZone` and posts it
    /// up on first authed page load. Per-session because the same user
    /// might be logged in from a laptop in Berlin and a phone in NYC at
    /// the same time. The user-record copy (`users::set_timezone`) holds
    /// the most recent value as a fallback for bearer-authed callers
    /// who never had a session.
    pub async fn set_timezone(&self, id: &str, timezone: &str) -> Result<(), SessionError> {
        sqlx::query("UPDATE sessions SET timezone = ? WHERE id = ?")
            .bind(timezone)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Drops a session row. Used by `/auth/logout`. Returns whether a
    /// row was actually removed (false → already gone).
    pub async fn delete(&self, id: &str) -> Result<bool, SessionError> {
        let r = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Serialises a session id into a signed cookie payload —
    /// `<id>.<hmac-base64url-nopad>`. The HMAC binds the id to our
    /// secret so a client can't forge a session by changing the id.
    pub fn sign(&self, id: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac key length");
        mac.update(id.as_bytes());
        let tag = mac.finalize().into_bytes();
        format!("{id}.{}", base64url_nopad(&tag))
    }

    /// Inverse of `sign` — checks the HMAC and returns the id. Constant
    /// time via `Hmac::verify_slice`.
    pub fn verify<'a>(&self, signed: &'a str) -> Result<&'a str, SessionError> {
        let (id, sig_b64) = signed.split_once('.').ok_or(SessionError::Malformed)?;
        let sig = base64url_decode_nopad(sig_b64).ok_or(SessionError::BadSignature)?;
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac key length");
        mac.update(id.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| SessionError::BadSignature)?;
        Ok(id)
    }

    /// Convenience: pull our cookie out of the request headers, verify
    /// the HMAC, look up the row. `None` for any failure (no cookie,
    /// tampered signature, expired row) — all of those produce the same
    /// "anonymous request" behaviour upstream.
    pub async fn lookup_from_headers(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<Session>, SessionError> {
        let Some(signed) = read_cookie(headers, COOKIE_NAME) else {
            return Ok(None);
        };
        let id = match self.verify(&signed) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        self.lookup(id).await
    }
}

/// Random 32-byte session id, hex-encoded. The HMAC binding means we
/// don't actually need cryptographic-strength ids — a guessing attacker
/// would also have to forge the HMAC — but it's cheap and avoids the
/// risk of accidentally narrowing the space in some future refactor.
fn random_session_id() -> String {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OsRng fill");
    hex_encode(&buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Pull a named cookie out of a `Cookie:` header. Tolerates whitespace
/// after `;`, doesn't try to handle percent-decoding — callers that
/// store user-supplied bytes would have to percent-encode at the call
/// site. (Current callers — session id and theme — don't need it.)
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(COOKIE)?.to_str().ok()?;
    for piece in header.split(';') {
        let piece = piece.trim();
        if let Some((k, v)) = piece.split_once('=')
            && k == name
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Base64url-without-padding encode/decode. The signature is the only
/// thing that needs round-tripping through a cookie value, and `cookie`
/// crate territory is overkill for that. RFC 4648 §5 alphabet:
/// `A-Za-z0-9-_`, no `=`.
fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let n = (rem[0] as u32) << 16 | (rem[1] as u32) << 8;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        _ => unreachable!(),
    }
    out
}

fn base64url_decode_nopad(s: &str) -> Option<Vec<u8>> {
    fn dec(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut chunks = bytes.chunks_exact(4);
    for chunk in chunks.by_ref() {
        let n = (dec(chunk[0])? as u32) << 18
            | (dec(chunk[1])? as u32) << 12
            | (dec(chunk[2])? as u32) << 6
            | (dec(chunk[3])? as u32);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        2 => {
            let n = (dec(rem[0])? as u32) << 18 | (dec(rem[1])? as u32) << 12;
            out.push((n >> 16) as u8);
        }
        3 => {
            let n = (dec(rem[0])? as u32) << 18
                | (dec(rem[1])? as u32) << 12
                | (dec(rem[2])? as u32) << 6;
            out.push((n >> 16) as u8);
            out.push((n >> 8) as u8);
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pool is part of the struct but `sign`/`verify` only touch the
    // HMAC secret, so a `#[tokio::test]` is enough — we open an
    // in-memory sqlite once via `db::open` and reuse it across tests
    // by spinning the runtime each time (cheap; these are unit tests).
    async fn store() -> SessionStore {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .expect("open in-memory sqlite");
        SessionStore::new(pool, [7u8; 32])
    }

    #[test]
    fn base64url_round_trip() {
        for n in 0..200 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 31) as u8).collect();
            let enc = base64url_nopad(&bytes);
            let dec = base64url_decode_nopad(&enc).expect("decode");
            assert_eq!(dec, bytes, "n = {n}");
        }
    }

    #[tokio::test]
    async fn sign_then_verify_round_trips() {
        let store = store().await;
        let id = "abc123";
        let signed = store.sign(id);
        let parsed = store.verify(&signed).expect("verify");
        assert_eq!(parsed, id);
    }

    #[tokio::test]
    async fn tampered_signature_rejected() {
        let store = store().await;
        let mut signed = store.sign("legit-session");
        let dot = signed.rfind('.').unwrap();
        let bytes = unsafe { signed.as_bytes_mut() };
        bytes[dot + 1] = if bytes[dot + 1] == b'A' { b'B' } else { b'A' };
        let err = store.verify(&signed).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature), "{err}");
    }

    #[tokio::test]
    async fn tampered_id_rejected() {
        let store = store().await;
        let signed = store.sign("session-x");
        let sig = signed.split_once('.').unwrap().1;
        let forged = format!("session-y.{sig}");
        let err = store.verify(&forged).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[tokio::test]
    async fn create_lookup_delete_round_trip() {
        let store = store().await;
        // The user_id FK doesn't allow arbitrary strings; seed a user.
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            &store.db,
            &crate::server::db::users::User {
                id: "alice".into(),
                email: "a@x".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();

        let session = store.create("alice").await.unwrap();
        let fetched = store.lookup(&session.id).await.unwrap().unwrap();
        assert_eq!(fetched.user_id, "alice");
        assert_eq!(fetched.id, session.id);

        assert!(store.delete(&session.id).await.unwrap());
        assert!(store.lookup(&session.id).await.unwrap().is_none());
        assert!(!store.delete(&session.id).await.unwrap()); // idempotent
    }

    #[tokio::test]
    async fn lookup_from_headers_finds_signed_cookie() {
        let store = store().await;
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            &store.db,
            &crate::server::db::users::User {
                id: "bob".into(),
                email: "b@x".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();
        let session = store.create("bob").await.unwrap();
        let signed = store.sign(&session.id);

        let mut h = HeaderMap::new();
        h.insert(COOKIE, format!("id={signed}").parse().unwrap());
        let fetched = store.lookup_from_headers(&h).await.unwrap().unwrap();
        assert_eq!(fetched.user_id, "bob");
    }

    #[test]
    fn cookie_parser_handles_whitespace_and_multiple_values() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            "other=foo;  id=value-here;  third=bar".parse().unwrap(),
        );
        assert_eq!(read_cookie(&h, "id").as_deref(), Some("value-here"));
        assert_eq!(read_cookie(&h, "other").as_deref(), Some("foo"));
        assert_eq!(read_cookie(&h, "missing"), None);
    }
}
