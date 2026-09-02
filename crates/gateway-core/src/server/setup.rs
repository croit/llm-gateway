// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Deployment setup state: is this gateway configured yet, what did the
//! operator configure, and how do they get back in when they lock themselves
//! out.
//!
//! Everything here lives in [`app_settings`] rows, because the point of the
//! setup wizard is that a fresh container needs no configuration file: it boots
//! with one environment variable (`GATEWAY_SESSION_KEY`), serves a wizard, and
//! writes what the operator answers straight into the database.
//!
//! # Three states
//!
//! * **First run** — [`COMPLETED_KEY`] absent. There is nothing to protect yet,
//!   so `/setup` is open and every other page redirects to it.
//! * **Operating** — [`COMPLETED_KEY`] set. `/setup` is gone.
//! * **Recovery** — [`COMPLETED_KEY`] set *and* [`RECOVERY_UNTIL_KEY`] in the
//!   future. The gateway keeps serving normally — chats, `/v1`, existing
//!   sessions all continue — and only `/setup` becomes reachable again, gated
//!   by the one-time token that `restore-setup` printed. That separation is
//!   deliberate: an admin who cannot log in must not be able to take a working
//!   production gateway offline for everyone else just by asking for help.
//!
//! # Why the OIDC secret is sealed but the issuer is not
//!
//! Same split as [`crate::server::db::upstreams_config`] and
//! `search_settings`: identifiers are plain rows an operator can read in a
//! `sqlite3` shell while debugging, secrets go through [`Crypto`]. A stored
//! secret is write-only from then on — the wizard shows "set", never the value.

use jiff::{SignedDuration, Timestamp};

use crate::server::auth::oidc::OidcParams;
use crate::server::config::OidcConfig;
use crate::server::crypto::{self, Crypto};
use crate::server::db::{DbError, Pool, app_settings};
use crate::server::oidc_settings;

/// Set to `"1"` once the wizard has finished. Its *absence* is what puts the
/// gateway in first-run mode, so it must only ever be written after a login
/// has actually been proven to work end to end.
const COMPLETED_KEY: &str = "setup.completed";
/// RFC 3339 timestamp until which `/setup` is reachable again on an already
/// configured gateway. Written by `restore-setup`.
///
/// `pub` only so `tests/it/setup_wizard.rs` can wind the deadline back to
/// simulate expiry; production code goes through [`access`].
pub const RECOVERY_UNTIL_KEY: &str = "setup.recovery_until";
/// SHA-256 (hex) of the one-time recovery token. The token itself is only ever
/// printed to the operator's terminal — the database stores the hash, so a
/// database read alone does not grant the ability to reopen setup.
const RECOVERY_TOKEN_KEY: &str = "setup.recovery_token";

const PUBLIC_URL_KEY: &str = "gateway.public_url";

/// Marks that the one-time import of `[oidc]` / `[gateway].public_url` from a
/// legacy config file has run. Same shape as the `topology.seeded` and
/// `rbac.seeded` markers: gated on the marker rather than on the rows being
/// empty, so an operator who deliberately clears a setting does not get the
/// config file's value resurrected on the next restart.
const IMPORT_MARKER_KEY: &str = "setup.config_imported";

/// How long `restore-setup` keeps `/setup` reachable. Long enough to walk
/// through the wizard including a trip to the IdP's admin console, short
/// enough that an operator who gets distracted does not leave a configuration
/// endpoint open on a production gateway.
pub const RECOVERY_WINDOW: SignedDuration = SignedDuration::from_mins(30);

/// Whether the wizard has ever completed.
pub async fn is_completed(pool: &Pool) -> Result<bool, DbError> {
    Ok(app_settings::get(pool, COMPLETED_KEY)
        .await?
        .is_some_and(|v| v == "1"))
}

/// Record that setup finished. Only called after a real login round trip.
pub async fn mark_completed(pool: &Pool) -> Result<(), DbError> {
    app_settings::set(pool, COMPLETED_KEY, "1").await?;
    // Finishing setup always closes any recovery window, whether or not this
    // run started from one, and clears the in-flight draft + claims.
    clear_recovery(pool).await?;
    clear_wizard_state(pool).await
}

/// How `/setup` may be reached right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAccess {
    /// Nothing is configured yet: open, no token.
    FirstRun,
    /// Configured, and `restore-setup` opened a window: token required.
    Recovery,
    /// Configured, no window open: `/setup` does not exist.
    Closed,
}

/// Resolve the current access mode. Reads at most two rows and is only called
/// on `/setup*` requests, so the operating path pays nothing for it.
pub async fn access(pool: &Pool) -> Result<SetupAccess, DbError> {
    if !is_completed(pool).await? {
        return Ok(SetupAccess::FirstRun);
    }
    match recovery_deadline(pool).await? {
        Some(deadline) if deadline > Timestamp::now() => Ok(SetupAccess::Recovery),
        _ => Ok(SetupAccess::Closed),
    }
}

async fn recovery_deadline(pool: &Pool) -> Result<Option<Timestamp>, DbError> {
    Ok(app_settings::get(pool, RECOVERY_UNTIL_KEY)
        .await?
        .and_then(|v| v.parse::<Timestamp>().ok()))
}

/// Open a recovery window and store the hash of `token`. Returns when it
/// expires, so the caller can print it.
pub async fn open_recovery(pool: &Pool, token: &str) -> Result<Timestamp, DbError> {
    let until = Timestamp::now() + RECOVERY_WINDOW;
    app_settings::set(pool, RECOVERY_UNTIL_KEY, &until.to_string()).await?;
    app_settings::set(
        pool,
        RECOVERY_TOKEN_KEY,
        &crypto::sha256_hex(token.as_bytes()),
    )
    .await?;
    Ok(until)
}

async fn clear_recovery(pool: &Pool) -> Result<(), DbError> {
    app_settings::delete(pool, RECOVERY_UNTIL_KEY).await?;
    app_settings::delete(pool, RECOVERY_TOKEN_KEY).await
}

/// Constant-time check of a presented recovery token against the stored hash.
///
/// Takes the already-resolved [`SetupAccess`] rather than re-deriving it: the
/// only caller has just computed it, and asking twice meant five `app_settings`
/// reads per `/setup` request where three do. Anything but
/// [`SetupAccess::Recovery`] is `false`, so an expired window cannot be walked
/// into with an old token.
pub async fn recovery_token_matches(
    pool: &Pool,
    access: SetupAccess,
    presented: &str,
) -> Result<bool, DbError> {
    if access != SetupAccess::Recovery {
        return Ok(false);
    }
    let Some(expected) = app_settings::get(pool, RECOVERY_TOKEN_KEY).await? else {
        return Ok(false);
    };
    Ok(constant_time_eq(
        expected.as_bytes(),
        crypto::sha256_hex(presented.as_bytes()).as_bytes(),
    ))
}

/// The gateway's own base URL, as the operator confirmed it in the wizard.
pub async fn public_url(pool: &Pool) -> Result<Option<String>, DbError> {
    Ok(app_settings::get(pool, PUBLIC_URL_KEY)
        .await?
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty()))
}

pub async fn set_public_url(pool: &Pool, url: &str) -> Result<(), DbError> {
    app_settings::set(pool, PUBLIC_URL_KEY, url.trim().trim_end_matches('/')).await
}

/// Sealed JSON holding the settings the operator typed into the wizard but
/// has not yet proven. See [`Draft`].
const DRAFT_KEY: &str = "setup.draft";
/// Sealed JSON holding the claims of the login that proved the draft works.
/// See [`Proof`].
const PROOF_KEY: &str = "setup.proof";

/// Wizard input awaiting proof.
///
/// It is deliberately *not* written into the live settings keys: a gateway
/// must never end up half-configured with a provider nobody has managed to
/// log in through. The draft is promoted to the live settings only after a
/// real authorization-code round trip succeeds, and is discarded either way.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub public_url: String,
    /// Flattened, so the stored JSON is one flat object and this struct cannot
    /// drift from [`OidcParams`] when a provider setting is added.
    #[serde(flatten)]
    pub params: OidcParams,
}

/// The result of the wizard's test login: who signed in, and every claim the
/// provider asserted about them. The operator picks the admin group out of
/// [`Self::claims`], which is the whole reason the wizard tests with a real
/// login instead of just probing discovery — you cannot guess a group claim's
/// name or its values, you have to look at them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Proof {
    pub subject: String,
    pub email: String,
    pub name: Option<String>,
    pub claims: serde_json::Value,
}

pub async fn save_draft(pool: &Pool, crypto: &Crypto, draft: &Draft) -> Result<(), DbError> {
    save_sealed_json(pool, crypto, DRAFT_KEY, draft).await
}

pub async fn load_draft(pool: &Pool, crypto: &Crypto) -> Result<Option<Draft>, DbError> {
    load_sealed_json(pool, crypto, DRAFT_KEY).await
}

pub async fn save_proof(pool: &Pool, crypto: &Crypto, proof: &Proof) -> Result<(), DbError> {
    save_sealed_json(pool, crypto, PROOF_KEY, proof).await
}

pub async fn load_proof(pool: &Pool, crypto: &Crypto) -> Result<Option<Proof>, DbError> {
    load_sealed_json(pool, crypto, PROOF_KEY).await
}

/// Drop every trace of an in-flight wizard run. Called when setup finishes and
/// when it is abandoned, so a half-finished attempt never leaks a client
/// secret or someone's claims into a long-lived row.
async fn clear_wizard_state(pool: &Pool) -> Result<(), DbError> {
    app_settings::delete(pool, DRAFT_KEY).await?;
    clear_proof(pool).await
}

/// Drop just the proven login, so the wizard's "back to provider settings"
/// button returns to screen 1 without discarding what was typed.
pub async fn clear_proof(pool: &Pool) -> Result<(), DbError> {
    app_settings::delete(pool, PROOF_KEY).await
}

async fn save_sealed_json<T: serde::Serialize>(
    pool: &Pool,
    crypto: &Crypto,
    key: &str,
    value: &T,
) -> Result<(), DbError> {
    let json = serde_json::to_string(value).map_err(|e| DbError::Decode {
        column: "setup_json",
        source: e.into(),
    })?;
    app_settings::set(pool, key, &crypto.seal_to_string(&json)?).await
}

async fn load_sealed_json<T: serde::de::DeserializeOwned>(
    pool: &Pool,
    crypto: &Crypto,
    key: &str,
) -> Result<Option<T>, DbError> {
    let Some(stored) = app_settings::get(pool, key).await? else {
        return Ok(None);
    };
    // A value we cannot open is a value we cannot use. Treat it as absent —
    // the wizard simply starts its step again — rather than failing the page.
    Ok(crypto
        .open_from_string(&stored)
        .and_then(|json| serde_json::from_str(&json).ok()))
}

/// Has this database ever been used? True as soon as one person has signed in
/// or one gateway group exists.
///
/// This is the safety net under [`import_config_once`], and it guards
/// something serious. First-run mode leaves `/setup` **open and
/// unauthenticated** — fine on an empty box with nothing to steal, a takeover
/// vector on a live one. An existing deployment must therefore never be able to
/// fall into it, and "has an importable `[oidc]` block" is too narrow a test:
/// a token-only `/v1` deployment may legitimately have no `[oidc]` at all, and
/// an operator upgrading with their `EnvironmentFile` temporarily missing has
/// one that cannot be resolved. Both of those are running gateways with real
/// users, real chats and real sealed backend keys.
///
/// So the question we actually ask is "is anybody here?", not "was the config
/// importable".
async fn has_been_used(pool: &Pool) -> Result<bool, DbError> {
    let used: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users) OR EXISTS(SELECT 1 FROM gateway_groups)",
    )
    .fetch_one(pool)
    .await?;
    Ok(used != 0)
}

/// Copy `[oidc]` and `[gateway].public_url` out of a legacy config file into
/// empty settings, exactly once.
///
/// This is what upgrades an existing config-file deployment in place: on the
/// first boot after this release its provider moves into the database and the
/// gateway marks setup complete, so nobody is redirected to a wizard and
/// `/setup` never opens. From then on the file's `[oidc]` block is ignored.
/// A genuinely fresh install imports nothing and lands in first-run mode.
///
/// Returns whether a provider was imported, for logging.
pub async fn import_config_once(
    pool: &Pool,
    crypto: &Crypto,
    oidc: Option<&OidcConfig>,
    config_public_url: &str,
) -> Result<bool, DbError> {
    if app_settings::get(pool, IMPORT_MARKER_KEY).await?.is_some() {
        return Ok(false);
    }
    let mut imported = false;
    // Whether the decision is final. A config file that *has* an `[oidc]` block
    // we could not resolve is not final — the client-secret env var may simply
    // be missing on this one boot (an `EnvironmentFile` not yet in place), and
    // burning the marker would ignore that block forever afterwards.
    let mut settled = true;

    if let Some(cfg) = oidc
        && oidc_settings::params(pool, crypto).await?.is_none()
    {
        match cfg.to_params() {
            Some(params) => {
                oidc_settings::set_params(pool, crypto, &params).await?;
                imported = true;
            }
            None => {
                settled = false;
                tracing::warn!(
                    env = %cfg.client_secret_env,
                    "config file has an [oidc] block but its client-secret env var is unset, \
                     so there is nothing to import; set it and restart, or configure the \
                     provider at /setup"
                );
            }
        }
    }

    if imported && public_url(pool).await?.is_none() {
        set_public_url(pool, config_public_url).await?;
    }

    // Never drop a deployment that is already in use into first-run mode: that
    // would redirect all its users to a wizard AND open `/setup` to anyone who
    // can reach the port. See [`has_been_used`].
    if imported || has_been_used(pool).await? {
        app_settings::set(pool, COMPLETED_KEY, "1").await?;
    }

    if settled {
        app_settings::set(pool, IMPORT_MARKER_KEY, "1").await?;
    }
    Ok(imported)
}

/// Compares without an early return on the first differing byte, so the
/// comparison time does not leak how much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

    fn crypto() -> Crypto {
        Crypto::from_key([7u8; 32])
    }

    async fn seed_a_user(pool: &Pool) {
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            pool,
            &crate::server::db::users::User {
                id: "someone".into(),
                email: "someone@example.com".into(),
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

    #[tokio::test]
    async fn an_empty_install_stays_in_first_run_mode() {
        let pool = fresh().await;
        assert!(
            !import_config_once(&pool, &crypto(), None, "http://localhost:8080")
                .await
                .unwrap()
        );
        assert_eq!(access(&pool).await.unwrap(), SetupAccess::FirstRun);
    }

    #[tokio::test]
    async fn a_deployment_already_in_use_never_falls_into_first_run_mode() {
        // The regression: first-run mode leaves `/setup` OPEN. A running
        // gateway with no `[oidc]` block — a token-only `/v1` deployment is a
        // supported shape — used to land there on the first boot after this
        // release, handing anyone who could reach the port an unauthenticated
        // way to make themselves admin.
        let pool = fresh().await;
        seed_a_user(&pool).await;

        assert!(
            !import_config_once(&pool, &crypto(), None, "http://localhost:8080")
                .await
                .unwrap(),
            "nothing to import"
        );
        assert_eq!(
            access(&pool).await.unwrap(),
            SetupAccess::Closed,
            "a gateway with users must not open its setup wizard"
        );
    }

    #[tokio::test]
    async fn a_config_provider_is_imported_and_marks_the_gateway_configured() {
        let pool = fresh().await;
        // SAFETY: this test's own env var, read synchronously below.
        unsafe { std::env::set_var("GATEWAY_SETUP_TEST_SECRET", "shh") };
        let cfg = OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "gw".into(),
            client_secret_env: "GATEWAY_SETUP_TEST_SECRET".into(),
            scopes: vec!["email".into()],
            roles_claim: Some("groups".into()),
        };

        assert!(
            import_config_once(&pool, &crypto(), Some(&cfg), "https://gw.example.com")
                .await
                .unwrap()
        );
        assert_eq!(access(&pool).await.unwrap(), SetupAccess::Closed);
        let params = oidc_settings::params(&pool, &crypto())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(params.client_secret, "shh");
        assert_eq!(
            public_url(&pool).await.unwrap().as_deref(),
            Some("https://gw.example.com")
        );

        // Second boot imports nothing more, even if the config changes.
        assert!(
            !import_config_once(&pool, &crypto(), Some(&cfg), "https://gw.example.com")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_unresolvable_config_provider_is_retried_on_the_next_boot() {
        // An `EnvironmentFile` that was not in place yet must not permanently
        // burn the import marker and leave the `[oidc]` block ignored forever.
        let pool = fresh().await;
        let cfg = OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "gw".into(),
            client_secret_env: "GATEWAY_SETUP_DEFINITELY_UNSET".into(),
            scopes: vec![],
            roles_claim: None,
        };

        assert!(
            !import_config_once(&pool, &crypto(), Some(&cfg), "https://gw.example.com")
                .await
                .unwrap()
        );
        assert!(
            app_settings::get(&pool, IMPORT_MARKER_KEY)
                .await
                .unwrap()
                .is_none(),
            "the decision was not final, so the marker must not be set"
        );

        // Env var appears; the next boot imports it.
        // SAFETY: this test's own env var, read synchronously below.
        unsafe { std::env::set_var("GATEWAY_SETUP_DEFINITELY_UNSET", "late") };
        assert!(
            import_config_once(&pool, &crypto(), Some(&cfg), "https://gw.example.com")
                .await
                .unwrap()
        );
        unsafe { std::env::remove_var("GATEWAY_SETUP_DEFINITELY_UNSET") };
    }

    #[tokio::test]
    async fn recovery_opens_a_window_and_the_token_is_single_purpose() {
        let pool = fresh().await;
        mark_completed(&pool).await.unwrap();
        assert_eq!(access(&pool).await.unwrap(), SetupAccess::Closed);
        assert!(
            !recovery_token_matches(&pool, SetupAccess::Closed, "anything")
                .await
                .unwrap(),
            "no window open"
        );

        open_recovery(&pool, "the-token").await.unwrap();
        assert_eq!(access(&pool).await.unwrap(), SetupAccess::Recovery);
        assert!(
            recovery_token_matches(&pool, SetupAccess::Recovery, "the-token")
                .await
                .unwrap()
        );
        assert!(
            !recovery_token_matches(&pool, SetupAccess::Recovery, "the-tokeN")
                .await
                .unwrap()
        );
        // An expired window is `Closed`, and no token opens it.
        assert!(
            !recovery_token_matches(&pool, SetupAccess::Closed, "the-token")
                .await
                .unwrap()
        );

        // The stored value is a hash, so reading the database does not hand
        // anyone the ability to reopen setup.
        let stored = app_settings::get(&pool, RECOVERY_TOKEN_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(stored, "the-token");
        assert_eq!(stored, crypto::sha256_hex(b"the-token"));
    }

    #[tokio::test]
    async fn finishing_setup_closes_any_recovery_window_and_clears_the_draft() {
        let pool = fresh().await;
        let c = crypto();
        save_draft(
            &pool,
            &c,
            &Draft {
                public_url: "https://gw.example.com".into(),
                params: OidcParams {
                    issuer: "https://id.example.com".into(),
                    client_id: "gw".into(),
                    client_secret: "shh".into(),
                    scopes: vec![],
                    roles_claim: None,
                },
            },
        )
        .await
        .unwrap();
        open_recovery(&pool, "t").await.unwrap();

        mark_completed(&pool).await.unwrap();

        assert_eq!(access(&pool).await.unwrap(), SetupAccess::Closed);
        assert!(
            load_draft(&pool, &c).await.unwrap().is_none(),
            "the draft holds a client secret; it must not outlive the run"
        );
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
