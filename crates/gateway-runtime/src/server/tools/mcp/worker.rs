// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Background maintenance for per-user MCP connections.
//!
//! OAuth access tokens are short-lived and refresh tokens can lapse from
//! *inactivity* on some providers (e.g. Google revokes idle grants). The
//! on-demand refresh path keeps an *actively used* connector alive, but an
//! idle one would eventually expire. This loop proactively refreshes
//! connections that are near expiry — exercising the refresh token regularly
//! so inactivity timers reset and access tokens stay fresh even when the user
//! hasn't touched the connector. It also sweeps expired pending-OAuth rows.
//!
//! Refreshes go through [`super::manager::McpConnectionManager::refresh_connection`],
//! which serializes per `(user, connector)` so the worker can't race a live
//! request and double-spend a rotating refresh token. A connection whose
//! refresh ultimately fails is parked (the store shows "needs reconnect") as
//! either `error` or — when the authorization server declared the credential
//! dead — `reauth`, which also pushes a notification, since only the user can
//! fix that one and they'd otherwise discover it mid-conversation. The loop
//! never panics — a failed pass is logged and retried.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use crate::rama_server::state::RamaState;
use gateway_core::server::db::DbError;
use gateway_core::server::db::user_mcp;

/// How often the maintenance pass runs.
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Refresh any connection whose access token expires within this window —
/// set a little above [`POLL_INTERVAL`] so a token can't slip through the gap
/// between two passes.
const REFRESH_WINDOW_SECS: i64 = 35 * 60;

/// For connections whose provider returned no `expires_in`, refresh when they
/// haven't been refreshed in this long, to keep the refresh token exercised.
const KEEPALIVE_SECS: i64 = 6 * 60 * 60;

/// Spawn the maintenance loop. Runs until the process exits.
pub fn spawn(state: Arc<RamaState>) {
    tokio::spawn(async move {
        loop {
            if let Err(err) = drain_once(&state).await {
                tracing::warn!(error = %err, "MCP connection-maintenance pass failed");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// One pass: sweep stale pending authorizations, then proactively refresh
/// every connection due for it.
async fn drain_once(state: &Arc<RamaState>) -> Result<(), DbError> {
    if let Ok(swept) = user_mcp::sweep_expired_pending(&state.db).await
        && swept > 0
    {
        tracing::debug!(swept, "swept expired pending MCP authorizations");
    }

    let now = Timestamp::now();
    let expiring_before = now + jiff::Span::new().seconds(REFRESH_WINDOW_SECS);
    let stale_before = now - jiff::Span::new().seconds(KEEPALIVE_SECS);
    let due =
        user_mcp::connections_due_for_refresh(&state.db, expiring_before, stale_before).await?;
    if due.is_empty() {
        return Ok(());
    }
    tracing::info!(count = due.len(), "proactively refreshing MCP connections");
    for conn in due {
        if let Err(err) = state
            .mcp
            .refresh_connection(&conn.user_id, &conn.connector_key)
            .await
        {
            tracing::warn!(
                user = %conn.user_id, connector = %conn.connector_key,
                error = %err, needs_reauth = err.needs_reauth,
                "proactive MCP token refresh failed — connection marked needs-reconnect"
            );
            // A dead credential is the user's to fix and they'd otherwise find
            // out mid-conversation, when a tool call quietly goes missing. A
            // transient failure gets no ping — the next pass may well fix it.
            if err.needs_reauth {
                notify_needs_reconnect(state, &conn.user_id, &conn.connector_key).await;
            }
        }
    }
    Ok(())
}

/// Ping the user's subscribed browsers that a connector needs re-authorizing.
///
/// Best-effort and quiet: no-op unless push is configured and the user has a
/// subscription, every failure only logged. Fires at most once per disconnect —
/// the connection has just left `connected`, so the sweep won't pick it up
/// again until the user reconnects.
async fn notify_needs_reconnect(state: &Arc<RamaState>, user_id: &str, connector_key: &str) {
    use gateway_core::server::db::{mcp_catalog, push_subscriptions};
    use gateway_features::server::push::{PushMessage, SendOutcome};
    use session_core::i18n::{self, Lang, t, t_args};

    let Some(push) = state.push.clone() else {
        return;
    };
    let subs = match push_subscriptions::list_for_user(&state.db, user_id).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return,
        Err(err) => {
            tracing::warn!(error = %err, "push: listing subscriptions for connector reconnect");
            return;
        }
    };
    // The display name the user knows the connector by ("Google Workspace"),
    // falling back to its key if the catalog row went away underneath us.
    let name = mcp_catalog::get(&state.db, connector_key)
        .await
        .ok()
        .flatten()
        .map(|c| c.name)
        .unwrap_or_else(|| connector_key.to_string());

    for sub in subs {
        let lang = sub
            .lang
            .as_deref()
            .and_then(Lang::from_code)
            .unwrap_or(Lang::En);
        let message = PushMessage {
            title: t(lang, "push-connector-reconnect-title"),
            body: t_args(
                lang,
                "push-connector-reconnect-body",
                &i18n::args([("connector", name.clone().into())]),
            ),
            url: "/integrations".to_string(),
            // One connector, one notification — a later ping for the same
            // connector replaces it rather than stacking.
            tag: format!("connector-{connector_key}"),
        };
        if push.send(&sub, &message).await == SendOutcome::Gone
            && let Err(err) = push_subscriptions::delete(&state.db, &sub.id).await
        {
            tracing::warn!(error = %err, "push: pruning gone subscription");
        }
    }
}
