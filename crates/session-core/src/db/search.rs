// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// Turn a raw user search string into a safe FTS5 MATCH expression.
///
/// The user types free text; FTS5's MATCH grammar treats characters like
/// `.`, `-`, `:`, `"`, `*`, `(`, and bare `AND`/`OR`/`NEAR` as operators,
/// so passing the raw string through throws "fts5: syntax error" on
/// perfectly reasonable queries (a filename like `config.yaml`, a flag
/// like `--verbose`, an email). We defuse this by splitting on Unicode
/// whitespace and wrapping each token as a double-quoted FTS5 *string*
/// (a `""` escapes an embedded quote). Quoted tokens are literal — no
/// operator is honoured inside them — and joining them with spaces makes
/// FTS5 require every token (implicit AND), which is the intuitive
/// "results contain all my words" behaviour. Language-agnostic: it never
/// inspects the words themselves, only whitespace boundaries.
///
/// Returns an empty string when the query is blank or all-punctuation,
/// which the caller treats as "no search" (falls back to the normal list).
pub(crate) fn to_fts_match_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sentinel characters `snippet()` wraps each matching term with: ASCII STX
/// (0x02) opens a highlight, ETX (0x03) closes it. We ask FTS5 to mark
/// matches with these control chars (via `char(2)`/`char(3)` in the query)
/// rather than literal `<b>`/`</b>`, HTML-escape the surrounding text in
/// [`highlight_snippet`], then swap the sentinels for real `<b>` tags — so a
/// `<script>` a user typed renders as inert text while the highlight markup
/// we control survives.
///
/// For this to be unambiguous the sentinels must never occur in the indexed
/// text itself (otherwise a user-typed STX would be rewritten into stray
/// `<b>`). [`strip_fts_sentinels`] enforces that at every write to an indexed
/// column, so any sentinel reaching [`highlight_snippet`] is one we injected.
pub(crate) const SNIPPET_OPEN: char = '\u{2}';
pub(crate) const SNIPPET_CLOSE: char = '\u{3}';

/// Remove the FTS highlight sentinels ([`SNIPPET_OPEN`] / [`SNIPPET_CLOSE`])
/// from text bound for an FTS-indexed column (`user_content` / `content`).
/// These ASCII control chars have no legitimate place in chat text or
/// markdown, and stripping them keeps [`highlight_snippet`]'s sentinel
/// invariant true — a user can't smuggle raw STX/ETX into a conversation to
/// forge highlight markup in the search sidebar. Borrows unchanged when the
/// text (the overwhelmingly common case) contains neither char.
pub(crate) fn strip_fts_sentinels(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(SNIPPET_OPEN) || s.contains(SNIPPET_CLOSE) {
        std::borrow::Cow::Owned(s.replace([SNIPPET_OPEN, SNIPPET_CLOSE], ""))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Convert a raw `snippet()` result — which embeds the [`SNIPPET_OPEN`] /
/// [`SNIPPET_CLOSE`] control chars around matches and otherwise contains
/// verbatim, attacker-influenced conversation text — into a safe HTML
/// fragment: the surrounding text is HTML-escaped (via
/// [`crate::chrome::escape_html`]) so it can't inject markup, then the
/// sentinels become `<b>`/`</b>`. Since [`strip_fts_sentinels`] guarantees
/// no sentinel survives in the indexed text, every sentinel here is ours.
pub(crate) fn highlight_snippet(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    // Walk sentinel-delimited segments: escape the text, emit our own tags
    // at the boundaries. `split_inclusive` would keep the delimiter; instead
    // scan char-by-char, buffering plain runs so escaping runs once per run.
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut String| {
        if !buf.is_empty() {
            out.push_str(&crate::chrome::escape_html(buf));
            buf.clear();
        }
    };
    for ch in raw.chars() {
        match ch {
            SNIPPET_OPEN => {
                flush(&mut buf, &mut out);
                out.push_str("<b>");
            }
            SNIPPET_CLOSE => {
                flush(&mut buf, &mut out);
                out.push_str("</b>");
            }
            _ => buf.push(ch),
        }
    }
    flush(&mut buf, &mut out);
    out
}

/// Full-text search across a user's conversations. Matches against the
/// conversation *title* and against user prompts and assistant responses
/// (excluding reasoning/thinking). Returns a ranked list of conversations
/// with highlighted snippets.
///
/// Title matches come first: the FTS index only covers turn text, so a term
/// that appears only in a (usually LLM-generated) title — e.g. searching
/// "E2E" for a chat named "E2E" — would otherwise never surface. Titles are
/// matched with a case-insensitive `LIKE` per whitespace token (all tokens
/// must be present), then the FTS content matches fill in the rest.
///
/// The raw query is normalised into a safe FTS5 MATCH expression by
/// [`to_fts_match_query`]. A blank / punctuation-only query returns the
/// normal recency-ordered list (via [`list_sessions`]).
///
/// One row per conversation: the query groups by session and keeps the
/// best-ranked matching turn (its snippet), so `limit` counts
/// *conversations*, not turns — a chatty conversation with many hits can't
/// crowd other matching conversations out of the results. Snippets are
/// HTML-escaped (see [`highlight_snippet`]) with only the match highlight
/// as live markup.
pub async fn search_sessions(
    pool: &Pool,
    user_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, DbError> {
    let match_query = to_fts_match_query(query);
    if match_query.is_empty() {
        // Empty or punctuation-only query → fall back to the normal list,
        // capped at `limit` so the "limit counts conversations" contract
        // holds on this path too (a user with hundreds of chats must not
        // flood the sidebar patch).
        let sessions = list_sessions(pool, user_id).await?;
        return Ok(sessions
            .into_iter()
            .take(limit)
            .map(|s| SearchHit {
                session_id: s.id,
                title: s.title,
                updated_at: s.updated_at,
                pinned: s.pinned,
                snippet: String::new(),
            })
            .collect());
    }

    // Two-stage query. The inner `matches` CTE queries the FTS table
    // *directly* with no joins — the only context where `bm25()` and
    // `snippet()` are allowed (a join lets the planner reach the FTS table
    // by rowid, which breaks the auxiliary functions). It yields one row
    // per matching turn: rowid + rank + highlighted snippet. The outer
    // query joins that to `chat_turns`/`chat_sessions` for metadata and
    // GROUP BYs session — `MIN(rank)` picks each conversation's best turn,
    // and SQLite's "bare column" rule takes the remaining columns (incl.
    // `snippet`) from that same min-rank row. Grouping *before* LIMIT means
    // LIMIT counts conversations, not turns — a chatty conversation with
    // many hits can't crowd others out. The `char(2)`/`char(3)` sentinels
    // become `<b>` after escaping (see `highlight_snippet`), so raw HTML in
    // the text can't inject markup.
    let rows = sqlx::query(
        r#"
        WITH matches AS MATERIALIZED (
            SELECT
                rowid AS turn_rowid,
                bm25(chat_turns_fts) AS rank,
                snippet(chat_turns_fts, -1, char(2), char(3), '…', 40) AS snippet
            FROM chat_turns_fts
            WHERE chat_turns_fts MATCH ?
        )
        SELECT
            s.id AS session_id,
            s.title AS title,
            s.updated_at AS updated_at,
            s.pinned AS pinned,
            MIN(m.rank) AS rank,
            m.snippet AS snippet
        FROM matches m
        JOIN chat_turns t ON t.rowid = m.turn_rowid
        JOIN chat_sessions s ON s.id = t.session_id
        WHERE s.user_id = ?
        GROUP BY s.id
        ORDER BY rank ASC, s.updated_at DESC
        LIMIT ?
        "#,
    )
    .bind(&match_query)
    .bind(user_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;

    let content_hits = rows
        .iter()
        .map(|row| {
            Ok(SearchHit {
                session_id: row.try_get("session_id")?,
                title: row.try_get("title")?,
                updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
                pinned: row.try_get::<i64, _>("pinned")? != 0,
                snippet: highlight_snippet(&row.try_get::<String, _>("snippet")?),
            })
        })
        .collect::<Result<Vec<SearchHit>, DbError>>()?;

    let title_hits = search_titles(pool, user_id, query, limit).await?;

    // Merge title matches ahead of content matches, deduplicated by session.
    // Title matches carry no snippet, so if the same conversation also matched
    // on content we graft that snippet on for context. `limit` counts distinct
    // conversations across both sources.
    let content_snippets: std::collections::HashMap<&str, &str> = content_hits
        .iter()
        .map(|h| (h.session_id.as_str(), h.snippet.as_str()))
        .collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<SearchHit> =
        Vec::with_capacity(limit.min(title_hits.len() + content_hits.len()));
    for mut h in title_hits {
        if h.snippet.is_empty()
            && let Some(sn) = content_snippets.get(h.session_id.as_str())
        {
            h.snippet = sn.to_string();
        }
        if seen.insert(h.session_id.clone()) {
            out.push(h);
            if out.len() >= limit {
                return Ok(out);
            }
        }
    }
    for h in content_hits {
        if out.len() >= limit {
            break;
        }
        if seen.insert(h.session_id.clone()) {
            out.push(h);
        }
    }
    Ok(out)
}

/// Escape a raw token for use inside a SQL `LIKE` pattern with `ESCAPE '\'`.
/// The wildcards `%` and `_` (and the escape char itself) become literals so a
/// user searching for e.g. `a_b` matches only `a_b`, not `axb`.
pub(crate) fn like_escape(tok: &str) -> String {
    let mut s = String::with_capacity(tok.len());
    for c in tok.chars() {
        if matches!(c, '\\' | '%' | '_') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Find a user's conversations whose *title* contains every whitespace token of
/// `query` (case-insensitive). Complements the FTS content search in
/// [`search_sessions`], since the FTS index does not cover titles. Returned
/// hits carry an empty snippet (the title itself is the match). Ordered pinned
/// first, then most-recently-updated.
pub(crate) async fn search_titles(
    pool: &Pool,
    user_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, DbError> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("%{}%", like_escape(t)))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let conditions = vec!["title LIKE ? ESCAPE '\\'"; tokens.len()].join(" AND ");
    let sql = format!(
        r#"
        SELECT id AS session_id, title, updated_at, pinned
        FROM chat_sessions
        WHERE user_id = ? AND title IS NOT NULL AND {conditions}
        ORDER BY pinned DESC, updated_at DESC
        LIMIT ?
        "#
    );

    let mut q = sqlx::query(&sql).bind(user_id);
    for pat in &tokens {
        q = q.bind(pat);
    }
    let rows = q.bind(limit as i64).fetch_all(pool).await?;

    rows.iter()
        .map(|row| {
            Ok(SearchHit {
                session_id: row.try_get("session_id")?,
                title: row.try_get("title")?,
                updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
                pinned: row.try_get::<i64, _>("pinned")? != 0,
                snippet: String::new(),
            })
        })
        .collect()
}

/// One attachment object that the fork path must copy in the blob
/// store: the source turn-scoped key, the destination turn-scoped key,
/// and the (raw, un-encoded) filename they share. Returned by
/// [`fork_session`] so the gateway — which owns the S3 client —
/// performs the byte copy while this crate stays storage-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCopy {
    pub from_turn_id: String,
    pub to_turn_id: String,
    pub filename: String,
}

/// One conversation matching a search query. Returned by
/// [`search_sessions`] with a highlighted snippet of where the term
/// matched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// The conversation's session id.
    pub session_id: String,
    /// The conversation's title. `None` when it was never titled; the
    /// renderer supplies the "Untitled chat" fallback (this layer does not
    /// substitute).
    pub title: Option<String>,
    /// Last activity timestamp.
    pub updated_at: Timestamp,
    /// Whether the conversation is pinned.
    pub pinned: bool,
    /// A highlighted, **HTML-safe** excerpt of the matching content: the
    /// surrounding text is HTML-escaped and only the match itself is wrapped
    /// in `<b>`/`</b>` (see [`highlight_snippet`]). Safe to render as raw
    /// HTML. Empty for the no-query fallback listing.
    pub snippet: String,
}
