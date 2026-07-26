// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rendering the chat document canvas.
//!
//! Lives here rather than beside the `create_document` / `edit_document` tools
//! (now in `gateway-tools`) because it has three consumers on two different
//! layers: the chat page's initial render and doc/version-switch route in
//! `gateway-web`, and the tools' own live SSE inject in `gateway-tools`. Both
//! sit above this crate, so the shared renderer has to sit below both.
//!
//! It touches no tool machinery at all — just the `documents` store and
//! `session_core::render` — so nothing is lost by separating it.

use gateway_core::server::db::documents;

/// Render the session's canvas panel (the active = most-recently-updated
/// document, or `active_id` if given, at `version` or latest) to an HTML
/// string. `Ok(None)` when the session has no documents. Shared by the
/// initial page render, the live SSE inject, and the doc/version-switch
/// route so all three stay byte-identical.
pub async fn render_canvas_html(
    pool: &gateway_core::server::db::Pool,
    session_id: &str,
    active_id: Option<&str>,
    version: Option<i64>,
    lang: session_core::i18n::Lang,
) -> Result<Option<String>, gateway_core::server::db::DbError> {
    // Live documents only — a deleted document is out of the panel and out of
    // its document picker.
    let docs = documents::list_for_session(pool, session_id, false).await?;
    if docs.is_empty() {
        return Ok(None);
    }
    // Default to the most-recently-updated document (list is ordered). An
    // `active_id` that isn't in the live list falls back to that default:
    // `get_version` resolves soft-deleted documents on purpose (deletion
    // hides, it doesn't unresolve), so filtering against `docs` — not the
    // `None` branch below — is what keeps a deleted document out of the
    // panel when a stale link or a pre-delete SSE patch still names it.
    let active = active_id
        .filter(|id| docs.iter().any(|d| d.id == *id))
        .unwrap_or(&docs[0].id);
    let Some((doc, ver)) = documents::get_version(pool, session_id, active, version).await? else {
        // Asked for a doc/version that isn't in this session — fall back
        // to the latest document so the panel never renders empty.
        return Box::pin(render_canvas_html(pool, session_id, None, None, lang)).await;
    };
    let all_docs: Vec<(String, String)> = docs
        .iter()
        .map(|d| (d.id.clone(), d.title.clone()))
        .collect();
    let canvas = session_core::render::DocCanvas {
        session_id,
        active_id: &doc.id,
        title: &doc.title,
        format: doc.format.as_str(),
        version: ver.version,
        max_version: doc.current_ver,
        content: &ver.content,
        all_docs,
    };
    Ok(Some(session_core::render::render_document_canvas(
        &canvas, lang,
    )))
}
