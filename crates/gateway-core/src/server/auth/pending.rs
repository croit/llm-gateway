// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The in-flight half of an OIDC authorization request: the `pending_logins`
//! row and the browser-binding cookie that goes with it.
//!
//! This lives in `gateway-core` rather than next to the `/auth/*` handlers
//! because two callers start the same dance. The sign-in path does, and so
//! does the setup wizard, which proves a provider works by running a genuine
//! authorization-code round trip through the very same
//! `{public_url}/auth/callback` redirect URI. Sharing one implementation is
//! what makes "the wizard tested it" mean "production will work" — a
//! second, parallel implementation would eventually drift and the test would
//! start proving something else.

use jiff::{SignedDuration, Timestamp};
use rama::http::{Body, Response, StatusCode, header};

use crate::server::auth::oidc::AuthorizationStart;
use crate::server::db::{DbError, Pool};

/// TTL for the in-flight row. Generous because some IdPs (Authentik,
/// Keycloak's account-linking flows) bounce the user through several screens
/// before redirecting back.
const PENDING_LOGIN_TTL: SignedDuration = SignedDuration::from_mins(15);

/// Cookie that binds an in-flight OIDC login to the browser that started it.
/// Set when the authorization request begins, and required to match the
/// `state` parameter at the callback. Without it, the `state` row alone does
/// not prove the callback arrived in the same browser, opening login CSRF /
/// session fixation. Scoped to `Path=/auth` so it only rides the login routes.
pub const BINDING_COOKIE: &str = "gw_oidc";

/// What an in-flight authorization request is *for*.
///
/// An enum rather than two `&str` constants because `/auth/callback`
/// dispatches on it, and a stringly-typed dispatch there fails in the worst
/// possible direction: anything that is not recognised falls through to the
/// branch that upserts a user and mints a session. A future third purpose
/// (device flow, connector linking, re-consent) that somebody forgets to wire
/// would silently sign people in. Matching exhaustively makes the compiler
/// raise it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// An ordinary sign-in: upsert the user, mint a session, land them on
    /// their page.
    Login,
    /// The setup wizard's test round trip. Same redirect URI, same callback,
    /// different ending: no user is created and no session is minted — the
    /// verified claims go back to the wizard instead.
    Setup,
}

impl Purpose {
    /// The value stored in `pending_logins.purpose`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Setup => "setup",
        }
    }

    /// Parse a stored value. `None` for anything unrecognised — the callback
    /// rejects the flow rather than guessing, which is the only safe default
    /// when the alternative is minting a session.
    pub fn from_stored(s: &str) -> Option<Self> {
        match s {
            "login" => Some(Self::Login),
            "setup" => Some(Self::Setup),
            _ => None,
        }
    }
}

/// Persist the per-flow secrets (PKCE verifier, nonce) under the `state`
/// value that will come back from the IdP.
pub async fn insert(
    pool: &Pool,
    start: &AuthorizationStart,
    return_to: Option<&str>,
    purpose: Purpose,
) -> Result<(), DbError> {
    let now = Timestamp::now();
    sqlx::query(
        "INSERT INTO pending_logins
           (state, pkce_verifier, nonce, return_to, created_at, expires_at, purpose)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&start.csrf)
    .bind(&start.pkce_verifier)
    .bind(&start.nonce)
    .bind(return_to)
    .bind(now.to_string())
    .bind((now + PENDING_LOGIN_TTL).to_string())
    .bind(purpose.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// The `Set-Cookie` value that binds the flow to this browser. `Max-Age`
/// matches [`PENDING_LOGIN_TTL`]; `HttpOnly` keeps it away from JS, and
/// `SameSite=Lax` is sufficient because the value must still equal the `state`
/// parameter at the callback.
pub fn binding_cookie(state: &str) -> String {
    format!(
        "{BINDING_COOKIE}={state}; Path=/auth; HttpOnly; SameSite=Lax; Max-Age={}",
        PENDING_LOGIN_TTL.as_secs()
    )
}

/// The `Set-Cookie` value that clears the binding once the flow is over.
pub fn clear_binding_cookie() -> String {
    format!("{BINDING_COOKIE}=; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// The 303 that sends the browser to the provider, carrying the binding
/// cookie. Both the sign-in path and the wizard's probe go through here, so
/// the probe is bound to its browser exactly as a real login is — and the
/// callback's anti-CSRF check cannot tell them apart, which is the point.
pub fn authorization_redirect(start: &AuthorizationStart) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, start.url.clone())
        .header(header::SET_COOKIE, binding_cookie(&start.csrf))
        .body(Body::empty())
        .expect("status, location and cookie are all valid header values")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_cookie_max_age_tracks_the_row_ttl() {
        // If these drift apart the cookie outlives (or predeceases) the row it
        // guards, and logins start failing for reasons nobody can reproduce.
        let cookie = binding_cookie("abc");
        assert!(cookie.contains("Max-Age=900"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.starts_with("gw_oidc=abc;"), "{cookie}");
    }

    #[test]
    fn purposes_round_trip_and_reject_anything_else() {
        for p in [Purpose::Login, Purpose::Setup] {
            assert_eq!(Purpose::from_stored(p.as_str()), Some(p));
        }
        // The callback must not treat an unknown purpose as a sign-in.
        assert_eq!(Purpose::from_stored("Setup"), None);
        assert_eq!(Purpose::from_stored(""), None);
    }

    #[test]
    fn clearing_the_binding_expires_it_on_the_same_path() {
        // A mismatched Path leaves the old cookie in place, which then fails
        // to match the next login's state.
        let clear = clear_binding_cookie();
        assert!(clear.contains("Path=/auth"), "{clear}");
        assert!(clear.contains("Max-Age=0"), "{clear}");
    }
}
