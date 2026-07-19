// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Persisted session conversations — used by every `SessionDriver`
//! implementation (today: the gateway's OpenAI-backed turns).
//!
//! Three tables (still keyed off the legacy `chat_*` names; the
//! rename to `session_*` rides in a follow-up migration):
//!
//! - `chat_sessions` — one row per conversation thread, scoped to a
//!   user. Single-tenant callers can pass a constant user id.
//! - `chat_turns` — one row per message in a thread. Role `user`
//!   carries the prompt; role `assistant` carries the streamed reply
//!   with `status` cycling through `in_progress → completed |
//!   cancelled | errored`.
//! - `chat_tool_calls` — side table because one assistant turn can
//!   fan out into many tool invocations across rounds.
//!
//! A driver writes to these tables incrementally as deltas arrive
//! from the upstream so that a client disconnecting mid-stream
//! doesn't lose progress: a reconnect reads the partial row, renders
//! what's there, and tails the broadcast for the remainder.
//!
//! Migrations live in the binary that owns the SQLite file (today
//! that's the gateway's `crates/gateway/migrations/`); session-core
//! does not manage schema. Call `sweep_in_progress_at_startup` once
//! after migrations to evict orphaned `in_progress` assistant rows
//! left by a crash.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

/// Database pool re-export so callers don't have to depend on `sqlx`
/// directly for the type signature.
pub type Pool = sqlx::SqlitePool;

/// Errors session-core's persistence functions can return. Kept
/// narrow — `Open` and `Migrate` are the binary's responsibility,
/// not session-core's. Callers that bubble these up can wrap with
/// their own variant (e.g. the gateway has `DbError::Session(#[from]
/// session_core::db::DbError)`).
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("query: {0}")]
    Query(#[from] sqlx::Error),
    #[error("decoding row column `{column}`")]
    Decode {
        column: &'static str,
        #[source]
        source: anyhow::Error,
    },
}

// ---------------------------------------------------------------------------
// Types

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    /// Title shown in the sidebar. None until a heuristic (or the user)
    /// fills it in. Renderer falls back to the first user message
    /// truncated.
    pub title: Option<String>,
    pub created_at: Timestamp,
    /// Bumped whenever a turn is created in this session — sidebar
    /// listing sorts most-recent first off this column.
    pub updated_at: Timestamp,
    /// When true, any signed-in user who knows this session's id may
    /// *read* it (the UUID is the capability). Mutations stay owner-only
    /// regardless. Toggled by the owner via `set_shared`.
    pub shared: bool,
    /// When true, the conversation is "pinned" — `list_sessions` floats
    /// it above the recency order so it stays reachable in the sidebar.
    /// Pure UI affordance; never affects readability. Toggled by the
    /// owner via `set_pinned`.
    pub pinned: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnRole {
    User,
    Assistant,
}

impl TurnRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
    fn parse(s: &str) -> Result<Self, DbError> {
        match s {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            _ => Err(DbError::Decode {
                column: "role",
                source: anyhow::anyhow!("unknown chat turn role `{s}`"),
            }),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStatus {
    /// Streaming in progress. Only valid for assistant turns.
    InProgress,
    /// Stream finished naturally.
    Completed,
    /// User pressed stop (or a fresh submit cancelled this one before
    /// the worker chose to keep going — see runner policy).
    Cancelled,
    /// Worker hit an error path (upstream non-2xx, malformed SSE,
    /// internal panic guard). `error_message` carries the human form.
    Errored,
}

impl TurnStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Errored => "errored",
        }
    }
    fn parse(s: &str) -> Result<Self, DbError> {
        match s {
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "errored" => Ok(Self::Errored),
            _ => Err(DbError::Decode {
                column: "status",
                source: anyhow::anyhow!("unknown chat turn status `{s}`"),
            }),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Running,
    Completed,
    Errored,
}

impl ToolCallStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Errored => "errored",
        }
    }
    fn parse(s: &str) -> Result<Self, DbError> {
        match s {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "errored" => Ok(Self::Errored),
            _ => Err(DbError::Decode {
                column: "status",
                source: anyhow::anyhow!("unknown tool call status `{s}`"),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub role: TurnRole,
    pub user_content: Option<String>,
    pub model: Option<String>,
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_elapsed_ms: Option<i64>,
    /// When the model emitted its first reasoning chunk. The anchor the
    /// client-side thinking timer counts up from (and a mid-stream reload
    /// resumes from). `None` until the turn reasons, or never for a turn
    /// that produces no reasoning.
    pub reasoning_started_at: Option<Timestamp>,
    pub status: TurnStatus,
    pub error_message: Option<String>,
    pub created_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    /// The model's `tool_call_id`. Doubles as the DOM id suffix.
    pub id: String,
    pub turn_id: String,
    pub seq: i64,
    pub name: String,
    pub arguments_json: String,
    pub output_json: Option<String>,
    pub status: ToolCallStatus,
    pub created_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}

/// Turn + its tool calls, fetched as one unit for rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnWithTools {
    pub turn: Turn,
    pub tool_calls: Vec<ToolCall>,
}

// ---------------------------------------------------------------------------
// Row decoding

fn parse_ts(s: String, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn parse_optional_ts(
    s: Option<String>,
    column: &'static str,
) -> Result<Option<Timestamp>, DbError> {
    s.map(|s| parse_ts(s, column)).transpose()
}

fn map_session(row: &SqliteRow) -> Result<Session, DbError> {
    Ok(Session {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        title: row.try_get("title")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
        shared: row.try_get::<i64, _>("shared")? != 0,
        pinned: row.try_get::<i64, _>("pinned")? != 0,
    })
}

fn map_turn(row: &SqliteRow) -> Result<Turn, DbError> {
    let role: String = row.try_get("role")?;
    let status: String = row.try_get("status")?;
    Ok(Turn {
        id: row.try_get("id")?,
        session_id: row.try_get("session_id")?,
        seq: row.try_get("seq")?,
        role: TurnRole::parse(&role)?,
        user_content: row.try_get("user_content")?,
        model: row.try_get("model")?,
        content: row.try_get("content")?,
        reasoning: row.try_get("reasoning")?,
        reasoning_elapsed_ms: row.try_get("reasoning_elapsed_ms")?,
        reasoning_started_at: parse_optional_ts(
            row.try_get("reasoning_started_at")?,
            "reasoning_started_at",
        )?,
        status: TurnStatus::parse(&status)?,
        error_message: row.try_get("error_message")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        completed_at: parse_optional_ts(row.try_get("completed_at")?, "completed_at")?,
    })
}

fn map_tool_call(row: &SqliteRow) -> Result<ToolCall, DbError> {
    let status: String = row.try_get("status")?;
    Ok(ToolCall {
        id: row.try_get("id")?,
        turn_id: row.try_get("turn_id")?,
        seq: row.try_get("seq")?,
        name: row.try_get("name")?,
        arguments_json: row.try_get("arguments_json")?,
        output_json: row.try_get("output_json")?,
        status: ToolCallStatus::parse(&status)?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        completed_at: parse_optional_ts(row.try_get("completed_at")?, "completed_at")?,
    })
}

// ---------------------------------------------------------------------------
// Sessions

/// Create a freshly empty conversation for `user_id`. Returns the new
/// row; the caller's next step is usually to redirect to its URL.
pub async fn create_session(pool: &Pool, user_id: &str) -> Result<Session, DbError> {
    let now = Timestamp::now();
    let s = Session {
        id: Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        title: None,
        created_at: now,
        updated_at: now,
        shared: false,
        pinned: false,
    };
    sqlx::query(
        r#"INSERT INTO chat_sessions (id, user_id, title, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(&s.id)
    .bind(&s.user_id)
    .bind(s.title.as_deref())
    .bind(s.created_at.to_string())
    .bind(s.updated_at.to_string())
    .execute(pool)
    .await?;
    Ok(s)
}

/// All conversations for a user. Pinned conversations float to the top
/// (in their own recency order); the rest follow, also most-recent first.
pub async fn list_sessions(pool: &Pool, user_id: &str) -> Result<Vec<Session>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, user_id, title, created_at, updated_at, shared, pinned
           FROM chat_sessions
           WHERE user_id = ?
           ORDER BY pinned DESC, updated_at DESC, id ASC"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_session).collect()
}

/// Look up a session by id, but only if it belongs to this user. Caller
/// uses the None case to send a 404 / redirect to /chat.
pub async fn get_session(
    pool: &Pool,
    user_id: &str,
    session_id: &str,
) -> Result<Option<Session>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, user_id, title, created_at, updated_at, shared, pinned
           FROM chat_sessions
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_session).transpose()
}

/// Owner of the chat session a given turn belongs to. Used by the
/// attachment-proxy route to authorize `GET /chat/attachment/<turn>/
/// <file>`: the caller's session must match the returned `user_id`,
/// otherwise user A could fetch user B's uploaded files by guessing
/// turn ids. `None` when the turn id doesn't exist.
pub async fn user_for_turn(pool: &Pool, turn_id: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query(
        r#"SELECT s.user_id AS user_id
           FROM chat_turns t
           JOIN chat_sessions s ON s.id = t.session_id
           WHERE t.id = ?"#,
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.try_get::<String, _>("user_id")).transpose()?)
}

/// Look up a session readable by `viewer_id`: either they own it, or it has
/// been shared (`shared = 1`). Used by the read-only paths (view, tail,
/// attachments) where any signed-in user holding the session's UUID may read
/// a shared conversation. Mutating paths must keep using the owner-only
/// [`get_session`].
pub async fn get_session_readable(
    pool: &Pool,
    viewer_id: &str,
    session_id: &str,
) -> Result<Option<Session>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, user_id, title, created_at, updated_at, shared, pinned
           FROM chat_sessions
           WHERE id = ? AND (user_id = ? OR shared = 1)"#,
    )
    .bind(session_id)
    .bind(viewer_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_session).transpose()
}

/// Set (or clear) a session's shared flag — owner-only. Returns true when a
/// row was updated (the caller owns it); false otherwise, so a non-owner's
/// attempt is a silent no-op rather than leaking existence via an error.
pub async fn set_shared(
    pool: &Pool,
    user_id: &str,
    session_id: &str,
    shared: bool,
) -> Result<bool, DbError> {
    let res = sqlx::query(r#"UPDATE chat_sessions SET shared = ? WHERE id = ? AND user_id = ?"#)
        .bind(shared as i64)
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Set (or clear) a session's pinned flag — owner-only. Returns true when a
/// row was updated (the caller owns it); false otherwise, so a non-owner's
/// attempt is a silent no-op rather than leaking existence via an error.
/// Same owner-scoping guarantee as [`set_shared`].
pub async fn set_pinned(
    pool: &Pool,
    user_id: &str,
    session_id: &str,
    pinned: bool,
) -> Result<bool, DbError> {
    let res = sqlx::query(r#"UPDATE chat_sessions SET pinned = ? WHERE id = ? AND user_id = ?"#)
        .bind(pinned as i64)
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Whether the chat session a given turn belongs to is readable by
/// `viewer_id` — owner or shared. Backs the attachment proxy so files in a
/// shared conversation are fetchable by a viewer, while a private turn's
/// files stay owner-only. `false` when the turn id doesn't exist.
pub async fn turn_session_readable(
    pool: &Pool,
    turn_id: &str,
    viewer_id: &str,
) -> Result<bool, DbError> {
    let row = sqlx::query(
        r#"SELECT 1 AS ok
           FROM chat_turns t
           JOIN chat_sessions s ON s.id = t.session_id
           WHERE t.id = ? AND (s.user_id = ? OR s.shared = 1)"#,
    )
    .bind(turn_id)
    .bind(viewer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Most-recent session for a user; None when they've never chatted. Used
/// by `GET /chat` to decide where to redirect.
pub async fn latest_session(pool: &Pool, user_id: &str) -> Result<Option<Session>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, user_id, title, created_at, updated_at, shared, pinned
           FROM chat_sessions
           WHERE user_id = ?
           ORDER BY updated_at DESC, id ASC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_session).transpose()
}

/// Delete a session (cascades to turns + tool_calls). Returns true iff
/// a row was actually removed — caller uses this to send a clean toast
/// vs a "not found" one.
pub async fn delete_session(pool: &Pool, user_id: &str, session_id: &str) -> Result<bool, DbError> {
    let result = sqlx::query(r#"DELETE FROM chat_sessions WHERE id = ? AND user_id = ?"#)
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

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
fn to_fts_match_query(query: &str) -> String {
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
const SNIPPET_OPEN: char = '\u{2}';
const SNIPPET_CLOSE: char = '\u{3}';

/// Remove the FTS highlight sentinels ([`SNIPPET_OPEN`] / [`SNIPPET_CLOSE`])
/// from text bound for an FTS-indexed column (`user_content` / `content`).
/// These ASCII control chars have no legitimate place in chat text or
/// markdown, and stripping them keeps [`highlight_snippet`]'s sentinel
/// invariant true — a user can't smuggle raw STX/ETX into a conversation to
/// forge highlight markup in the search sidebar. Borrows unchanged when the
/// text (the overwhelmingly common case) contains neither char.
fn strip_fts_sentinels(s: &str) -> std::borrow::Cow<'_, str> {
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
fn highlight_snippet(raw: &str) -> String {
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
fn like_escape(tok: &str) -> String {
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
async fn search_titles(
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

/// Copy an entire conversation into `new_user_id`'s account as a fresh,
/// **private** session (re-sharing is the new owner's decision). Title
/// and turn history are copied 1-to-1; every turn (and its tool calls)
/// gets a fresh id. Runs in one transaction.
///
/// Attachment markers in the copied turn text are rewritten so their
/// `/chat/attachment/<turn>/<file>` proxy URLs point at the *new* turn
/// ids — otherwise the fork's bubbles would reference the original
/// owner's turns and break the moment they un-share or delete. The
/// returned [`AttachmentCopy`] list tells the caller which blob objects
/// to duplicate (deduped, so a file referenced twice copies once); the
/// bytes themselves live in S3, which this crate doesn't touch.
///
/// An `in_progress` turn (a shared chat forked mid-stream) is copied as
/// `errored`, never live — the fork has no worker driving it, so a
/// copied spinner would hang forever.
pub async fn fork_session(
    pool: &Pool,
    src: &Session,
    new_user_id: &str,
) -> Result<(Session, Vec<AttachmentCopy>), DbError> {
    let src_turns = list_turns(pool, &src.id).await?;

    // Pre-mint every new turn id up front: a composer attachment's proxy
    // URL keys off the assistant turn id, which can be a *different* turn
    // than the one whose text carries the marker, so we need the whole
    // old→new map available while rewriting any single turn.
    let id_map: std::collections::HashMap<String, String> = src_turns
        .iter()
        .map(|t| (t.turn.id.clone(), Uuid::new_v4().to_string()))
        .collect();

    let now = Timestamp::now();
    let new_session = Session {
        id: Uuid::new_v4().to_string(),
        user_id: new_user_id.to_string(),
        title: src.title.clone(),
        created_at: now,
        updated_at: now,
        shared: false,
        // A fork starts unpinned — pinning, like re-sharing, is the new
        // owner's decision.
        pinned: false,
    };

    // Collect the blob copies as we go, deduped on (source turn, file):
    // the same object can be referenced by markers in more than one turn.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut copies: Vec<AttachmentCopy> = Vec::new();
    let mut record = |text: &str| {
        for att in crate::attachments::parse_markers(text) {
            let Some(old_turn) = crate::attachments::proxy_url_turn_id(&att.url) else {
                continue;
            };
            let Some(new_turn) = id_map.get(old_turn) else {
                continue;
            };
            if seen.insert((old_turn.to_string(), att.filename.clone())) {
                copies.push(AttachmentCopy {
                    from_turn_id: old_turn.to_string(),
                    to_turn_id: new_turn.clone(),
                    filename: att.filename,
                });
            }
        }
    };

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"INSERT INTO chat_sessions (id, user_id, title, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(&new_session.id)
    .bind(&new_session.user_id)
    .bind(new_session.title.as_deref())
    .bind(new_session.created_at.to_string())
    .bind(new_session.updated_at.to_string())
    .execute(&mut *tx)
    .await?;

    for tw in &src_turns {
        let turn = &tw.turn;
        let new_turn_id = id_map.get(&turn.id).expect("minted for every src turn");

        let user_content = turn.user_content.as_ref().map(|t| {
            record(t);
            crate::attachments::remap_attachment_turn_ids(t, &id_map)
        });
        let content = turn.content.as_ref().map(|t| {
            record(t);
            crate::attachments::remap_attachment_turn_ids(t, &id_map)
        });

        // Never copy an in-progress turn as live — no worker drives the
        // fork, so it would spin forever. Stamp it errored + completed.
        let (status, completed_at) = if turn.status == TurnStatus::InProgress {
            (TurnStatus::Errored, Some(now))
        } else {
            (turn.status, turn.completed_at)
        };

        sqlx::query(
            r#"INSERT INTO chat_turns
                  (id, session_id, seq, role, user_content, model, content,
                   reasoning, reasoning_elapsed_ms, reasoning_started_at,
                   status, error_message,
                   created_at, completed_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(new_turn_id)
        .bind(&new_session.id)
        .bind(turn.seq)
        .bind(turn.role.as_str())
        .bind(user_content)
        .bind(turn.model.as_deref())
        .bind(content)
        .bind(turn.reasoning.as_deref())
        .bind(turn.reasoning_elapsed_ms)
        .bind(turn.reasoning_started_at.map(|t| t.to_string()))
        .bind(status.as_str())
        .bind(turn.error_message.as_deref())
        .bind(turn.created_at.to_string())
        .bind(completed_at.map(|t| t.to_string()))
        .execute(&mut *tx)
        .await?;

        for tc in &tw.tool_calls {
            // Mint a fresh id: `chat_tool_calls.id` is a global PRIMARY KEY
            // (it's the model's tool_call_id), so reusing the source row's id
            // would collide with the original — and with any prior fork of the
            // same shared chat. The id is only a row/DOM handle once persisted,
            // so a new UUID is safe.
            sqlx::query(
                r#"INSERT INTO chat_tool_calls
                      (id, turn_id, seq, name, arguments_json, output_json,
                       status, created_at, completed_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(Uuid::new_v4().to_string())
            .bind(new_turn_id)
            .bind(tc.seq)
            .bind(&tc.name)
            .bind(&tc.arguments_json)
            .bind(tc.output_json.as_deref())
            .bind(tc.status.as_str())
            .bind(tc.created_at.to_string())
            .bind(tc.completed_at.map(|t| t.to_string()))
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;

    Ok((new_session, copies))
}

/// Bump `updated_at` so the session floats to the top of the sidebar.
/// Called after a new turn lands.
pub async fn touch_session(pool: &Pool, session_id: &str) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_sessions SET updated_at = ? WHERE id = ?"#)
        .bind(Timestamp::now().to_string())
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set the session title. Called once when the first user message lands
/// (auto-title = first user message truncated); the user may overwrite
/// later from the UI.
pub async fn set_session_title(pool: &Pool, session_id: &str, title: &str) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_sessions SET title = ? WHERE id = ?"#)
        .bind(title)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Turns

async fn next_turn_seq(pool: &Pool, session_id: &str) -> Result<i64, DbError> {
    let row = sqlx::query(
        r#"SELECT COALESCE(MAX(seq), -1) + 1 AS next_seq
           FROM chat_turns
           WHERE session_id = ?"#,
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("next_seq")?)
}

/// Insert a user-role turn (already 'completed' — the user's message is
/// final the moment we receive it). Returns the new turn.
///
/// `turn_id` is caller-supplied so attachments uploaded under that id's
/// S3 prefix have a stable key the chat-page render-refresh can find
/// later. Pre-generate at the handler entry and pass both here and to
/// the upload step.
pub async fn create_user_turn(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    content: &str,
) -> Result<Turn, DbError> {
    let seq = next_turn_seq(pool, session_id).await?;
    let now = Timestamp::now();
    let turn = Turn {
        id: turn_id.to_string(),
        session_id: session_id.to_string(),
        seq,
        role: TurnRole::User,
        user_content: Some(content.to_string()),
        model: None,
        content: None,
        reasoning: None,
        reasoning_elapsed_ms: None,
        reasoning_started_at: None,
        status: TurnStatus::Completed,
        error_message: None,
        created_at: now,
        completed_at: Some(now),
    };
    sqlx::query(
        r#"INSERT INTO chat_turns
              (id, session_id, seq, role, user_content, status, created_at, completed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&turn.id)
    .bind(&turn.session_id)
    .bind(turn.seq)
    .bind(turn.role.as_str())
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn.status.as_str())
    .bind(turn.created_at.to_string())
    .bind(turn.completed_at.map(|t| t.to_string()))
    .execute(pool)
    .await?;
    Ok(turn)
}

/// Insert an assistant-role turn in `in_progress` state. The worker
/// fills in content/reasoning incrementally and calls `finalize_turn`
/// at the end.
///
/// The turn id is supplied by the caller so it can reserve the
/// per-user worker slot under that id *before* persisting anything —
/// see `chat_message_send`. If reservation fails, no DB row is
/// created and the id is simply discarded.
pub async fn create_assistant_turn_in_progress(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    model: &str,
) -> Result<Turn, DbError> {
    let seq = next_turn_seq(pool, session_id).await?;
    let now = Timestamp::now();
    let turn = Turn {
        id: turn_id.to_string(),
        session_id: session_id.to_string(),
        seq,
        role: TurnRole::Assistant,
        user_content: None,
        model: Some(model.to_string()),
        content: None,
        reasoning: None,
        reasoning_elapsed_ms: None,
        reasoning_started_at: None,
        status: TurnStatus::InProgress,
        error_message: None,
        created_at: now,
        completed_at: None,
    };
    sqlx::query(
        r#"INSERT INTO chat_turns
              (id, session_id, seq, role, model, status, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&turn.id)
    .bind(&turn.session_id)
    .bind(turn.seq)
    .bind(turn.role.as_str())
    .bind(model)
    .bind(turn.status.as_str())
    .bind(turn.created_at.to_string())
    .execute(pool)
    .await?;
    Ok(turn)
}

/// Append to an in-progress assistant turn's `content`. Worker batches
/// these every ~100ms; SQLite handles the small-string concat fine but
/// we don't want one write per token.
pub async fn append_content(pool: &Pool, turn_id: &str, chunk: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET content = COALESCE(content, '') || ?
           WHERE id = ?"#,
    )
    .bind(strip_fts_sentinels(chunk).as_ref())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Overwrite an in-progress assistant turn's `content` wholesale.
/// Unlike [`append_content`], this *replaces* the column — used when a
/// tool needs to rewrite prior markers rather than only add to them
/// (e.g. a typst re-render superseding the earlier render's chip within
/// the same turn). The live view re-renders full turn content from the
/// DB on every tick, so the rewrite is reflected without a delta-accrual
/// mismatch. Pairs with [`get_content`].
pub async fn set_content(pool: &Pool, turn_id: &str, content: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET content = ?
           WHERE id = ?"#,
    )
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the current `content` of a turn — used by tools that need
/// to inspect already-spliced attachment markers (e.g. to pick a
/// non-colliding filename for a same-turn re-upload). Returns `None`
/// for a missing row, `Some(String::new())` for a row whose content
/// is still SQL-NULL.
pub async fn get_content(pool: &Pool, turn_id: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query(r#"SELECT content FROM chat_turns WHERE id = ?"#)
        .bind(turn_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| {
        r.try_get::<Option<String>, _>("content")
            .ok()
            .flatten()
            .unwrap_or_default()
    }))
}

/// Append to an in-progress assistant turn's `reasoning`. Same
/// batching pattern as `append_content`.
pub async fn append_reasoning(pool: &Pool, turn_id: &str, chunk: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET reasoning = COALESCE(reasoning, '') || ?
           WHERE id = ?"#,
    )
    .bind(chunk)
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Anchor the reasoning timer. Called once, when the model emits its
/// first reasoning chunk. The client-side `<thinking-timer>` counts up
/// from this instant; a mid-stream reload / late SSE subscriber reads it
/// to resume the count instead of restarting at 0. The `IS NULL` guard
/// keeps it set-once even if the driver calls it more than once.
pub async fn set_reasoning_started(
    pool: &Pool,
    turn_id: &str,
    started_at: Timestamp,
) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns SET reasoning_started_at = ?
           WHERE id = ? AND reasoning_started_at IS NULL"#,
    )
    .bind(started_at.to_string())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Freeze the reasoning timer at its final duration. Called once, the
/// moment the model emits its first visible content delta (= it has
/// stopped reasoning), and at finalization for a reasoning-only turn.
/// This is the authoritative value the settled "Thought for X.Ys" label
/// renders from.
pub async fn set_reasoning_elapsed(
    pool: &Pool,
    turn_id: &str,
    elapsed_ms: i64,
) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_turns SET reasoning_elapsed_ms = ? WHERE id = ?"#)
        .bind(elapsed_ms)
        .bind(turn_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the largest `prompt_tokens` the upstream reported across an
/// assistant turn's rounds — a model-tokenizer-accurate measure of how
/// big the session's replayed context had grown by the end of the turn.
/// The gateway's compaction trigger reads it back via
/// [`latest_context_tokens`]. Column added in migration 0032.
pub async fn set_context_tokens(
    pool: &Pool,
    turn_id: &str,
    context_tokens: i64,
) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_turns SET context_tokens = ? WHERE id = ?"#)
        .bind(context_tokens)
        .bind(turn_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The most recently measured context size for a session — the
/// `context_tokens` of the latest turn that carried one. `None` when no
/// turn in the session has a measurement yet (e.g. the upstream never
/// reported usage). Used by the gateway's auto-compaction threshold check.
pub async fn latest_context_tokens(pool: &Pool, session_id: &str) -> Result<Option<i64>, DbError> {
    let row = sqlx::query(
        r#"SELECT context_tokens
           FROM chat_turns
           WHERE session_id = ? AND context_tokens IS NOT NULL
           ORDER BY seq DESC
           LIMIT 1"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.try_get::<i64, _>("context_tokens"))
        .transpose()
        .map_err(Into::into)
}

/// End-of-stream: flip the turn's status and stamp `completed_at`. The
/// worker calls this exactly once per assistant turn whether the turn
/// ended naturally, via cancel, or with an error.
pub async fn finalize_turn(
    pool: &Pool,
    turn_id: &str,
    status: TurnStatus,
    error_message: Option<&str>,
) -> Result<(), DbError> {
    if status == TurnStatus::InProgress {
        return Err(DbError::Decode {
            column: "status",
            source: anyhow::anyhow!("finalize_turn called with status=in_progress"),
        });
    }
    sqlx::query(
        r#"UPDATE chat_turns
           SET status = ?, error_message = ?, completed_at = ?
           WHERE id = ?"#,
    )
    .bind(status.as_str())
    .bind(error_message)
    .bind(Timestamp::now().to_string())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// All turns in a session, oldest first, each carrying its tool calls
/// (also oldest first). Used by the renderer for both initial page
/// load and the reconnect-tail path.
pub async fn list_turns(pool: &Pool, session_id: &str) -> Result<Vec<TurnWithTools>, DbError> {
    let turn_rows = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ?
           ORDER BY seq ASC"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    let turns: Vec<Turn> = turn_rows.iter().map(map_turn).collect::<Result<_, _>>()?;
    if turns.is_empty() {
        return Ok(Vec::new());
    }

    // One query for all tool calls in the session, then bucketed by
    // turn_id. Cheaper than N+1.
    let tool_rows = sqlx::query(
        r#"SELECT tc.id, tc.turn_id, tc.seq, tc.name, tc.arguments_json,
                  tc.output_json, tc.status, tc.created_at, tc.completed_at
           FROM chat_tool_calls tc
           JOIN chat_turns t ON t.id = tc.turn_id
           WHERE t.session_id = ?
           ORDER BY tc.turn_id, tc.seq ASC"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    let mut by_turn: std::collections::HashMap<String, Vec<ToolCall>> =
        std::collections::HashMap::new();
    for r in &tool_rows {
        let tc = map_tool_call(r)?;
        by_turn.entry(tc.turn_id.clone()).or_default().push(tc);
    }

    Ok(turns
        .into_iter()
        .map(|turn| TurnWithTools {
            tool_calls: by_turn.remove(&turn.id).unwrap_or_default(),
            turn,
        })
        .collect())
}

/// The single in-flight assistant turn for a session, if any. Used by
/// the tail-subscription handler to decide whether to attach to a
/// running worker.
pub async fn in_flight_turn(pool: &Pool, session_id: &str) -> Result<Option<Turn>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ? AND status = 'in_progress'
           ORDER BY seq DESC
           LIMIT 1"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_turn).transpose()
}

/// Fetch one turn by id, scoped to its session. `None` if it doesn't
/// exist or belongs to another session. Used by retry/edit to look up
/// the target turn's `seq` and role before truncating.
pub async fn get_turn(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
) -> Result<Option<Turn>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ? AND id = ?"#,
    )
    .bind(session_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_turn).transpose()
}

/// Replace a user turn's text (the "edit" action). Scoped to the
/// session + `role = 'user'` so it can never rewrite an assistant turn.
/// Returns whether a row was updated.
pub async fn update_user_turn_content(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    content: &str,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        r#"UPDATE chat_turns SET user_content = ?
           WHERE id = ? AND session_id = ? AND role = 'user'"#,
    )
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn_id)
    .bind(session_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Delete every turn in the session from `from_seq` onwards (inclusive).
/// Backs retry/edit: drop the target turn and everything below it before
/// regenerating. Tool-call rows cascade via the FK. Returns the number
/// of turns removed.
pub async fn delete_turns_from_seq(
    pool: &Pool,
    session_id: &str,
    from_seq: i64,
) -> Result<u64, DbError> {
    let affected = sqlx::query("DELETE FROM chat_turns WHERE session_id = ? AND seq >= ?")
        .bind(session_id)
        .bind(from_seq)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}

/// Flip every still-`in_progress` assistant turn to `errored`. Used
/// at startup to evict orphans left behind by a crash / SIGKILL — no
/// worker is going to come back and finish them.
///
/// Returns the number of rows actually touched (useful for a one-shot
/// log line and for the startup test). Idempotent on a clean DB.
pub async fn sweep_in_progress_at_startup(pool: &Pool) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        r#"UPDATE chat_turns
           SET status = 'errored',
               error_message = COALESCE(error_message,
                                        'Stream interrupted — the server restarted before this response finished.'),
               completed_at = ?
           WHERE status = 'in_progress' AND role = 'assistant'"#,
    )
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected();
    // The turn sweep above doesn't touch `chat_tool_calls`. A row left at
    // 'running' renders as "Calling" forever even once its turn is errored —
    // the orphaned-tool-call bug. At startup no worker can come back to finish
    // any of them, so flip every still-running row to errored too.
    sqlx::query(
        r#"UPDATE chat_tool_calls
           SET status = 'errored',
               output_json = COALESCE(output_json,
                                      'Tool call interrupted — the server restarted before it finished.'),
               completed_at = ?
           WHERE status = 'running'"#,
    )
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(affected)
}

/// Mark in-progress assistant turns in `session_id` as errored,
/// except for `exempt_turn_id` (the one a live worker is still
/// driving). Called from the render path so a stale orphan from a
/// previous crash / `Busy`-path leak doesn't keep showing the
/// thinking spinner.
///
/// Returns the number of rows flipped.
pub async fn mark_orphaned_in_progress_as_errored(
    pool: &Pool,
    session_id: &str,
    exempt_turn_id: Option<&str>,
) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let message = "Stream interrupted — no worker is producing this response.";
    let affected = if let Some(exempt) = exempt_turn_id {
        sqlx::query(
            r#"UPDATE chat_turns
               SET status = 'errored',
                   error_message = COALESCE(error_message, ?),
                   completed_at = ?
               WHERE session_id = ?
                 AND status = 'in_progress'
                 AND role = 'assistant'
                 AND id != ?"#,
        )
        .bind(message)
        .bind(&now)
        .bind(session_id)
        .bind(exempt)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        sqlx::query(
            r#"UPDATE chat_turns
               SET status = 'errored',
                   error_message = COALESCE(error_message, ?),
                   completed_at = ?
               WHERE session_id = ?
                 AND status = 'in_progress'
                 AND role = 'assistant'"#,
        )
        .bind(message)
        .bind(&now)
        .bind(session_id)
        .execute(pool)
        .await?
        .rows_affected()
    };
    // Flip running tool-call rows belonging to the turns we just errored —
    // otherwise they render as "Calling" forever (same orphaned-tool-call bug
    // the startup sweep fixes, but scoped to this session). The live worker's
    // own turn (`exempt`) is preserved so a genuinely in-flight call keeps
    // showing "Calling".
    let tool_msg = "Tool call interrupted — no worker is producing this response.";
    if let Some(exempt) = exempt_turn_id {
        sqlx::query(
            r#"UPDATE chat_tool_calls
               SET status = 'errored',
                   output_json = COALESCE(output_json, ?),
                   completed_at = ?
               WHERE status = 'running'
                 AND turn_id IN (
                     SELECT id FROM chat_turns
                     WHERE session_id = ? AND role = 'assistant' AND id != ?
                 )"#,
        )
        .bind(tool_msg)
        .bind(&now)
        .bind(session_id)
        .bind(exempt)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE chat_tool_calls
               SET status = 'errored',
                   output_json = COALESCE(output_json, ?),
                   completed_at = ?
               WHERE status = 'running'
                 AND turn_id IN (
                     SELECT id FROM chat_turns
                     WHERE session_id = ? AND role = 'assistant'
                 )"#,
        )
        .bind(tool_msg)
        .bind(&now)
        .bind(session_id)
        .execute(pool)
        .await?;
    }
    Ok(affected)
}

// ---------------------------------------------------------------------------
// Tool calls

async fn next_tool_call_seq(pool: &Pool, turn_id: &str) -> Result<i64, DbError> {
    let row = sqlx::query(
        r#"SELECT COALESCE(MAX(seq), -1) + 1 AS next_seq
           FROM chat_tool_calls
           WHERE turn_id = ?"#,
    )
    .bind(turn_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("next_seq")?)
}

/// Insert a running tool call row. The model emits `id`,
/// `function.name`, and `function.arguments` (potentially across
/// multiple stream deltas) — the worker assembles those server-side
/// and inserts when ready to dispatch the tool.
pub async fn insert_running_tool_call(
    pool: &Pool,
    turn_id: &str,
    id: &str,
    name: &str,
    arguments_json: &str,
) -> Result<ToolCall, DbError> {
    let seq = next_tool_call_seq(pool, turn_id).await?;
    let now = Timestamp::now();
    let call = ToolCall {
        id: id.to_string(),
        turn_id: turn_id.to_string(),
        seq,
        name: name.to_string(),
        arguments_json: arguments_json.to_string(),
        output_json: None,
        status: ToolCallStatus::Running,
        created_at: now,
        completed_at: None,
    };
    sqlx::query(
        r#"INSERT INTO chat_tool_calls
              (id, turn_id, seq, name, arguments_json, status, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&call.id)
    .bind(&call.turn_id)
    .bind(call.seq)
    .bind(&call.name)
    .bind(&call.arguments_json)
    .bind(call.status.as_str())
    .bind(call.created_at.to_string())
    .execute(pool)
    .await?;
    Ok(call)
}

/// Max bytes of `output_json` we persist per tool call. `fetch_url`
/// can hand us a 4 MB text body; storing that verbatim in SQLite
/// per call bloats the DB fast and balloons the rows pulled back on
/// every chat-history list. The MODEL already saw the full payload
/// in the turn loop and synthesised its response off it — and the
/// openai_driver history replay rebuilds the upstream message list
/// from `chat_turns.content` (the assistant's prose), NOT from past
/// tool-call rows. So a cap here is invisible to upstream LLM
/// correctness; only the UI + audit log are affected, and both are
/// fine with a head + a "truncated" note.
///
/// 16 KB head matches the UI render cap (`TOOL_CALL_RENDER_CAP` in
/// `render.rs`); the symmetry means no further truncation is needed
/// downstream.
const PERSISTED_TOOL_OUTPUT_CAP: usize = 16 * 1024;

/// Cap `raw` to the persistence ceiling without splitting a UTF-8
/// codepoint. Strings under the cap pass through unchanged.
fn cap_tool_output(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.len() <= PERSISTED_TOOL_OUTPUT_CAP {
        return std::borrow::Cow::Borrowed(raw);
    }
    let head_end = raw
        .char_indices()
        .take_while(|(i, _)| *i <= PERSISTED_TOOL_OUTPUT_CAP)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(PERSISTED_TOOL_OUTPUT_CAP);
    let mut out = String::with_capacity(head_end + 128);
    out.push_str(&raw[..head_end]);
    out.push_str(&format!(
        "\n\n…\n(truncated by gateway at persist time: original {} bytes, \
         displayed first {} chars; the model saw the full payload before this \
         row was written)\n",
        raw.len(),
        head_end,
    ));
    std::borrow::Cow::Owned(out)
}

/// Stamp a tool call's output + flip status. Idempotent on the same
/// (id, output) pair: worker calls it exactly once per tool result.
pub async fn complete_tool_call(
    pool: &Pool,
    id: &str,
    output_json: &str,
    status: ToolCallStatus,
) -> Result<(), DbError> {
    if status == ToolCallStatus::Running {
        return Err(DbError::Decode {
            column: "status",
            source: anyhow::anyhow!("complete_tool_call called with status=running"),
        });
    }
    let capped = cap_tool_output(output_json);
    sqlx::query(
        r#"UPDATE chat_tool_calls
           SET output_json = ?, status = ?, completed_at = ?
           WHERE id = ?"#,
    )
    .bind(capped.as_ref())
    .bind(status.as_str())
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous};
    use std::str::FromStr;

    #[test]
    fn cap_tool_output_passes_small_payloads_through() {
        let small = "ok";
        assert!(matches!(
            cap_tool_output(small),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn cap_tool_output_truncates_oversized_payloads_with_footer() {
        let huge = "x".repeat(PERSISTED_TOOL_OUTPUT_CAP * 4);
        let out = cap_tool_output(&huge);
        assert!(matches!(out, std::borrow::Cow::Owned(_)));
        assert!(out.len() < huge.len() / 2);
        assert!(out.contains("truncated by gateway at persist time"));
        assert!(out.contains(&huge.len().to_string()));
    }

    #[test]
    fn cap_tool_output_doesnt_split_utf8() {
        // Emoji right at the cap so a naive byte slice would corrupt.
        let prefix = "x".repeat(PERSISTED_TOOL_OUTPUT_CAP - 1);
        let payload = format!("{prefix}\u{1F600}\u{1F600}");
        let out = cap_tool_output(&payload);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    /// In-memory SQLite + the chat_* schema set up inline.
    ///
    /// session-core deliberately doesn't own migrations (the bins do
    /// — see the module-level doc comment), so for tests we recreate
    /// just enough schema here: a stub `users` table (because
    /// `chat_sessions.user_id` foreign-keys into it) plus the three
    /// tables this module actually manages. Kept in lock-step with
    /// `crates/gateway/migrations/0005_chat_persistence.sql`; if that
    /// file changes shape, mirror the change here.
    async fn pool() -> Pool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .synchronous(SqliteSynchronous::Off)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        for stmt in [
            r#"CREATE TABLE users (
                id          TEXT PRIMARY KEY NOT NULL,
                email       TEXT NOT NULL,
                name        TEXT,
                roles_json  TEXT NOT NULL DEFAULT '[]',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            )"#,
            r#"CREATE TABLE chat_sessions (
                id          TEXT PRIMARY KEY NOT NULL,
                user_id     TEXT NOT NULL,
                title       TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                shared      INTEGER NOT NULL DEFAULT 0,
                pinned      INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            )"#,
            r#"CREATE TABLE chat_turns (
                id                    TEXT PRIMARY KEY NOT NULL,
                session_id            TEXT NOT NULL,
                seq                   INTEGER NOT NULL,
                role                  TEXT NOT NULL,
                user_content          TEXT,
                model                 TEXT,
                content               TEXT,
                reasoning             TEXT,
                reasoning_elapsed_ms  INTEGER,
                reasoning_started_at  TEXT,
                status                TEXT NOT NULL,
                error_message         TEXT,
                created_at            TEXT NOT NULL,
                completed_at          TEXT,
                FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE,
                UNIQUE (session_id, seq)
            )"#,
            r#"CREATE TABLE chat_tool_calls (
                id              TEXT PRIMARY KEY NOT NULL,
                turn_id         TEXT NOT NULL,
                seq             INTEGER NOT NULL,
                name            TEXT NOT NULL,
                arguments_json  TEXT NOT NULL,
                output_json     TEXT,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                completed_at    TEXT,
                FOREIGN KEY (turn_id) REFERENCES chat_turns(id) ON DELETE CASCADE,
                UNIQUE (turn_id, seq)
            )"#,
            // FTS5 table for search (matches migration 0031). Keyed on the
            // implicit integer `rowid` because `chat_turns.id` is a TEXT UUID
            // and FTS5's content_rowid must be an integer.
            r#"CREATE VIRTUAL TABLE chat_turns_fts USING fts5(
                user_content,
                content,
                content='chat_turns',
                tokenize='unicode61'
            )"#,
            // Terminal-status-gated triggers (see migration 0031 for the
            // full rationale): a row is indexed IFF status != 'in_progress',
            // so streaming `append_content` updates do no FTS work and each
            // turn is tokenized exactly once at finalize.
            r#"CREATE TRIGGER chat_turns_fts_ai AFTER INSERT ON chat_turns
               WHEN new.status != 'in_progress'
               BEGIN
                INSERT INTO chat_turns_fts(rowid, user_content, content)
                VALUES (new.rowid, new.user_content, new.content);
            END"#,
            r#"CREATE TRIGGER chat_turns_fts_ad AFTER DELETE ON chat_turns
               WHEN old.status != 'in_progress'
               BEGIN
                INSERT INTO chat_turns_fts(chat_turns_fts, rowid, user_content, content)
                VALUES ('delete', old.rowid, old.user_content, old.content);
            END"#,
            r#"CREATE TRIGGER chat_turns_fts_au AFTER UPDATE OF user_content, content, status ON chat_turns
               WHEN old.status != 'in_progress' OR new.status != 'in_progress'
               BEGIN
                INSERT INTO chat_turns_fts(chat_turns_fts, rowid, user_content, content)
                    SELECT 'delete', old.rowid, old.user_content, old.content
                    WHERE old.status != 'in_progress';
                INSERT INTO chat_turns_fts(rowid, user_content, content)
                    SELECT new.rowid, new.user_content, new.content
                    WHERE new.status != 'in_progress';
            END"#,
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        sqlx::query(
            r#"INSERT INTO users (id, email, name, roles_json, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', 'U1', '[]', ?, ?)"#,
        )
        .bind(Timestamp::now().to_string())
        .bind(Timestamp::now().to_string())
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn delete_turns_from_seq_truncates_inclusive() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "t0", "hi").await.unwrap();
        create_assistant_turn_in_progress(&pool, &s.id, "t1", "m")
            .await
            .unwrap();
        create_user_turn(&pool, &s.id, "t2", "again").await.unwrap();
        // Drop seq>=1 → keeps only the first user turn.
        let removed = delete_turns_from_seq(&pool, &s.id, 1).await.unwrap();
        assert_eq!(removed, 2);
        let turns = list_turns(&pool, &s.id).await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn.id, "t0");
    }

    #[tokio::test]
    async fn update_user_turn_content_only_touches_user_rows() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "u", "old").await.unwrap();
        create_assistant_turn_in_progress(&pool, &s.id, "a", "m")
            .await
            .unwrap();
        assert!(
            update_user_turn_content(&pool, &s.id, "u", "new")
                .await
                .unwrap()
        );
        // An assistant turn can't be rewritten through this path.
        assert!(
            !update_user_turn_content(&pool, &s.id, "a", "x")
                .await
                .unwrap()
        );
        let t = get_turn(&pool, &s.id, "u").await.unwrap().unwrap();
        assert_eq!(t.user_content.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn get_turn_is_scoped_to_session() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "u", "hi").await.unwrap();
        assert!(
            get_turn(&pool, "other-session", "u")
                .await
                .unwrap()
                .is_none()
        );
        assert!(get_turn(&pool, &s.id, "u").await.unwrap().is_some());
    }

    // Owner is the seeded user `u1`; the "viewer" `u2` is only ever a WHERE
    // param in the read paths (never inserted), so no users row is needed for
    // it — the FK only bites on the create_session INSERT.
    #[tokio::test]
    async fn get_session_readable_owner_shared_and_denied() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // Owner always reads.
        assert!(
            get_session_readable(&pool, "u1", &s.id)
                .await
                .unwrap()
                .is_some()
        );
        // A different signed-in user cannot read a private session.
        assert!(
            get_session_readable(&pool, "u2", &s.id)
                .await
                .unwrap()
                .is_none()
        );
        // After sharing, the other user can read it (and sees shared=true).
        assert!(set_shared(&pool, "u1", &s.id, true).await.unwrap());
        let seen = get_session_readable(&pool, "u2", &s.id).await.unwrap();
        assert!(seen.is_some_and(|s| s.shared));
        // Unsharing revokes the other user again.
        assert!(set_shared(&pool, "u1", &s.id, false).await.unwrap());
        assert!(
            get_session_readable(&pool, "u2", &s.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn set_shared_is_owner_only() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // A non-owner's attempt updates nothing and reports false.
        assert!(!set_shared(&pool, "u2", &s.id, true).await.unwrap());
        assert!(
            !get_session(&pool, "u1", &s.id)
                .await
                .unwrap()
                .unwrap()
                .shared
        );
        // The owner can set it.
        assert!(set_shared(&pool, "u1", &s.id, true).await.unwrap());
        assert!(
            get_session(&pool, "u1", &s.id)
                .await
                .unwrap()
                .unwrap()
                .shared
        );
    }

    #[tokio::test]
    async fn set_pinned_is_owner_only() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // A freshly created session is unpinned.
        assert!(!s.pinned);
        // A non-owner's attempt updates nothing and reports false.
        assert!(!set_pinned(&pool, "u2", &s.id, true).await.unwrap());
        assert!(
            !get_session(&pool, "u1", &s.id)
                .await
                .unwrap()
                .unwrap()
                .pinned
        );
        // The owner can set it, and clear it again.
        assert!(set_pinned(&pool, "u1", &s.id, true).await.unwrap());
        assert!(
            get_session(&pool, "u1", &s.id)
                .await
                .unwrap()
                .unwrap()
                .pinned
        );
        assert!(set_pinned(&pool, "u1", &s.id, false).await.unwrap());
        assert!(
            !get_session(&pool, "u1", &s.id)
                .await
                .unwrap()
                .unwrap()
                .pinned
        );
    }

    #[tokio::test]
    async fn list_sessions_floats_pinned_above_recency() {
        let pool = pool().await;
        let old = create_session(&pool, "u1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let recent = create_session(&pool, "u1").await.unwrap();

        // By default `recent` sorts first (most-recent).
        let listed = list_sessions(&pool, "u1").await.unwrap();
        assert_eq!(listed[0].id, recent.id);
        assert_eq!(listed[1].id, old.id);

        // Pinning the older one floats it to the top despite being staler.
        assert!(set_pinned(&pool, "u1", &old.id, true).await.unwrap());
        let listed = list_sessions(&pool, "u1").await.unwrap();
        assert_eq!(listed[0].id, old.id, "pinned must come first");
        assert!(listed[0].pinned);
        assert_eq!(listed[1].id, recent.id);
        assert!(!listed[1].pinned);
    }

    #[tokio::test]
    async fn turn_session_readable_tracks_share_flag() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "turn-1", "hi")
            .await
            .unwrap();
        // Owner: always readable; other: only once shared.
        assert!(turn_session_readable(&pool, "turn-1", "u1").await.unwrap());
        assert!(!turn_session_readable(&pool, "turn-1", "u2").await.unwrap());
        set_shared(&pool, "u1", &s.id, true).await.unwrap();
        assert!(turn_session_readable(&pool, "turn-1", "u2").await.unwrap());
        // Unknown turn id → false (no leak).
        assert!(
            !turn_session_readable(&pool, "no-such-turn", "u1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn create_and_list_session() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        assert_eq!(s.user_id, "u1");
        assert!(s.title.is_none());

        let listed = list_sessions(&pool, "u1").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, s.id);
    }

    #[tokio::test]
    async fn get_session_scoped_to_user() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // Insert a second user and confirm they can't see u1's session.
        sqlx::query(
            r#"INSERT INTO users (id, email, name, roles_json, created_at, updated_at)
               VALUES ('u2', 'u2@example.com', 'U2', '[]', ?, ?)"#,
        )
        .bind(Timestamp::now().to_string())
        .bind(Timestamp::now().to_string())
        .execute(&pool)
        .await
        .unwrap();
        assert!(get_session(&pool, "u2", &s.id).await.unwrap().is_none());
        assert!(get_session(&pool, "u1", &s.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_sessions_orders_most_recent_first() {
        let pool = pool().await;
        let a = create_session(&pool, "u1").await.unwrap();
        let b = create_session(&pool, "u1").await.unwrap();
        // Touch a *after* b so it floats above.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        touch_session(&pool, &a.id).await.unwrap();

        let listed = list_sessions(&pool, "u1").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, a.id);
        assert_eq!(listed[1].id, b.id);
    }

    #[tokio::test]
    async fn latest_session_returns_none_then_most_recent() {
        let pool = pool().await;
        assert!(latest_session(&pool, "u1").await.unwrap().is_none());
        let _a = create_session(&pool, "u1").await.unwrap();
        let b = create_session(&pool, "u1").await.unwrap();
        assert_eq!(latest_session(&pool, "u1").await.unwrap().unwrap().id, b.id);
    }

    #[tokio::test]
    async fn delete_session_cascades_to_turns_and_tool_calls() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let user_turn = create_user_turn(&pool, &s.id, "u-hi", "hi").await.unwrap();
        let asst = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &asst.id, "tc1", "echo", "{}")
            .await
            .unwrap();

        assert!(delete_session(&pool, "u1", &s.id).await.unwrap());

        let turns = list_turns(&pool, &s.id).await.unwrap();
        assert!(turns.is_empty());
        // The user-turn row should also be gone (cascading FK).
        let row = sqlx::query("SELECT id FROM chat_turns WHERE id = ?")
            .bind(&user_turn.id)
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn delete_session_returns_false_when_already_gone() {
        let pool = pool().await;
        assert!(!delete_session(&pool, "u1", "missing").await.unwrap());
    }

    #[tokio::test]
    async fn turn_seq_increments_per_session() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t1 = create_user_turn(&pool, &s.id, "u-first", "first")
            .await
            .unwrap();
        let t2 = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        let t3 = create_user_turn(&pool, &s.id, "u-third", "third")
            .await
            .unwrap();
        assert_eq!((t1.seq, t2.seq, t3.seq), (0, 1, 2));
    }

    #[tokio::test]
    async fn append_content_and_reasoning_accumulate() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();

        append_reasoning(&pool, &t.id, "let me think… ")
            .await
            .unwrap();
        append_reasoning(&pool, &t.id, "okay.").await.unwrap();
        set_reasoning_elapsed(&pool, &t.id, 2200).await.unwrap();
        append_content(&pool, &t.id, "Hel").await.unwrap();
        append_content(&pool, &t.id, "lo!").await.unwrap();

        let turns = list_turns(&pool, &s.id).await.unwrap();
        assert_eq!(turns.len(), 1);
        let got = &turns[0].turn;
        assert_eq!(got.content.as_deref(), Some("Hello!"));
        assert_eq!(got.reasoning.as_deref(), Some("let me think… okay."));
        assert_eq!(got.reasoning_elapsed_ms, Some(2200));
        assert_eq!(got.status, TurnStatus::InProgress);
    }

    #[tokio::test]
    async fn finalize_turn_flips_status_and_stamps_completed_at() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        finalize_turn(&pool, &t.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        let turns = list_turns(&pool, &s.id).await.unwrap();
        let got = &turns[0].turn;
        assert_eq!(got.status, TurnStatus::Completed);
        assert!(got.completed_at.is_some());
        assert!(got.error_message.is_none());
    }

    #[tokio::test]
    async fn finalize_turn_with_error_status_records_message() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        finalize_turn(&pool, &t.id, TurnStatus::Errored, Some("upstream 502"))
            .await
            .unwrap();
        let got = &list_turns(&pool, &s.id).await.unwrap()[0].turn;
        assert_eq!(got.status, TurnStatus::Errored);
        assert_eq!(got.error_message.as_deref(), Some("upstream 502"));
    }

    #[tokio::test]
    async fn finalize_turn_rejects_in_progress_status() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        let err = finalize_turn(&pool, &t.id, TurnStatus::InProgress, None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DbError::Decode {
                column: "status",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn in_flight_turn_picks_the_open_assistant_row() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let _user = create_user_turn(&pool, &s.id, "u-hi", "hi").await.unwrap();
        // No assistant yet → no in-flight.
        assert!(in_flight_turn(&pool, &s.id).await.unwrap().is_none());

        let asst = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        let found = in_flight_turn(&pool, &s.id).await.unwrap().unwrap();
        assert_eq!(found.id, asst.id);

        finalize_turn(&pool, &asst.id, TurnStatus::Completed, None)
            .await
            .unwrap();
        assert!(in_flight_turn(&pool, &s.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tool_calls_round_trip() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "call_1", "echo", r#"{"msg":"hi"}"#)
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "call_2", "now", r#"{}"#)
            .await
            .unwrap();
        complete_tool_call(&pool, "call_1", r#"{"ok":true}"#, ToolCallStatus::Completed)
            .await
            .unwrap();

        let turns = list_turns(&pool, &s.id).await.unwrap();
        assert_eq!(turns.len(), 1);
        let calls = &turns[0].tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].status, ToolCallStatus::Completed);
        assert_eq!(calls[0].output_json.as_deref(), Some(r#"{"ok":true}"#));
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].status, ToolCallStatus::Running);
    }

    #[tokio::test]
    async fn list_turns_includes_tool_calls_in_seq_order() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "a", "tool_a", "{}")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "b", "tool_b", "{}")
            .await
            .unwrap();
        let turns = list_turns(&pool, &s.id).await.unwrap();
        let seqs: Vec<i64> = turns[0].tool_calls.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[tokio::test]
    async fn sweep_in_progress_at_startup_flips_assistants_to_errored() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // One in-flight assistant + one completed assistant + one user.
        let live = create_assistant_turn_in_progress(&pool, &s.id, "asst-live", "m")
            .await
            .unwrap();
        let done = create_assistant_turn_in_progress(&pool, &s.id, "asst-done", "m")
            .await
            .unwrap();
        finalize_turn(&pool, &done.id, TurnStatus::Completed, None)
            .await
            .unwrap();
        let _ = create_user_turn(&pool, &s.id, "u-hi", "hi").await.unwrap();

        let affected = sweep_in_progress_at_startup(&pool).await.unwrap();
        assert_eq!(affected, 1);

        let after: Vec<_> = list_turns(&pool, &s.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.turn.id, t.turn.status))
            .collect();
        let live_now = after.iter().find(|(id, _)| id == &live.id).unwrap();
        assert_eq!(live_now.1, TurnStatus::Errored);
        let done_now = after.iter().find(|(id, _)| id == &done.id).unwrap();
        assert_eq!(done_now.1, TurnStatus::Completed); // untouched
    }

    #[tokio::test]
    async fn sweep_indexes_crashed_in_progress_turns_for_search() {
        // A turn left in_progress by a crash is not indexed (streaming path
        // skips indexing). The startup sweep flips it in_progress → errored,
        // and that transition must index its partial content so a crashed
        // reply is still findable.
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "t0", "m")
            .await
            .unwrap();
        append_content(&pool, &a.id, "crashedneedle partial reply")
            .await
            .unwrap();

        // Not searchable while in_progress.
        assert_eq!(
            search_sessions(&pool, "u1", "crashedneedle", 10)
                .await
                .unwrap()
                .len(),
            0
        );

        sweep_in_progress_at_startup(&pool).await.unwrap();

        // Now searchable (swept to errored → indexed).
        assert_eq!(
            search_sessions(&pool, "u1", "crashedneedle", 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn mark_orphaned_in_progress_skips_the_exempt_turn() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let live = create_assistant_turn_in_progress(&pool, &s.id, "asst-live", "m")
            .await
            .unwrap();
        let orphan = create_assistant_turn_in_progress(&pool, &s.id, "asst-orphan", "m")
            .await
            .unwrap();

        let affected = mark_orphaned_in_progress_as_errored(&pool, &s.id, Some(&live.id))
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let after: Vec<_> = list_turns(&pool, &s.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.turn.id, t.turn.status))
            .collect();
        let live_now = after.iter().find(|(id, _)| id == &live.id).unwrap();
        assert_eq!(live_now.1, TurnStatus::InProgress); // exempt — left alone
        let orphan_now = after.iter().find(|(id, _)| id == &orphan.id).unwrap();
        assert_eq!(orphan_now.1, TurnStatus::Errored);
    }

    #[tokio::test]
    async fn mark_orphaned_in_progress_with_no_exempt_flips_all_in_session() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "asst-a", "m")
            .await
            .unwrap();
        let b = create_assistant_turn_in_progress(&pool, &s.id, "asst-b", "m")
            .await
            .unwrap();

        let affected = mark_orphaned_in_progress_as_errored(&pool, &s.id, None)
            .await
            .unwrap();
        assert_eq!(affected, 2);

        for turn_id in [a.id.as_str(), b.id.as_str()] {
            let row: String = sqlx::query_scalar("SELECT status FROM chat_turns WHERE id = ?")
                .bind(turn_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(row, "errored");
        }
    }

    #[tokio::test]
    async fn sweep_in_progress_at_startup_also_errors_running_tool_calls() {
        // The orphaned-tool-call fix: a row left 'running' when the server
        // restarts must be swept too, or it renders "Calling" forever.
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t = create_assistant_turn_in_progress(&pool, &s.id, "asst-x", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "running_call", "tool_a", "{}")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t.id, "done_call", "tool_b", "{}")
            .await
            .unwrap();
        complete_tool_call(
            &pool,
            "done_call",
            r#"{"ok":true}"#,
            ToolCallStatus::Completed,
        )
        .await
        .unwrap();

        sweep_in_progress_at_startup(&pool).await.unwrap();

        let calls = list_turns(&pool, &s.id).await.unwrap()[0]
            .tool_calls
            .clone();
        let running = calls.iter().find(|c| c.id == "running_call").unwrap();
        assert_eq!(running.status, ToolCallStatus::Errored);
        assert!(
            running
                .output_json
                .as_deref()
                .unwrap_or_default()
                .contains("interrupted")
        );
        // An already-settled call is left exactly as it was.
        let done = calls.iter().find(|c| c.id == "done_call").unwrap();
        assert_eq!(done.status, ToolCallStatus::Completed);
        assert_eq!(done.output_json.as_deref(), Some(r#"{"ok":true}"#));
    }

    #[tokio::test]
    async fn mark_orphaned_in_progress_preserves_the_live_turns_tool_calls() {
        // The per-render sweep must error orphaned tool calls but never the
        // live worker's own in-flight call (its turn is exempt).
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let live = create_assistant_turn_in_progress(&pool, &s.id, "asst-live", "m")
            .await
            .unwrap();
        let orphan = create_assistant_turn_in_progress(&pool, &s.id, "asst-orphan", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &live.id, "live_call", "tool_a", "{}")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &orphan.id, "orphan_call", "tool_b", "{}")
            .await
            .unwrap();

        mark_orphaned_in_progress_as_errored(&pool, &s.id, Some(&live.id))
            .await
            .unwrap();

        let live_status: String =
            sqlx::query_scalar("SELECT status FROM chat_tool_calls WHERE id = ?")
                .bind("live_call")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(live_status, "running"); // exempt — still legitimately "Calling"
        let orphan_status: String =
            sqlx::query_scalar("SELECT status FROM chat_tool_calls WHERE id = ?")
                .bind("orphan_call")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphan_status, "errored");
    }

    async fn seed_user(pool: &Pool, id: &str) {
        sqlx::query(
            r#"INSERT INTO users (id, email, name, roles_json, created_at, updated_at)
               VALUES (?, ?, 'U', '[]', ?, ?)"#,
        )
        .bind(id)
        .bind(format!("{id}@example.com"))
        .bind(Timestamp::now().to_string())
        .bind(Timestamp::now().to_string())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fork_session_copies_turns_into_new_owner_unshared() {
        let pool = pool().await;
        seed_user(&pool, "u2").await;
        let src = create_session(&pool, "u1").await.unwrap();
        set_session_title(&pool, &src.id, "Plans").await.unwrap();
        set_shared(&pool, "u1", &src.id, true).await.unwrap();
        create_user_turn(&pool, &src.id, "t0", "hello")
            .await
            .unwrap();
        let a = create_assistant_turn_in_progress(&pool, &src.id, "t1", "gpt")
            .await
            .unwrap();
        append_content(&pool, &a.id, "hi there").await.unwrap();
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        let src = get_session(&pool, "u1", &src.id).await.unwrap().unwrap();
        let (fork, copies) = fork_session(&pool, &src, "u2").await.unwrap();

        // New owner, private, title carried over, distinct id.
        assert_eq!(fork.user_id, "u2");
        assert!(!fork.shared);
        assert_eq!(fork.title.as_deref(), Some("Plans"));
        assert_ne!(fork.id, src.id);
        assert!(copies.is_empty(), "no attachments in this conversation");

        // Turns copied 1-to-1 with fresh ids, same order + payload.
        let orig = list_turns(&pool, &src.id).await.unwrap();
        let copy = list_turns(&pool, &fork.id).await.unwrap();
        assert_eq!(copy.len(), orig.len());
        assert_eq!(copy[0].turn.user_content.as_deref(), Some("hello"));
        assert_eq!(copy[1].turn.content.as_deref(), Some("hi there"));
        assert_eq!(copy[1].turn.status, TurnStatus::Completed);
        for (o, c) in orig.iter().zip(&copy) {
            assert_ne!(o.turn.id, c.turn.id, "turn ids must be fresh");
            assert_eq!(c.turn.session_id, fork.id);
        }
        // The original is untouched.
        assert_eq!(orig.len(), 2);
    }

    #[tokio::test]
    async fn fork_session_remaps_attachment_markers_and_lists_copies() {
        let pool = pool().await;
        seed_user(&pool, "u2").await;
        let src = create_session(&pool, "u1").await.unwrap();
        // A user turn whose marker URL points at its own turn id "t0".
        let marker =
            crate::attachments::marker_line("c.png", "image/png", "/chat/attachment/t0/c.png", 9);
        create_user_turn(&pool, &src.id, "t0", &format!("see\n{marker}"))
            .await
            .unwrap();

        let src = get_session(&pool, "u1", &src.id).await.unwrap().unwrap();
        let (fork, copies) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy = list_turns(&pool, &fork.id).await.unwrap();
        let new_turn_id = &copy[0].turn.id;

        // Marker URL now points at the NEW turn id, not the original t0.
        let body = copy[0].turn.user_content.clone().unwrap();
        assert!(
            body.contains(&format!("/chat/attachment/{new_turn_id}/c.png")),
            "marker not remapped: {body}"
        );
        assert!(!body.contains("/chat/attachment/t0/"), "stale url: {body}");

        // And the blob-copy descriptor maps t0 → the new turn.
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].from_turn_id, "t0");
        assert_eq!(&copies[0].to_turn_id, new_turn_id);
        assert_eq!(copies[0].filename, "c.png");
    }

    #[tokio::test]
    async fn fork_session_copies_in_progress_turn_as_errored() {
        let pool = pool().await;
        seed_user(&pool, "u2").await;
        let src = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &src.id, "t0", "go").await.unwrap();
        // Left in_progress — a chat forked mid-stream.
        create_assistant_turn_in_progress(&pool, &src.id, "t1", "gpt")
            .await
            .unwrap();

        let src = get_session(&pool, "u1", &src.id).await.unwrap().unwrap();
        let (fork, _) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy = list_turns(&pool, &fork.id).await.unwrap();
        // The copied assistant turn is errored (never a hung spinner).
        assert_eq!(copy[1].turn.status, TurnStatus::Errored);
        assert!(copy[1].turn.completed_at.is_some());
    }

    #[tokio::test]
    async fn fork_session_remints_tool_call_ids() {
        // `chat_tool_calls.id` is a global PRIMARY KEY (the model's
        // tool_call_id). Forking a conversation that made tool calls must
        // mint fresh ids — reusing the source ids collides with the originals
        // (and with any prior fork of the same shared chat), which used to
        // surface as a 500 on POST /chat/{id}/fork.
        let pool = pool().await;
        seed_user(&pool, "u2").await;
        let src = create_session(&pool, "u1").await.unwrap();
        let a = create_assistant_turn_in_progress(&pool, &src.id, "t0", "gpt")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &a.id, "call_1", "web_search", "{}")
            .await
            .unwrap();
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        let src = get_session(&pool, "u1", &src.id).await.unwrap().unwrap();

        // First fork succeeds and the copied tool call has a fresh id.
        let (fork1, _) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy1 = list_turns(&pool, &fork1.id).await.unwrap();
        assert_eq!(copy1[0].tool_calls.len(), 1);
        assert_ne!(
            copy1[0].tool_calls[0].id, "call_1",
            "tool-call id must be re-minted, not reused"
        );
        assert_eq!(copy1[0].tool_calls[0].name, "web_search");

        // Forking the same shared chat again must not collide on the PK.
        let (fork2, _) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy2 = list_turns(&pool, &fork2.id).await.unwrap();
        assert_ne!(copy1[0].tool_calls[0].id, copy2[0].tool_calls[0].id);
    }

    // -----------------------------------------------------------------------
    // search_sessions tests

    #[tokio::test]
    async fn search_sessions_finds_content_match() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "t0", "ceph osd timeout")
            .await
            .unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "t1", "gpt")
            .await
            .unwrap();
        append_content(&pool, &a.id, "yes, that config")
            .await
            .unwrap();
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "ceph osd", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, s.id);
        assert!(hits[0].snippet.contains("<b>ceph</b>") || hits[0].snippet.contains("<b>osd</b>"));
    }

    #[tokio::test]
    async fn search_sessions_finds_title_match() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        // The title carries the term; the turn text does NOT — so this only
        // passes if titles are searched (the FTS index covers only turns).
        set_session_title(&pool, &s.id, "E2E smoke run")
            .await
            .unwrap();
        create_user_turn(&pool, &s.id, "t0", "unrelated body text")
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "E2E", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "title-only match must surface");
        assert_eq!(hits[0].session_id, s.id);
        assert_eq!(hits[0].title.as_deref(), Some("E2E smoke run"));
    }

    #[tokio::test]
    async fn search_sessions_title_match_ranks_first() {
        let pool = pool().await;
        // A conversation matching only on content.
        let content = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &content.id, "c0", "widget deployment notes")
            .await
            .unwrap();
        // A conversation matching on title.
        let titled = create_session(&pool, "u1").await.unwrap();
        set_session_title(&pool, &titled.id, "widget plan")
            .await
            .unwrap();
        create_user_turn(&pool, &titled.id, "t0", "nothing relevant here")
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "widget", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, titled.id, "title hit ranks first");
        assert_eq!(hits[1].session_id, content.id);
    }

    #[tokio::test]
    async fn search_sessions_title_and_content_dedupes_keeping_snippet() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        set_session_title(&pool, &s.id, "ceph tuning")
            .await
            .unwrap();
        create_user_turn(&pool, &s.id, "t0", "ceph osd config")
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "ceph", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "one row when title and content both match");
        assert_eq!(hits[0].session_id, s.id);
        assert!(
            hits[0].snippet.contains("<b>ceph</b>"),
            "content snippet grafted onto the deduped title hit"
        );
    }

    #[tokio::test]
    async fn search_sessions_excludes_reasoning() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "t0", "hello").await.unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "t1", "gpt")
            .await
            .unwrap();
        append_content(&pool, &a.id, "response").await.unwrap();
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        // Put "unique_term" only in reasoning, not in content.
        sqlx::query(
            r#"UPDATE chat_turns SET reasoning = 'thinking about unique_term' WHERE id = 't1'"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let hits = search_sessions(&pool, "u1", "unique_term", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 0, "reasoning should not be indexed");
    }

    #[tokio::test]
    async fn search_sessions_finds_attachment_filename() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();

        // Attachment marker inline in user_content.
        let marker = crate::attachments::marker_line(
            "config.yaml",
            "text/yaml",
            "/chat/attachment/t0/config.yaml",
            123,
        );
        create_user_turn(&pool, &s.id, "t0", &format!("here is the file\n{marker}"))
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "config.yaml", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "attachment filenames are indexed");
        assert_eq!(hits[0].session_id, s.id);
    }

    #[tokio::test]
    async fn search_sessions_respects_user_scoping() {
        let pool = pool().await;
        seed_user(&pool, "u2").await;
        let _s1 = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &_s1.id, "t0", "secret")
            .await
            .unwrap();

        let _s2 = create_session(&pool, "u2").await.unwrap();
        create_user_turn(&pool, &_s2.id, "t1", "secret")
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "secret", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, _s1.id, "must not return u2's session");
    }

    #[tokio::test]
    async fn search_sessions_empty_query_returns_normal_list() {
        let pool = pool().await;
        let _s1 = create_session(&pool, "u1").await.unwrap();
        let _s2 = create_session(&pool, "u1").await.unwrap();

        let hits = search_sessions(&pool, "u1", "", 10).await.unwrap();
        assert_eq!(hits.len(), 2, "empty query lists all sessions");

        let hits = search_sessions(&pool, "u1", "   ", 10).await.unwrap();
        assert_eq!(hits.len(), 2, "whitespace-only query lists all sessions");
    }

    #[tokio::test]
    async fn search_sessions_deduplicates_by_session() {
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "t0", "term").await.unwrap();
        create_user_turn(&pool, &s.id, "t1", "term again")
            .await
            .unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "t2", "gpt")
            .await
            .unwrap();
        append_content(&pool, &a.id, "term response").await.unwrap();
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();

        let hits = search_sessions(&pool, "u1", "term", 10).await.unwrap();
        assert_eq!(
            hits.len(),
            1,
            "multiple matching turns → one hit per session"
        );
        assert_eq!(hits[0].session_id, s.id);
    }

    #[test]
    fn to_fts_match_query_quotes_tokens_and_neutralises_punctuation() {
        // Plain words → quoted, AND-joined.
        assert_eq!(to_fts_match_query("ceph osd"), "\"ceph\" \"osd\"");
        // Punctuation that FTS5 would treat as an operator is defused.
        assert_eq!(to_fts_match_query("config.yaml"), "\"config.yaml\"");
        assert_eq!(to_fts_match_query("--verbose"), "\"--verbose\"");
        // Embedded quotes are escaped by doubling.
        assert_eq!(to_fts_match_query("a\"b"), "\"a\"\"b\"");
        // Blank / whitespace-only → empty (caller falls back to the list).
        assert_eq!(to_fts_match_query(""), "");
        assert_eq!(to_fts_match_query("   "), "");
    }

    #[tokio::test]
    async fn search_sessions_handles_punctuation_query_without_error() {
        // Regression: a filename query used to throw "fts5: syntax error".
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &s.id, "t0", "look at --flag and osd:timeout")
            .await
            .unwrap();

        // These must not error, even though they contain FTS5 operators.
        for q in ["--flag", "osd:timeout", "a.b.c", "(", "AND OR"] {
            let hits = search_sessions(&pool, "u1", q, 10).await;
            assert!(hits.is_ok(), "query {q:?} should not error: {hits:?}");
        }
    }

    #[test]
    fn highlight_snippet_escapes_html_and_keeps_only_our_markup() {
        // Sentinels become <b>; surrounding attacker text is escaped.
        let raw = format!("a {SNIPPET_OPEN}b{SNIPPET_CLOSE} <script>x</script> &\"'");
        let out = highlight_snippet(&raw);
        assert_eq!(
            out,
            "a <b>b</b> &lt;script&gt;x&lt;/script&gt; &amp;&quot;&#39;"
        );
        assert!(!out.contains("<script>"), "raw <script> must be escaped");
    }

    #[tokio::test]
    async fn search_sessions_snippet_escapes_stored_html() {
        // A conversation containing an XSS payload must come back escaped —
        // no live <script>/<img> tag in the rendered snippet.
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        create_user_turn(
            &pool,
            &s.id,
            "t0",
            "danger <img src=x onerror=alert(1)> danger",
        )
        .await
        .unwrap();

        let hits = search_sessions(&pool, "u1", "danger", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        let snip = &hits[0].snippet;
        assert!(!snip.contains("<img"), "raw <img> leaked: {snip}");
        assert!(snip.contains("&lt;img"), "payload not escaped: {snip}");
        // The match highlight we control is still real markup.
        assert!(snip.contains("<b>danger</b>"), "highlight missing: {snip}");
    }

    #[tokio::test]
    async fn search_sessions_limit_counts_conversations_not_turns() {
        // One chatty conversation with many matching turns must not crowd a
        // second matching conversation out of a limit-1 result set — the
        // limit counts conversations, so we still see both when limit >= 2.
        let pool = pool().await;
        let chatty = create_session(&pool, "u1").await.unwrap();
        for i in 0..5 {
            create_user_turn(&pool, &chatty.id, &format!("c{i}"), "needle here")
                .await
                .unwrap();
        }
        let other = create_session(&pool, "u1").await.unwrap();
        create_user_turn(&pool, &other.id, "o0", "needle there")
            .await
            .unwrap();

        // limit 1 → exactly one conversation (not one of five chatty turns).
        let hits = search_sessions(&pool, "u1", "needle", 1).await.unwrap();
        assert_eq!(hits.len(), 1, "limit counts conversations");

        // limit 2 → both distinct conversations surface.
        let hits = search_sessions(&pool, "u1", "needle", 2).await.unwrap();
        assert_eq!(hits.len(), 2, "both conversations, deduped to one row each");
        let ids: std::collections::HashSet<_> = hits.iter().map(|h| &h.session_id).collect();
        assert!(ids.contains(&chatty.id) && ids.contains(&other.id));
    }

    #[tokio::test]
    async fn search_sessions_blank_query_respects_limit() {
        // A blank / whitespace-only query falls back to the session list, but
        // must still honour `limit` so a big account can't flood the sidebar.
        let pool = pool().await;
        for i in 0..5 {
            create_session(&pool, "u1").await.unwrap();
            let _ = i;
        }
        let hits = search_sessions(&pool, "u1", "   ", 3).await.unwrap();
        assert_eq!(hits.len(), 3, "blank-query fallback must cap at limit");
    }

    #[test]
    fn strip_fts_sentinels_removes_only_the_control_chars() {
        // Borrows unchanged when clean (common case).
        assert!(matches!(
            strip_fts_sentinels("plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Strips STX/ETX, keeps everything else (incl. tabs/newlines).
        let dirty = format!("a{SNIPPET_OPEN}b{SNIPPET_CLOSE}c\td\ne");
        assert_eq!(strip_fts_sentinels(&dirty).as_ref(), "abc\td\ne");
    }

    #[tokio::test]
    async fn search_snippet_cannot_be_forged_with_typed_sentinels() {
        // A user pasting raw STX/ETX must NOT be able to forge highlight
        // markup: the sentinels are stripped at write time, so the only <b>
        // in a snippet is the one we inject around the real match.
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let payload = format!("hello {SNIPPET_OPEN}forged{SNIPPET_CLOSE} needle");
        create_user_turn(&pool, &s.id, "t0", &payload)
            .await
            .unwrap();

        // The stored text has no sentinels left.
        let turn = get_turn(&pool, &s.id, "t0").await.unwrap().unwrap();
        let stored = turn.user_content.unwrap();
        assert!(
            !stored.contains(SNIPPET_OPEN) && !stored.contains(SNIPPET_CLOSE),
            "sentinels must be stripped at write: {stored:?}"
        );

        // Searching highlights only the real match, not the forged span.
        let hits = search_sessions(&pool, "u1", "needle", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        let snip = &hits[0].snippet;
        assert!(
            snip.contains("<b>needle</b>"),
            "real match highlighted: {snip}"
        );
        assert!(
            snip.matches("<b>").count() == snip.matches("</b>").count(),
            "balanced highlight markup only: {snip}"
        );
        assert!(
            !snip.contains("forged</b>"),
            "forged span must not exist: {snip}"
        );
    }

    #[tokio::test]
    async fn in_progress_turn_is_not_indexed_until_finalized() {
        // The FTS invariant: an in_progress assistant turn is NOT searchable;
        // it becomes searchable exactly when finalize_turn flips its status.
        // (This is what keeps streaming append_content off the FTS hot path.)
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let a = create_assistant_turn_in_progress(&pool, &s.id, "t0", "gpt")
            .await
            .unwrap();
        append_content(&pool, &a.id, "distinctword reply")
            .await
            .unwrap();

        // Mid-stream: content exists on the row but is not yet in the index.
        let hits = search_sessions(&pool, "u1", "distinctword", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 0, "in_progress turn must not be searchable");

        // Finalize → now indexed and searchable.
        finalize_turn(&pool, &a.id, TurnStatus::Completed, None)
            .await
            .unwrap();
        let hits = search_sessions(&pool, "u1", "distinctword", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "finalized turn must be searchable");
        assert_eq!(hits[0].session_id, s.id);
    }
}
