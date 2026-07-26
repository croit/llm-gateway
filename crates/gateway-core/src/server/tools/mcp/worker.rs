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
//! refresh ultimately fails is marked `error` (the store shows "needs
//! reconnect"); the loop never panics — a failed pass is logged and retried.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use crate::rama_server::state::RamaState;
use crate::server::db::DbError;
use crate::server::db::user_mcp;

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
                user = %conn.user_id, connector = %conn.connector_key, error = %err,
                "proactive MCP token refresh failed — connection marked needs-reconnect"
            );
        }
    }
    Ok(())
}
