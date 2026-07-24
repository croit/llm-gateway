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

mod fork;
mod search;
mod sessions;
mod tool_calls;
mod turns;

pub use fork::*;
pub use search::*;
pub use sessions::*;
pub use tool_calls::*;
pub use turns::*;

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
                id              TEXT NOT NULL,
                turn_id         TEXT NOT NULL,
                seq             INTEGER NOT NULL,
                name            TEXT NOT NULL,
                arguments_json  TEXT NOT NULL,
                output_json     TEXT,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                completed_at    TEXT,
                PRIMARY KEY (turn_id, id),
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
        complete_tool_call(
            &pool,
            &t.id,
            "call_1",
            r#"{"ok":true}"#,
            ToolCallStatus::Completed,
        )
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
    async fn tool_call_ids_may_repeat_across_turns() {
        // Regression: tool-call identity is (turn_id, id), so a backend that
        // recycles `call_0` every request (qwen / vLLM) can use it again in a
        // later turn — or another session — without the insert aborting on a
        // `UNIQUE constraint failed`. Two turns, same id, both persist; and
        // completing one keys on its own turn, never touching the other.
        let pool = pool().await;
        let s = create_session(&pool, "u1").await.unwrap();
        let t1 = create_assistant_turn_in_progress(&pool, &s.id, "asst-1", "m")
            .await
            .unwrap();
        insert_running_tool_call(&pool, &t1.id, "call_0", "echo", "{}")
            .await
            .unwrap();
        let t2 = create_assistant_turn_in_progress(&pool, &s.id, "asst-2", "m")
            .await
            .unwrap();
        // Same id, different turn — must not collide.
        insert_running_tool_call(&pool, &t2.id, "call_0", "echo", "{}")
            .await
            .unwrap();

        complete_tool_call(
            &pool,
            &t2.id,
            "call_0",
            r#"{"ok":true}"#,
            ToolCallStatus::Completed,
        )
        .await
        .unwrap();

        let turns = list_turns(&pool, &s.id).await.unwrap();
        let by_id =
            |tid: &str| turns.iter().find(|t| t.turn.id == tid).unwrap().tool_calls[0].clone();
        // Only turn 2's call flipped to completed; turn 1's stays running.
        assert_eq!(by_id(&t1.id).status, ToolCallStatus::Running);
        assert_eq!(by_id(&t2.id).status, ToolCallStatus::Completed);
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
            &t.id,
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
    async fn fork_session_preserves_tool_call_ids_scoped_to_turn() {
        // Tool-call identity is (turn_id, id). A fork lands under fresh turn
        // ids, so the source tool-call id is copied verbatim without colliding
        // with the original — or with any prior fork of the same shared chat.
        // (This used to require re-minting a UUID, back when `id` alone was a
        // global primary key and reusing it surfaced as a 500 on fork.)
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

        // The copied tool call keeps its id, scoped under the fork's new turn.
        let (fork1, _) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy1 = list_turns(&pool, &fork1.id).await.unwrap();
        assert_eq!(copy1[0].tool_calls.len(), 1);
        assert_eq!(copy1[0].tool_calls[0].id, "call_1");
        assert_eq!(copy1[0].tool_calls[0].name, "web_search");

        // Forking the same shared chat again lands under yet another turn id,
        // so the identical tool-call id does not collide on the PK.
        let (fork2, _) = fork_session(&pool, &src, "u2").await.unwrap();
        let copy2 = list_turns(&pool, &fork2.id).await.unwrap();
        assert_eq!(copy2[0].tool_calls[0].id, "call_1");
        assert_ne!(
            copy1[0].turn.id, copy2[0].turn.id,
            "each fork gets a distinct turn id, which is what makes the shared \
             tool-call id safe"
        );
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
