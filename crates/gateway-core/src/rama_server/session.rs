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
//! Expiry is a *sliding idle timeout*: a session dies `ttl` after it was
//! last used, not after it was created. [`SessionStore::lookup`] pushes
//! `expires_at` forward once a session is past its half-life, so anyone
//! who visits at least once per `ttl` never has to log in again. The DB
//! row is the authority — the cookie itself carries a much longer
//! `Max-Age` (see [`COOKIE_MAX_AGE`]) purely so it survives a browser or
//! laptop restart.
//!
//! What we deliberately don't do (and what tower-sessions did):
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

/// Default *idle* session lifetime — 30 days since last use. Overridable
/// per deployment via `gateway.session_ttl_days`. Because `lookup`
/// renews (see [`SessionStore::lookup`]) this is the maximum time you
/// can stay away before the gateway asks you to sign in again, not a
/// countdown from login.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// How long the browser is asked to keep the cookie. Deliberately far
/// longer than the session TTL: the DB row decides when a session is
/// dead, and a cookie that outlives it just resolves to "anonymous".
/// Without an explicit `Max-Age` the cookie would be a *browser-session*
/// cookie — dropped on browser quit / laptop restart, which forced a
/// fresh login even though the server-side session was still valid.
///
/// 400 days is the ceiling Chrome and Firefox clamp cookie lifetimes to
/// (RFC 6265bis §5.5), so anything larger buys nothing.
pub const COOKIE_MAX_AGE: Duration = Duration::from_secs(60 * 60 * 24 * 400);

/// Absolute session lifetime — 90 days from *creation*, no matter how
/// actively the session is used. OWASP's session-management guidance
/// pairs an idle timeout with an absolute one so a leaked cookie can't be
/// kept alive indefinitely by using it; it also forces a periodic trip
/// through the IdP, which is the only moment group/role claims are
/// re-read. Overridable via `gateway.session_absolute_max_days`.
pub const DEFAULT_ABSOLUTE_MAX: Duration = Duration::from_secs(60 * 60 * 24 * 90);

/// Lifetime of an *impersonation* session — 8 hours, fixed, never renewed
/// (see [`SessionStore::lookup`]). Ordinary logins want to last; acting as
/// somebody else is a short debugging errand, and the long sliding window
/// that's right for a personal login would be wrong here.
pub const IMPERSONATION_TTL: Duration = Duration::from_secs(60 * 60 * 8);

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
    /// Idle timeout applied to new sessions and to every renewal. See
    /// [`DEFAULT_TTL`]; set from config at boot via [`SessionStore::with_ttl`].
    ttl: Duration,
    /// Hard ceiling on a session's total lifetime, measured from
    /// `created_at`. See [`DEFAULT_ABSOLUTE_MAX`].
    absolute_max: Duration,
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
    /// Set when this session is an admin *impersonating* another user:
    /// `user_id` is the target being acted as, `impersonator_id` is the
    /// admin who started it. None for ordinary sessions. Drives the
    /// persistent impersonation banner and the `/impersonate/stop` route.
    /// See `migrations/0018_impersonation.sql`.
    pub impersonator_id: Option<String>,
}

impl SessionStore {
    /// `secret` is the raw HMAC key — 32 bytes, sourced from
    /// `$GATEWAY_SESSION_KEY` (hex-decoded) at boot.
    pub fn new(db: Pool, secret: [u8; 32]) -> Self {
        Self {
            db,
            secret,
            ttl: DEFAULT_TTL,
            absolute_max: DEFAULT_ABSOLUTE_MAX,
        }
    }

    /// Overrides the idle timeout new sessions get (and that renewals
    /// extend by). Called once at boot with `gateway.session_ttl_days`.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Overrides the absolute lifetime cap. Called once at boot with
    /// `gateway.session_absolute_max_days`.
    pub fn with_absolute_max(mut self, absolute_max: Duration) -> Self {
        self.absolute_max = absolute_max;
        self
    }

    /// The configured idle timeout — what `create` stamps and what
    /// `lookup` renews to.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn ttl_span(&self) -> SignedDuration {
        SignedDuration::from_secs(secs(self.ttl))
    }

    fn absolute_max_span(&self) -> SignedDuration {
        SignedDuration::from_secs(secs(self.absolute_max))
    }

    /// Mints a fresh session for `user_id` with the configured idle
    /// timeout, persists it, and returns it. Caller serialises the id
    /// into a cookie via [`Self::cookie`].
    pub async fn create(&self, user_id: &str) -> Result<Session, SessionError> {
        self.create_with_ttl(user_id, self.ttl).await
    }

    /// Like [`Self::create`] but with an explicit initial lifetime. Note
    /// that this only sets the *first* expiry: once the session is used,
    /// renewal extends it by the store's configured TTL, not by `ttl`.
    pub async fn create_with_ttl(
        &self,
        user_id: &str,
        ttl: Duration,
    ) -> Result<Session, SessionError> {
        self.insert(user_id, None, ttl).await
    }

    /// Mints an impersonation session: `user_id` is the target the admin
    /// will act as, `impersonator_id` is the admin themselves. The whole
    /// app reads `session.user_id`, so the resulting cookie makes every
    /// handler behave as the target — `impersonator_id` is what the
    /// banner and `/impersonate/stop` use to find the way back.
    pub async fn create_impersonation(
        &self,
        target_user_id: &str,
        impersonator_id: &str,
    ) -> Result<Session, SessionError> {
        self.insert(target_user_id, Some(impersonator_id), IMPERSONATION_TTL)
            .await
    }

    /// Shared INSERT for both ordinary and impersonation sessions.
    async fn insert(
        &self,
        user_id: &str,
        impersonator_id: Option<&str>,
        ttl: Duration,
    ) -> Result<Session, SessionError> {
        let id = random_session_id();
        let now = Timestamp::now();
        let expires =
            now + SignedDuration::try_from(ttl).unwrap_or(SignedDuration::from_hours(24 * 7));
        sqlx::query(
            "INSERT INTO sessions (id, user_id, created_at, expires_at, impersonator_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(now.to_string())
        .bind(expires.to_string())
        .bind(impersonator_id)
        .execute(&self.db)
        .await?;
        Ok(Session {
            id,
            user_id: user_id.to_string(),
            expires_at: expires,
            timezone: None,
            impersonator_id: impersonator_id.map(str::to_string),
        })
    }

    /// Looks up a session by id, returning it iff present **and** not
    /// expired. Expired rows are left for a future GC pass; we just hide
    /// them at read time so a clock-skew gap doesn't grant access.
    ///
    /// Doubles as the renewal point: a session that's past its half-life
    /// gets `expires_at` pushed to `now + ttl` (see [`Self::renew`]), so
    /// the TTL behaves as an idle timeout instead of a hard countdown
    /// from login. Only writing past the half-life keeps this to at most
    /// one UPDATE per session per `ttl/2` rather than one per request.
    pub async fn lookup(&self, id: &str) -> Result<Option<Session>, SessionError> {
        // (user_id, created_at, expires_at, timezone, impersonator_id)
        type Row = (String, String, String, Option<String>, Option<String>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT user_id, created_at, expires_at, timezone, impersonator_id \
                 FROM sessions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id, created_at, expires_at, timezone, impersonator_id)) = row else {
            return Ok(None);
        };
        let expires_at: Timestamp = expires_at.parse().map_err(|_| SessionError::Malformed)?;
        let created_at: Timestamp = created_at.parse().map_err(|_| SessionError::Malformed)?;
        let now = Timestamp::now();
        if expires_at < now {
            return Ok(None);
        }
        // Absolute timeout, enforced here rather than trusted from
        // `expires_at`: it also covers rows written before the cap existed
        // and a config where `ttl` exceeds `absolute_max`.
        if created_at + self.absolute_max_span() < now {
            return Ok(None);
        }
        // Sliding expiration. A renewal failure must not fail the request
        // — the session is valid either way, it just expires earlier.
        let expires_at = if impersonator_id.is_some() {
            // Impersonation sessions never slide: they're a debugging aid
            // with someone else's identity, so they end at a fixed
            // deadline (see IMPERSONATION_TTL) rather than living as long
            // as an admin keeps clicking.
            expires_at
        } else {
            match self.renew(id, created_at, expires_at, now).await {
                Ok(fresh) => fresh,
                Err(err) => {
                    tracing::warn!(error = %err, "session renewal");
                    expires_at
                }
            }
        };
        Ok(Some(Session {
            id: id.to_string(),
            user_id,
            expires_at,
            timezone,
            impersonator_id,
        }))
    }

    /// Pushes `expires_at` to `now + ttl` when less than half the TTL is
    /// left, returning the (possibly unchanged) expiry. Half-life is the
    /// throttle: renewing on every request would mean a write per page
    /// view, renewing never would mean a hard logout `ttl` after login.
    ///
    /// Never past `created_at + absolute_max` — the absolute timeout wins
    /// over the sliding one, so no session can be kept alive forever just
    /// by using it.
    async fn renew(
        &self,
        id: &str,
        created_at: Timestamp,
        expires_at: Timestamp,
        now: Timestamp,
    ) -> Result<Timestamp, SessionError> {
        // Half-life as a point in time — jiff's `Timestamp - Timestamp`
        // is a calendar `Span`, so comparing instants is both simpler and
        // free of unit conversions.
        if now < expires_at - SignedDuration::from_secs(secs(self.ttl) / 2) {
            return Ok(expires_at);
        }
        let renewed = (now + self.ttl_span()).min(created_at + self.absolute_max_span());
        if renewed <= expires_at {
            // Already at the absolute ceiling; nothing to write.
            return Ok(expires_at);
        }
        sqlx::query("UPDATE sessions SET expires_at = ? WHERE id = ?")
            .bind(renewed.to_string())
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(renewed)
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

    /// Full `Set-Cookie` value for `id` — signed payload plus the
    /// attributes every call site must agree on. `secure` comes from
    /// [`secure_cookies`].
    ///
    /// `Max-Age` is what makes a login survive a browser quit or a
    /// reboot; see [`COOKIE_MAX_AGE`] for why it's much longer than the
    /// session TTL.
    pub fn cookie(&self, id: &str, secure: bool) -> String {
        format!(
            "{COOKIE_NAME}={signed}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
            signed = self.sign(id),
            max_age = COOKIE_MAX_AGE.as_secs(),
            secure = if secure { "; Secure" } else { "" },
        )
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

/// `Set-Cookie` value that deletes the session cookie. Attributes other
/// than `Max-Age` must match [`SessionStore::cookie`] or the browser
/// treats it as a different cookie and keeps the old one.
pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}",
        secure = if secure { "; Secure" } else { "" },
    )
}

/// Whether to mark cookies `Secure`, derived from the gateway's own
/// public URL (`gateway.public_url`) rather than the request: behind a
/// TLS-terminating proxy the inbound request is plain HTTP, so the
/// scheme on the wire would say `false` for every production deployment.
/// A plain-HTTP deployment (local dev) must not get `Secure` or the
/// browser drops the cookie outright.
pub fn secure_cookies(public_url: &str) -> bool {
    public_url.trim_start().starts_with("https://")
}

/// Whole seconds of a `Duration` as `i64` — the unit jiff's
/// `SignedDuration` takes. Saturates rather than wrapping; a TTL past
/// year 292-billion is not a case worth a `Result`.
fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
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

/// True if `p` is a safe *same-origin* redirect target. `starts_with('/')`
/// is not enough: `//evil.com` and `/\evil.com` are protocol-relative URLs
/// (browsers normalise `\`→`/`), so a naive check would let a post-login
/// redirect bounce the user to an attacker's host. Leading ASCII
/// whitespace is trimmed first, since browsers strip it before resolving.
///
/// Lives here rather than next to the `/auth/*` handlers because both the
/// OIDC callback (in the `gateway` crate) and the page chrome's login links
/// (in `gateway-web`) must agree on it. `session_core::chrome` carries a
/// deliberate duplicate for the driver-agnostic renderers.
pub fn is_safe_return_to(p: &str) -> bool {
    let p = p.trim_start_matches(|c: char| c.is_ascii_whitespace());
    p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\")
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
                speech_voice: None,
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
    async fn impersonation_session_carries_impersonator_id() {
        let store = store().await;
        let now = Timestamp::now();
        for id in ["root", "victim"] {
            crate::server::db::users::upsert(
                &store.db,
                &crate::server::db::users::User {
                    id: id.into(),
                    email: format!("{id}@x"),
                    name: None,
                    roles: vec![],
                    created_at: now,
                    updated_at: now,
                    timezone: None,
                    speech_voice: None,
                },
            )
            .await
            .unwrap();
        }
        // An ordinary session has no impersonator.
        let plain = store.create("root").await.unwrap();
        assert_eq!(plain.impersonator_id, None);

        // An impersonation session acts as the target, remembers the admin.
        let imp = store.create_impersonation("victim", "root").await.unwrap();
        assert_eq!(imp.user_id, "victim");
        assert_eq!(imp.impersonator_id.as_deref(), Some("root"));

        // And it round-trips through lookup.
        let fetched = store.lookup(&imp.id).await.unwrap().unwrap();
        assert_eq!(fetched.user_id, "victim");
        assert_eq!(fetched.impersonator_id.as_deref(), Some("root"));
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
                speech_voice: None,
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

    /// Seed a user so the sessions FK is satisfiable.
    async fn seed_user(store: &SessionStore, id: &str) {
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            &store.db,
            &crate::server::db::users::User {
                id: id.into(),
                email: format!("{id}@x"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
                speech_voice: None,
            },
        )
        .await
        .unwrap();
    }

    async fn set_row_times(store: &SessionStore, id: &str, created: Timestamp, expires: Timestamp) {
        sqlx::query("UPDATE sessions SET created_at = ?, expires_at = ? WHERE id = ?")
            .bind(created.to_string())
            .bind(expires.to_string())
            .bind(id)
            .execute(&store.db)
            .await
            .unwrap();
    }

    /// The whole point of the sliding window: someone who comes back after
    /// a fortnight keeps their login instead of being bounced to the IdP.
    #[tokio::test]
    async fn lookup_renews_a_session_past_its_half_life() {
        let store = store().await;
        seed_user(&store, "alice").await;
        let session = store.create("alice").await.unwrap();

        // Pretend the session was created 20 days ago: 10 days left of a
        // 30-day TTL, i.e. past the half-life.
        let now = Timestamp::now();
        let created = now - SignedDuration::from_hours(24 * 20);
        let stale_expiry = created + SignedDuration::from_hours(24 * 30);
        set_row_times(&store, &session.id, created, stale_expiry).await;

        let fetched = store.lookup(&session.id).await.unwrap().unwrap();
        assert!(
            fetched.expires_at > stale_expiry,
            "expiry should have been pushed forward: {} !> {stale_expiry}",
            fetched.expires_at,
        );
        // And it's persisted, not just returned.
        let stored: String = sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id = ?")
            .bind(&session.id)
            .fetch_one(&store.db)
            .await
            .unwrap();
        assert_eq!(stored.parse::<Timestamp>().unwrap(), fetched.expires_at);
    }

    /// Renewal is throttled to the second half of the window so an active
    /// session doesn't cost a write per request.
    #[tokio::test]
    async fn lookup_leaves_a_fresh_session_untouched() {
        let store = store().await;
        seed_user(&store, "bob").await;
        let session = store.create("bob").await.unwrap();

        let fetched = store.lookup(&session.id).await.unwrap().unwrap();
        assert_eq!(
            fetched.expires_at, session.expires_at,
            "a just-created session must not be rewritten",
        );
    }

    /// The absolute cap outranks the sliding renewal — a session can't be
    /// kept alive forever just by being used.
    #[tokio::test]
    async fn absolute_max_caps_renewal_and_then_expires_the_session() {
        let store = store()
            .await
            .with_absolute_max(Duration::from_secs(60 * 60 * 24 * 90));
        seed_user(&store, "carol").await;
        let session = store.create("carol").await.unwrap();

        // 80 days in, still inside the 90-day cap but past the half-life:
        // renewal happens, clamped to created_at + 90 days.
        let now = Timestamp::now();
        let created = now - SignedDuration::from_hours(24 * 80);
        set_row_times(
            &store,
            &session.id,
            created,
            created + SignedDuration::from_hours(24 * 85),
        )
        .await;
        let fetched = store.lookup(&session.id).await.unwrap().unwrap();
        let cap = created + SignedDuration::from_hours(24 * 90);
        assert_eq!(fetched.expires_at, cap, "renewal must clamp to the cap");

        // 100 days in: the row still says "not expired", but the absolute
        // cap has passed, so the session is gone regardless.
        set_row_times(
            &store,
            &session.id,
            now - SignedDuration::from_hours(24 * 100),
            now + SignedDuration::from_hours(24 * 30),
        )
        .await;
        assert!(store.lookup(&session.id).await.unwrap().is_none());
    }

    /// Acting as someone else is deliberately short-lived: a fixed
    /// deadline, and no sliding renewal to push it out.
    #[tokio::test]
    async fn impersonation_sessions_are_short_lived_and_never_renewed() {
        let store = store().await;
        seed_user(&store, "root").await;
        seed_user(&store, "victim").await;

        let imp = store.create_impersonation("victim", "root").await.unwrap();
        let now = Timestamp::now();
        assert!(
            imp.expires_at < now + SignedDuration::from_hours(9),
            "impersonation should expire in hours, not days: {}",
            imp.expires_at,
        );

        // Well past the half-life of an 8-hour window — an ordinary
        // session would be renewed here.
        let created = now - SignedDuration::from_hours(7);
        let expiry = created + SignedDuration::from_hours(8);
        set_row_times(&store, &imp.id, created, expiry).await;
        let fetched = store.lookup(&imp.id).await.unwrap().unwrap();
        assert_eq!(fetched.expires_at, expiry, "must not slide");
    }

    #[tokio::test]
    async fn cookie_carries_the_attributes_that_survive_a_restart() {
        let store = store().await;
        let cookie = store.cookie("sess-1", true);
        // Max-Age is what makes the cookie outlive a browser/laptop restart.
        assert!(
            cookie.contains(&format!("Max-Age={}", COOKIE_MAX_AGE.as_secs())),
            "{cookie}"
        );
        assert!(cookie.starts_with("id=sess-1."), "{cookie}");
        for attr in ["Path=/", "HttpOnly", "SameSite=Lax", "Secure"] {
            assert!(cookie.contains(attr), "missing {attr} in {cookie}");
        }
        // Plain-HTTP deployments must not get `Secure` — the browser would
        // drop the cookie and nobody could log in at all.
        assert!(!store.cookie("sess-1", false).contains("Secure"));
    }

    /// A cleared cookie only replaces the live one if every attribute
    /// besides Max-Age matches.
    #[test]
    fn clear_cookie_mirrors_the_set_cookie_attributes() {
        let cleared = clear_cookie(true);
        for attr in [
            "id=",
            "Path=/",
            "HttpOnly",
            "SameSite=Lax",
            "Max-Age=0",
            "Secure",
        ] {
            assert!(cleared.contains(attr), "missing {attr} in {cleared}");
        }
        assert!(!clear_cookie(false).contains("Secure"));
    }

    #[test]
    fn secure_cookies_follows_the_public_url_scheme() {
        assert!(secure_cookies("https://llm.example.com"));
        assert!(!secure_cookies("http://localhost:8080"));
        assert!(!secure_cookies(""));
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

#[cfg(test)]
mod return_to_tests {
    use super::is_safe_return_to;

    #[test]
    fn accepts_local_paths() {
        assert!(is_safe_return_to("/"));
        assert!(is_safe_return_to("/chat/abc"));
        assert!(is_safe_return_to("/chat/shared-1?x=1"));
    }

    #[test]
    fn rejects_protocol_relative_and_absolute_urls() {
        // The whole point of the guard: these all pass `starts_with('/')`
        // (or look local) yet resolve off-origin in a browser.
        assert!(!is_safe_return_to("//evil.com"));
        assert!(!is_safe_return_to("/\\evil.com"));
        assert!(!is_safe_return_to("https://evil.com"));
        assert!(!is_safe_return_to("http://evil.com"));
        assert!(!is_safe_return_to("evil.com"));
        assert!(!is_safe_return_to(""));
    }

    #[test]
    fn rejects_whitespace_smuggled_protocol_relative() {
        // Browsers strip leading whitespace before resolving, so we must
        // trim before checking, else `\t//evil.com` would slip through.
        assert!(!is_safe_return_to("  //evil.com"));
        assert!(!is_safe_return_to("\t//evil.com"));
        assert!(!is_safe_return_to("\n//evil.com"));
    }
}
