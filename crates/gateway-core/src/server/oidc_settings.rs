// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The OIDC provider, as a database-backed operator setting.
//!
//! Sibling of [`gateway_features::server::search_settings`] and the same
//! shape: identifiers are plain [`app_settings`] rows an operator can read in
//! a `sqlite3` shell while debugging, and the one secret goes through
//! [`Crypto`]. A stored secret is write-only from then on — the wizard shows
//! "set", never the value.
//!
//! Deliberately its own module rather than part of `server::setup`. The setup
//! wizard *writes* these rows, but the ordinary boot path *reads* them on
//! every start to build the live [`OidcClient`], and a module named "setup"
//! would make that read look like something it isn't.
//!
//! [`OidcClient`]: crate::server::auth::oidc::OidcClient
//! [`Crypto`]: crate::server::crypto::Crypto
//! [`app_settings`]: crate::server::db::app_settings

use crate::server::auth::oidc::OidcParams;
use crate::server::crypto::Crypto;
use crate::server::db::{DbError, Pool, app_settings};

const ISSUER_KEY: &str = "oidc.issuer";
const CLIENT_ID_KEY: &str = "oidc.client_id";
/// Sealed via [`Crypto::seal_to_string`].
const CLIENT_SECRET_KEY: &str = "oidc.client_secret";
const SCOPES_KEY: &str = "oidc.scopes";
const ROLES_CLAIM_KEY: &str = "oidc.roles_claim";

/// Load the stored provider settings. `None` when the gateway has no provider
/// configured (a fresh install, or one whose settings were cleared).
pub async fn params(pool: &Pool, crypto: &Crypto) -> Result<Option<OidcParams>, DbError> {
    let (Some(issuer), Some(client_id), Some(stored)) = (
        app_settings::get(pool, ISSUER_KEY).await?,
        app_settings::get(pool, CLIENT_ID_KEY).await?,
        app_settings::get(pool, CLIENT_SECRET_KEY).await?,
    ) else {
        return Ok(None);
    };
    let Some(client_secret) = crypto.open_from_string(&stored) else {
        // Almost always: the at-rest key changed. Report it as unconfigured —
        // the wizard can then be reopened to re-enter the secret — but say so
        // loudly, because "OIDC silently stopped working" is otherwise a very
        // confusing morning.
        tracing::error!(
            "the stored OIDC client secret could not be decrypted (GATEWAY_ENCRYPTION_KEY \
             or GATEWAY_SESSION_KEY changed?); sign-in is disabled until it is re-entered \
             via `restore-setup`"
        );
        return Ok(None);
    };
    Ok(Some(OidcParams {
        issuer,
        client_id,
        client_secret,
        // A stored row is authoritative, empty included: a legacy config with
        // `scopes = []` means "request only openid", and falling back to the
        // wizard defaults there would start asking for `groups` — which IdPs
        // that do not know that scope reject outright with `invalid_scope`.
        // The default only applies when nothing was ever stored.
        scopes: match app_settings::get(pool, SCOPES_KEY).await? {
            Some(raw) => parse_scopes(&raw),
            None => default_scopes(),
        },
        roles_claim: app_settings::get(pool, ROLES_CLAIM_KEY)
            .await?
            .filter(|v| !v.trim().is_empty()),
    }))
}

/// Persist provider settings, sealing the client secret.
pub async fn set_params(pool: &Pool, crypto: &Crypto, params: &OidcParams) -> Result<(), DbError> {
    // Sealing only fails when the cipher itself is unusable, which is a
    // deployment problem. Propagating it keeps the wizard from reporting
    // success on a secret it never stored.
    let sealed = crypto.seal_to_string(&params.client_secret)?;
    app_settings::set(pool, ISSUER_KEY, params.issuer.trim()).await?;
    app_settings::set(pool, CLIENT_ID_KEY, params.client_id.trim()).await?;
    app_settings::set(pool, CLIENT_SECRET_KEY, &sealed).await?;
    app_settings::set(pool, SCOPES_KEY, &params.scopes.join(" ")).await?;
    match params.roles_claim.as_deref().map(str::trim) {
        Some(c) if !c.is_empty() => app_settings::set(pool, ROLES_CLAIM_KEY, c).await,
        _ => app_settings::delete(pool, ROLES_CLAIM_KEY).await,
    }
}

/// Split a scope list. Accepts spaces or commas so a value typed into the
/// wizard's single-line field works either way. May return empty — an operator
/// who wants only `openid` is allowed to say so.
pub fn parse_scopes(raw: &str) -> Vec<String> {
    // A seen-set rather than `Vec::dedup`, which only collapses *adjacent*
    // duplicates: "email profile email" would otherwise go out as
    // `scope=openid email profile email`. Order is the operator's, so keep it.
    let mut seen = std::collections::HashSet::new();
    raw.split([' ', ','])
        .map(str::trim)
        // `openid` is added by OidcClient::build; a second copy here would
        // send `scope=openid openid email`.
        .filter(|s| !s.is_empty() && *s != "openid")
        .filter(|s| seen.insert(s.to_owned()))
        .map(str::to_owned)
        .collect()
}

/// [`parse_scopes`] with the wizard's defaults for a blank field.
///
/// Only for form input, where "I left it empty" means "give me something
/// sensible". A *stored* empty list is a deliberate choice and
/// [`params`] honours it.
pub fn parse_scopes_or_default(raw: &str) -> Vec<String> {
    let parsed = parse_scopes(raw);
    if parsed.is_empty() {
        return default_scopes();
    }
    parsed
}

/// What the setup wizard pre-fills into its scopes field.
///
/// Wider than [`crate::server::config`]'s config-file default, and
/// deliberately so: the wizard's next screen asks the operator to pick a group
/// claim out of their own token, which only works if a groups scope was
/// requested. `openid` is added by the client itself and is absent here.
pub fn default_scopes() -> Vec<String> {
    vec!["email".into(), "profile".into(), "groups".into()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    #[test]
    fn scopes_accept_spaces_or_commas_and_never_repeat_openid() {
        // `openid` is added by OidcClient::build; carrying a second copy
        // through the settings would send `scope=openid openid email`.
        assert_eq!(parse_scopes("openid email profile"), ["email", "profile"]);
        assert!(parse_scopes("openid").is_empty());
        assert_eq!(
            parse_scopes("email, profile ,groups"),
            ["email", "profile", "groups"]
        );
    }

    #[test]
    fn a_blank_form_field_falls_back_to_the_defaults() {
        assert_eq!(parse_scopes_or_default("   "), default_scopes());
        assert_eq!(parse_scopes_or_default("openid"), default_scopes());
    }

    #[tokio::test]
    async fn a_stored_empty_scope_list_is_honoured_not_defaulted() {
        // A legacy `scopes = []` means "openid only". Substituting the wizard
        // defaults there starts requesting `groups`, and a provider that does
        // not know that scope rejects the whole authorization request.
        let pool = open(Path::new(":memory:")).await.unwrap();
        let crypto = Crypto::from_key([9u8; 32]);
        set_params(
            &pool,
            &crypto,
            &OidcParams {
                issuer: "https://id.example.com".into(),
                client_id: "gw".into(),
                client_secret: "s".into(),
                scopes: vec![],
                roles_claim: None,
            },
        )
        .await
        .unwrap();

        assert!(
            params(&pool, &crypto)
                .await
                .unwrap()
                .unwrap()
                .scopes
                .is_empty()
        );
    }

    #[test]
    fn scopes_drop_non_adjacent_duplicates_too() {
        // `Vec::dedup` only collapses neighbours, so this used to go out as
        // `scope=openid email profile email`.
        assert_eq!(parse_scopes("email profile email"), ["email", "profile"]);
    }

    #[tokio::test]
    async fn params_round_trip_with_the_secret_sealed() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let crypto = Crypto::from_key([3u8; 32]);
        let want = OidcParams {
            issuer: "https://id.example.com/realms/x".into(),
            client_id: "gw".into(),
            client_secret: "s3cr3t".into(),
            scopes: vec!["email".into(), "groups".into()],
            roles_claim: Some("groups".into()),
        };
        set_params(&pool, &crypto, &want).await.unwrap();

        let got = params(&pool, &crypto).await.unwrap().unwrap();
        assert_eq!(got.issuer, want.issuer);
        assert_eq!(got.client_secret, want.client_secret);
        assert_eq!(got.scopes, want.scopes);
        assert_eq!(got.roles_claim, want.roles_claim);

        // The row itself must not carry the plaintext.
        let stored = app_settings::get(&pool, CLIENT_SECRET_KEY)
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.contains("s3cr3t"), "{stored}");
    }

    #[tokio::test]
    async fn a_secret_sealed_under_another_key_reads_as_unconfigured() {
        // The at-rest key changed. Sign-in must report "not configured" (which
        // `restore-setup` can fix) rather than surfacing a decryption error.
        let pool = open(Path::new(":memory:")).await.unwrap();
        set_params(
            &pool,
            &Crypto::from_key([1u8; 32]),
            &OidcParams {
                issuer: "https://id.example.com".into(),
                client_id: "gw".into(),
                client_secret: "s".into(),
                scopes: vec![],
                roles_claim: None,
            },
        )
        .await
        .unwrap();

        assert!(
            params(&pool, &Crypto::from_key([2u8; 32]))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn clearing_the_roles_claim_removes_the_row() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let crypto = Crypto::from_key([4u8; 32]);
        let mut p = OidcParams {
            issuer: "https://id.example.com".into(),
            client_id: "gw".into(),
            client_secret: "s".into(),
            scopes: vec![],
            roles_claim: Some("groups".into()),
        };
        set_params(&pool, &crypto, &p).await.unwrap();
        p.roles_claim = Some("   ".into());
        set_params(&pool, &crypto, &p).await.unwrap();

        assert!(
            app_settings::get(&pool, ROLES_CLAIM_KEY)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            params(&pool, &crypto)
                .await
                .unwrap()
                .unwrap()
                .roles_claim
                .is_none()
        );
    }
}
