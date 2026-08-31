// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Browser consent for a RAG source that cannot be authorised by a typed
//! credential.
//!
//! Two handlers, and no provider is named in either of them. Which providers
//! need consent, where to send the browser, and what to ask for all come from
//! [`ProviderFactory::auth`] — so a second OAuth source (OneDrive, Dropbox,
//! Box) is a factory that returns [`AuthKind::OAuth2`] and nothing here
//! changes.
//!
//! The consent is per *collection*, not per user: a RAG corpus is shared, and
//! the background worker that indexes it has no user session to borrow. The
//! refresh token is sealed into the collection's existing
//! `source_secrets_ct` blob beside the client secret, so it inherits the
//! at-rest encryption and the delete-cascade that blob already has, and no new
//! secret store appears.
//!
//! Whoever grants access decides what the gateway can read: every user who
//! can search the collection sees documents through *that* account's
//! permissions. The consent screen names the account, and the collection page
//! shows it afterwards, so this is visible rather than implied.

use std::sync::Arc;

use gateway_core::server::auth::mcp_oauth;
use gateway_core::server::db::rag as rag_db;
use gateway_core::server::db::rag_oauth as oauth_db;
use gateway_features::server::rag::source::{AuthKind, REFRESH_TOKEN_KEY};
use gateway_runtime::rama_server::state::RamaState;
use rama::http::service::web::extract::{Path, Query, State};
use rama::http::{Request, Response};
use serde::Deserialize;
use session_core::i18n::{self, Lang, t, t_args};

use super::{internal_error_html, require_admin_or_403, see_other};

/// Where Google sends the browser back to. One route for every provider —
/// the pending row says which collection and which kind it belongs to.
pub const CALLBACK_PATH: &str = "/rag/oauth/callback";

/// GET `/rag/{id}/connect` — start the consent flow for a collection's source.
pub async fn rag_connect(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let collection = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return internal_error_html(&user.email, &t(lang, "rag-toast-vanished")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "rag oauth: collection lookup");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-lookup-failed"));
        }
    };

    let registry = state.provider_registry();
    let Some(factory) = registry.get(&collection.source.kind) else {
        return internal_error_html(&user.email, &t(lang, "rag-source-unknown-kind"));
    };
    let AuthKind::OAuth2 {
        authorize_url,
        token_url,
        scopes,
        client_id_key,
        ..
    } = factory.auth()
    else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-not-oauth"));
    };

    // The client the operator registered with the provider. Its id is plain
    // config; its secret is only needed at the token exchange, so it stays
    // sealed until the callback.
    let Some(client_id) = collection
        .source
        .config
        .get(client_id_key)
        .filter(|s| !s.is_empty())
    else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-no-client"));
    };

    let public = state.config.gateway.public_url.trim_end_matches('/');
    let redirect_uri = format!("{public}{CALLBACK_PATH}");
    let pkce = mcp_oauth::pkce();
    let oauth_state = mcp_oauth::random_state();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();

    let authorize = match mcp_oauth::build_authorize_url(
        authorize_url,
        client_id,
        &redirect_uri,
        &scopes,
        &oauth_state,
        &pkce.challenge,
        // RFC 8707 audience binding is an MCP concern; an ordinary OAuth
        // provider has no audience to name and may reject the parameter.
        None,
    ) {
        Ok(u) => u,
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: building the authorize url");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-bad-authorize-url"));
        }
    };

    let pending = oauth_db::PendingSourceOauth {
        state: oauth_state,
        collection_id: id,
        source_kind: collection.source.kind.clone(),
        pkce_verifier: pkce.verifier,
        redirect_uri,
        token_url: token_url.to_string(),
        admin_user_id: user.id.clone(),
    };
    if let Err(err) = oauth_db::create_pending(&state.db, &pending).await {
        tracing::warn!(error = %err, %id, "rag oauth: saving pending consent");
        return internal_error_html(&user.email, &t(lang, "rag-oauth-start-failed"));
    }
    see_other(&authorize)
}

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// GET `/rag/oauth/callback` — finish the flow and seal the refresh token.
pub async fn rag_oauth_callback(
    State(state): State<Arc<RamaState>>,
    Query(params): Query<CallbackParams>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    // Admin, not merely signed in: this hands a shared corpus a credential.
    // Gating on the session alone would let any authenticated user who
    // reached the redirect with a live `state` complete someone else's grant.
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    if let Some(err) = params.error {
        return internal_error_html(
            &user.email,
            &t_args(
                lang,
                "rag-oauth-provider-refused",
                &i18n::args([("error", err.into())]),
            ),
        );
    }
    let (Some(code), Some(st)) = (params.code, params.state) else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-callback-missing"));
    };

    // Consumed on read: an unknown, replayed or expired state all look the
    // same from here, and all mean "start again".
    let pending = match oauth_db::take_pending(&state.db, &st).await {
        Ok(Some(p)) => p,
        Ok(None) => return internal_error_html(&user.email, &t(lang, "rag-oauth-expired")),
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: reading pending consent");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-lookup-failed"));
        }
    };

    let collection = match rag_db::find_collection_by_id(&state.db, pending.collection_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return internal_error_html(&user.email, &t(lang, "rag-toast-vanished")),
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: collection lookup");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-lookup-failed"));
        }
    };

    // Open the stored secrets so the client secret can be used and the new
    // refresh token can be folded in beside it.
    // Which keys hold the client credentials is the provider's to say, not
    // this handler's to assume.
    let Some(factory) = state.provider_registry().get(&collection.source.kind) else {
        return internal_error_html(&user.email, &t(lang, "rag-source-unknown-kind"));
    };
    let AuthKind::OAuth2 {
        client_id_key,
        client_secret_key,
        ..
    } = factory.auth()
    else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-not-oauth"));
    };

    let mut secrets = collection.source.open_secrets(&state.crypto);
    let Some(client_id) = collection
        .source
        .config
        .get(client_id_key)
        .filter(|s| !s.is_empty())
    else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-no-client"));
    };
    let client_secret = secrets.get(client_secret_key).cloned();

    let tokens = match mcp_oauth::exchange_code(
        &state.http,
        &pending.token_url,
        &code,
        &pending.pkce_verifier,
        &pending.redirect_uri,
        client_id,
        client_secret.as_deref(),
        None,
    )
    .await
    {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: exchanging the code");
            return internal_error_html(
                &user.email,
                &t_args(
                    lang,
                    "rag-oauth-exchange-failed",
                    &i18n::args([("error", err.to_string().into())]),
                ),
            );
        }
    };

    // No refresh token means no unattended indexing: the access token dies in
    // an hour and the corpus stops updating with a 401 nobody is watching for.
    // Google withholds it when the account has already granted consent and the
    // request did not force the prompt, so say what to do about it.
    let Some(refresh_token) = tokens.refresh_token else {
        return internal_error_html(&user.email, &t(lang, "rag-oauth-no-refresh-token"));
    };
    secrets.insert(REFRESH_TOKEN_KEY.to_string(), refresh_token);

    let json = match serde_json::to_string(&secrets) {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: serialising secrets");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-store-failed"));
        }
    };
    let sealed = match state.crypto.seal_str(&json) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "rag oauth: sealing secrets");
            return internal_error_html(&user.email, &t(lang, "rag-oauth-store-failed"));
        }
    };
    if let Err(err) =
        rag_db::set_source_secrets(&state.db, pending.collection_id, Some(&sealed)).await
    {
        tracing::warn!(error = %err, "rag oauth: storing the refresh token");
        return internal_error_html(&user.email, &t(lang, "rag-oauth-store-failed"));
    }

    // Record whose access this corpus is now read through. Asking the
    // provider rather than trusting the session: the person who clicked
    // Connect and the Google account they picked on the consent screen are
    // not necessarily the same, and it is the latter that decides what the
    // index can see.
    let account = match state.provider_registry().build(
        &collection.source.kind,
        &gateway_features::server::rag::source::ProviderConfig::new(
            collection.source.config.clone(),
            secrets.clone(),
        ),
        state.http.clone(),
    ) {
        Ok(provider) => provider.probe().await.ok().and_then(|r| r.account),
        Err(err) => {
            // Not fatal: the token is stored and the corpus will index. We
            // just cannot name the account yet.
            tracing::warn!(error = %err, "rag oauth: naming the connected account");
            None
        }
    };
    if let Err(err) = rag_db::set_connected_account(
        &state.db,
        pending.collection_id,
        account.as_deref(),
        &user.email,
    )
    .await
    {
        tracing::warn!(error = %err, "rag oauth: recording the connected account");
    }

    // The corpus could not be read before this moment, so whatever is indexed
    // was built without the source. Queue a build rather than leave a
    // connected collection sitting idle until someone notices.
    if let Some(indexer) = state.indexer.as_ref() {
        for r in rag_db::list_refs(&state.db, pending.collection_id)
            .await
            .unwrap_or_default()
        {
            let _ = indexer.request_full_rebuild(r.id).await;
        }
    }
    tracing::info!(
        collection_id = pending.collection_id,
        kind = %pending.source_kind,
        started_by = %pending.admin_user_id,
        completed_by = %user.email,
        account = ?account,
        "rag oauth: source connected"
    );
    see_other("/rag")
}
