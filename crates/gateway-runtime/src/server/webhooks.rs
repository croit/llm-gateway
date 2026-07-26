// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Inbound webhooks: per-user prompts that run when an external caller POSTs
//! to a secret trigger URL. The event-driven twin of [`scheduled`] — instead
//! of a cron tick, an inbound request to `/hooks/{secret}` fires the run.
//!
//! This module owns the persistence (`webhooks` table + CRUD, created by
//! migration `0037_webhooks.sql`). The trigger endpoint and the management UI
//! live in `rama_server::pages::webhooks`; the actual model run is driven by
//! the shared [`crate::server::headless`] helper (the same engine scheduled
//! actions and `/chat` use).
//!
//! The trigger secret is a `gwh_<64 hex>` string minted by
//! [`gateway_core::server::auth::token::mint_webhook`]; only its SHA-256 hash is
//! stored here, so the plaintext URL exists only in the owner's hands.
//!
//! [`scheduled`]: crate::server::scheduled

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use gateway_core::server::db::{DbError, Pool};

/// A persisted webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prompt: String,
    pub model: String,
    /// `true` = the fire runs with the owner's normal tools; `false` (the
    /// default) = no tools, since an anonymous caller triggers the run.
    pub tools_enabled: bool,
    /// `true` = the caller waits and gets the model output in the HTTP
    /// response; `false` = fire-and-forget (respond 202, run in background).
    pub synchronous: bool,
    /// SHA-256 hex of the `gwh_` trigger secret. The plaintext is shown to the
    /// owner only on create/rotate.
    pub secret_hash: String,
    /// `false` = paused; the trigger 404s and never fires.
    pub enabled: bool,
    pub last_fired_at: Option<Timestamp>,
    /// `"ok"` or `"error"` — the outcome of the most recent fire.
    pub last_status: Option<String>,
    /// Chat session opened by the most recent fire, for the "open" link.
    pub last_session_id: Option<String>,
    pub last_error: Option<String>,
    /// Raw body of the most recent fire, kept so the owner can rerun it with a
    /// different prompt (see [`set_last_payload`] and the rerun handlers).
    /// `None` until the webhook has fired at least once.
    pub last_payload: Option<String>,
    /// `true` = each live fire appends into the previous fire's chat (so the
    /// model sees prior fires as history); `false` (default) = each fire opens
    /// a fresh chat. Reruns are always fresh regardless.
    pub reuse_conversation: bool,
    /// When reusing, how many recent rounds (one round = prompt + reply = 2
    /// turns) of history to replay — caps unbounded growth.
    pub reuse_rounds: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The validated, ready-to-insert fields for a new webhook. The caller mints
/// the secret and passes only its hash.
pub struct NewWebhook {
    pub user_id: String,
    pub name: String,
    pub prompt: String,
    pub model: String,
    pub tools_enabled: bool,
    pub synchronous: bool,
    pub reuse_conversation: bool,
    pub reuse_rounds: i64,
    pub secret_hash: String,
}

/// The mutable fields of an existing webhook, as submitted by the edit form.
/// The secret is rotated separately via [`rotate_secret`].
pub struct EditWebhook {
    pub name: String,
    pub prompt: String,
    pub model: String,
    pub tools_enabled: bool,
    pub synchronous: bool,
    pub reuse_conversation: bool,
    pub reuse_rounds: i64,
}

fn parse_ts(s: String, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn parse_opt_ts(s: Option<String>, column: &'static str) -> Result<Option<Timestamp>, DbError> {
    s.map(|s| parse_ts(s, column)).transpose()
}

fn map_row(row: &SqliteRow) -> Result<Webhook, DbError> {
    Ok(Webhook {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        name: row.try_get("name")?,
        prompt: row.try_get("prompt")?,
        model: row.try_get("model")?,
        tools_enabled: row.try_get::<i64, _>("tools_enabled")? != 0,
        synchronous: row.try_get::<i64, _>("synchronous")? != 0,
        secret_hash: row.try_get("secret_hash")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        last_fired_at: parse_opt_ts(row.try_get("last_fired_at")?, "last_fired_at")?,
        last_status: row.try_get("last_status")?,
        last_session_id: row.try_get("last_session_id")?,
        last_error: row.try_get("last_error")?,
        last_payload: row.try_get("last_payload")?,
        reuse_conversation: row.try_get::<i64, _>("reuse_conversation")? != 0,
        reuse_rounds: row.try_get("reuse_rounds")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
    })
}

const COLS: &str = "id, user_id, name, prompt, model, tools_enabled, synchronous, secret_hash, \
     enabled, last_fired_at, last_status, last_session_id, last_error, last_payload, \
     reuse_conversation, reuse_rounds, created_at, updated_at";

/// Insert a new webhook. Returns the stored row.
pub async fn create(pool: &Pool, new: NewWebhook) -> Result<Webhook, DbError> {
    let now = Timestamp::now();
    let row = Webhook {
        id: Uuid::new_v4().to_string(),
        user_id: new.user_id,
        name: new.name,
        prompt: new.prompt,
        model: new.model,
        tools_enabled: new.tools_enabled,
        synchronous: new.synchronous,
        secret_hash: new.secret_hash,
        enabled: true,
        last_fired_at: None,
        last_status: None,
        last_session_id: None,
        last_error: None,
        last_payload: None,
        reuse_conversation: new.reuse_conversation,
        reuse_rounds: new.reuse_rounds,
        created_at: now,
        updated_at: now,
    };
    sqlx::query(
        r#"INSERT INTO webhooks
              (id, user_id, name, prompt, model, tools_enabled, synchronous, secret_hash,
               enabled, reuse_conversation, reuse_rounds, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)"#,
    )
    .bind(&row.id)
    .bind(&row.user_id)
    .bind(&row.name)
    .bind(&row.prompt)
    .bind(&row.model)
    .bind(row.tools_enabled as i64)
    .bind(row.synchronous as i64)
    .bind(&row.secret_hash)
    .bind(row.reuse_conversation as i64)
    .bind(row.reuse_rounds)
    .bind(row.created_at.to_string())
    .bind(row.updated_at.to_string())
    .execute(pool)
    .await?;
    Ok(row)
}

/// All of a user's webhooks, newest first.
pub async fn list_for_user(pool: &Pool, user_id: &str) -> Result<Vec<Webhook>, DbError> {
    let sql =
        format!("SELECT {COLS} FROM webhooks WHERE user_id = ? ORDER BY created_at DESC, id ASC");
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// One webhook, scoped to its owner (so a user can't read another's).
pub async fn get(pool: &Pool, user_id: &str, id: &str) -> Result<Option<Webhook>, DbError> {
    let sql = format!("SELECT {COLS} FROM webhooks WHERE id = ? AND user_id = ?");
    let row = sqlx::query(&sql)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_row).transpose()
}

/// Apply an edit, scoped to the owner. Returns `true` if a row matched.
pub async fn update(
    pool: &Pool,
    user_id: &str,
    id: &str,
    edit: EditWebhook,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        r#"UPDATE webhooks
           SET name = ?, prompt = ?, model = ?, tools_enabled = ?, synchronous = ?,
               reuse_conversation = ?, reuse_rounds = ?, updated_at = ?
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(&edit.name)
    .bind(&edit.prompt)
    .bind(&edit.model)
    .bind(edit.tools_enabled as i64)
    .bind(edit.synchronous as i64)
    .bind(edit.reuse_conversation as i64)
    .bind(edit.reuse_rounds)
    .bind(Timestamp::now().to_string())
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Pause or resume a webhook, scoped to the owner. A paused webhook's trigger
/// 404s. Returns `true` if a row matched.
pub async fn set_enabled(
    pool: &Pool,
    user_id: &str,
    id: &str,
    enabled: bool,
) -> Result<bool, DbError> {
    let affected =
        sqlx::query("UPDATE webhooks SET enabled = ?, updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(enabled as i64)
            .bind(Timestamp::now().to_string())
            .bind(id)
            .bind(user_id)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(affected > 0)
}

/// Swap in a fresh secret hash, scoped to the owner. The old trigger URL stops
/// working immediately. Returns `true` if a row matched.
pub async fn rotate_secret(
    pool: &Pool,
    user_id: &str,
    id: &str,
    new_hash: &str,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        "UPDATE webhooks SET secret_hash = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(new_hash)
    .bind(Timestamp::now().to_string())
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Delete a webhook, scoped to the owner. Returns `true` if a row matched.
pub async fn delete(pool: &Pool, user_id: &str, id: &str) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM webhooks WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// The webhook a trigger secret's hash belongs to — but only when it's
/// enabled. The trigger's hot path; a paused (or missing) webhook returns
/// `None`, which the handler turns into a 404. Not owner-scoped: the secret
/// itself is the credential.
pub async fn find_active_by_secret_hash(
    pool: &Pool,
    secret_hash: &str,
) -> Result<Option<Webhook>, DbError> {
    let sql = format!("SELECT {COLS} FROM webhooks WHERE secret_hash = ? AND enabled = 1");
    let row = sqlx::query(&sql)
        .bind(secret_hash)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_row).transpose()
}

/// Record the outcome of a fire. `status` is `"ok"` or `"error"`;
/// `session_id` is the chat the fire opened (kept even on error so the owner
/// can inspect the partial conversation).
pub async fn mark_fired(
    pool: &Pool,
    id: &str,
    status: &str,
    session_id: Option<&str>,
    error: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE webhooks
           SET last_fired_at = ?, last_status = ?, last_session_id = ?, last_error = ?,
               updated_at = ?
           WHERE id = ?"#,
    )
    .bind(Timestamp::now().to_string())
    .bind(status)
    .bind(session_id)
    .bind(error)
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stash the raw body of a fire so the owner can later rerun it with a
/// different prompt. Called at fire time (before the run), so the payload is
/// retained even if the run itself errors. Not owner-scoped — the trigger has
/// already resolved the webhook by its secret.
pub async fn set_last_payload(pool: &Pool, id: &str, payload: &str) -> Result<(), DbError> {
    sqlx::query("UPDATE webhooks SET last_payload = ?, updated_at = ? WHERE id = ?")
        .bind(payload)
        .bind(Timestamp::now().to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Run history (webhook_runs, migration 0038)

/// One recorded fire of a webhook — the run-history unit. `prompt` + `payload`
/// are captured per run so any past run is fully reproducible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRun {
    pub id: String,
    pub webhook_id: String,
    pub fired_at: Timestamp,
    /// `None` while the run is in flight (async fires respond before finishing);
    /// `"ok"` or `"error"` once complete.
    pub status: Option<String>,
    /// Chat session this run opened.
    pub session_id: Option<String>,
    pub error: Option<String>,
    pub prompt: String,
    pub payload: String,
    /// `"fire"` (external trigger) or `"rerun"` (manual replay).
    pub source: String,
    pub created_at: Timestamp,
}

const RUN_COLS: &str = "id, webhook_id, fired_at, status, session_id, error, prompt, payload, \
     source, created_at";

fn map_run(row: &SqliteRow) -> Result<WebhookRun, DbError> {
    Ok(WebhookRun {
        id: row.try_get("id")?,
        webhook_id: row.try_get("webhook_id")?,
        fired_at: parse_ts(row.try_get("fired_at")?, "fired_at")?,
        status: row.try_get("status")?,
        session_id: row.try_get("session_id")?,
        error: row.try_get("error")?,
        prompt: row.try_get("prompt")?,
        payload: row.try_get("payload")?,
        source: row.try_get("source")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
    })
}

/// Record the start of a run (status left NULL until [`finish_run`]). Returns
/// the new run id. Not owner-scoped — the caller has already resolved the
/// webhook (by secret for a fire, by owner for a rerun).
pub async fn record_run_start(
    pool: &Pool,
    webhook_id: &str,
    session_id: &str,
    prompt: &str,
    payload: &str,
    source: &str,
) -> Result<String, DbError> {
    let id = Uuid::new_v4().to_string();
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO webhook_runs
              (id, webhook_id, fired_at, session_id, prompt, payload, source, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&id)
    .bind(webhook_id)
    .bind(&now)
    .bind(session_id)
    .bind(prompt)
    .bind(payload)
    .bind(source)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Record the outcome of a run started by [`record_run_start`].
pub async fn finish_run(
    pool: &Pool,
    run_id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query("UPDATE webhook_runs SET status = ?, error = ? WHERE id = ?")
        .bind(status)
        .bind(error)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A webhook's most recent runs, newest first, capped at `limit`.
pub async fn list_runs(
    pool: &Pool,
    webhook_id: &str,
    limit: i64,
) -> Result<Vec<WebhookRun>, DbError> {
    // Newest-first. The tiebreaker is `rowid DESC` (SQLite's monotonic
    // insertion order), NOT `id` — `id` is a random UUID, so two runs sharing a
    // `fired_at` tick would otherwise come back in nondeterministic order.
    let sql = format!(
        "SELECT {RUN_COLS} FROM webhook_runs WHERE webhook_id = ? \
         ORDER BY fired_at DESC, rowid DESC LIMIT ?"
    );
    let rows = sqlx::query(&sql)
        .bind(webhook_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_run).collect()
}

/// One run, scoped to its webhook (the caller has verified webhook ownership).
pub async fn get_run(
    pool: &Pool,
    webhook_id: &str,
    run_id: &str,
) -> Result<Option<WebhookRun>, DbError> {
    let sql = format!("SELECT {RUN_COLS} FROM webhook_runs WHERE webhook_id = ? AND id = ?");
    let row = sqlx::query(&sql)
        .bind(webhook_id)
        .bind(run_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_run).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_db() -> Pool {
        // `open` runs the migration set, which includes 0037 (the webhooks table).
        gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    async fn seed_user(pool: &Pool, id: &str) {
        // The FK to users(id) means we need a row to attach webhooks to.
        let now = Timestamp::now();
        gateway_core::server::db::users::upsert(
            pool,
            &gateway_core::server::db::users::User {
                id: id.to_string(),
                email: format!("{id}@example.com"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: Some("Europe/Berlin".to_string()),
            },
        )
        .await
        .unwrap();
    }

    fn sample(user_id: &str, secret_hash: &str) -> NewWebhook {
        NewWebhook {
            user_id: user_id.to_string(),
            name: "Deploy digest".to_string(),
            prompt: "Summarize the incoming payload.".to_string(),
            model: "qwen".to_string(),
            tools_enabled: false,
            synchronous: true,
            reuse_conversation: false,
            reuse_rounds: 5,
            secret_hash: secret_hash.to_string(),
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let created = create(&pool, sample("u1", "hash-aaa")).await.unwrap();
        let got = get(&pool, "u1", &created.id).await.unwrap().unwrap();
        assert_eq!(got, created);
        assert!(got.enabled && got.synchronous && !got.tools_enabled);
        assert_eq!(got.secret_hash, "hash-aaa");
    }

    #[tokio::test]
    async fn get_and_delete_are_scoped_to_owner() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        seed_user(&pool, "u2").await;
        let created = create(&pool, sample("u1", "hash-bbb")).await.unwrap();
        assert!(get(&pool, "u2", &created.id).await.unwrap().is_none());
        assert!(!delete(&pool, "u2", &created.id).await.unwrap());
        assert!(get(&pool, "u1", &created.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn find_active_by_secret_hash_only_hits_enabled() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let created = create(&pool, sample("u1", "hash-ccc")).await.unwrap();
        assert_eq!(
            find_active_by_secret_hash(&pool, "hash-ccc")
                .await
                .unwrap()
                .map(|w| w.id),
            Some(created.id.clone())
        );
        // Pausing takes it out of the trigger's reach.
        set_enabled(&pool, "u1", &created.id, false).await.unwrap();
        assert!(
            find_active_by_secret_hash(&pool, "hash-ccc")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_active_by_secret_hash(&pool, "hash-nope")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rotate_secret_swaps_the_hash() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let created = create(&pool, sample("u1", "hash-old")).await.unwrap();
        assert!(
            rotate_secret(&pool, "u1", &created.id, "hash-new")
                .await
                .unwrap()
        );
        // Old hash no longer resolves; new one does.
        assert!(
            find_active_by_secret_hash(&pool, "hash-old")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_active_by_secret_hash(&pool, "hash-new")
                .await
                .unwrap()
                .is_some()
        );
        // Rotation is owner-scoped.
        seed_user(&pool, "u2").await;
        assert!(
            !rotate_secret(&pool, "u2", &created.id, "hash-hijack")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn mark_fired_records_status_and_session() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let created = create(&pool, sample("u1", "hash-ddd")).await.unwrap();
        mark_fired(&pool, &created.id, "ok", Some("sess-1"), None)
            .await
            .unwrap();
        let got = get(&pool, "u1", &created.id).await.unwrap().unwrap();
        assert_eq!(got.last_status.as_deref(), Some("ok"));
        assert_eq!(got.last_session_id.as_deref(), Some("sess-1"));
        assert!(got.last_fired_at.is_some());
        assert!(got.last_error.is_none());
    }

    #[tokio::test]
    async fn set_last_payload_round_trips_for_rerun() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let created = create(&pool, sample("u1", "hash-eee")).await.unwrap();
        assert!(
            created.last_payload.is_none(),
            "no payload until first fire"
        );
        set_last_payload(&pool, &created.id, r#"{"event":"deploy"}"#)
            .await
            .unwrap();
        let got = get(&pool, "u1", &created.id).await.unwrap().unwrap();
        assert_eq!(got.last_payload.as_deref(), Some(r#"{"event":"deploy"}"#));
    }

    #[tokio::test]
    async fn run_history_records_lists_and_reproduces() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let hook = create(&pool, sample("u1", "hash-run")).await.unwrap();

        let r1 = record_run_start(
            &pool,
            &hook.id,
            "sess-1",
            "prompt one",
            r#"{"a":1}"#,
            "fire",
        )
        .await
        .unwrap();
        finish_run(&pool, &r1, "ok", None).await.unwrap();
        let r2 = record_run_start(
            &pool,
            &hook.id,
            "sess-2",
            "prompt two",
            r#"{"b":2}"#,
            "rerun",
        )
        .await
        .unwrap();
        finish_run(&pool, &r2, "error", Some("boom")).await.unwrap();

        // Newest first.
        let runs = list_runs(&pool, &hook.id, 50).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, r2);
        assert_eq!(runs[0].source, "rerun");
        assert_eq!(runs[0].status.as_deref(), Some("error"));
        assert_eq!(runs[0].error.as_deref(), Some("boom"));
        assert_eq!(runs[0].session_id.as_deref(), Some("sess-2"));
        assert_eq!(runs[1].id, r1);
        assert_eq!(runs[1].status.as_deref(), Some("ok"));

        // A run carries its own prompt + payload so it can be replayed exactly.
        let got = get_run(&pool, &hook.id, &r1).await.unwrap().unwrap();
        assert_eq!(got.prompt, "prompt one");
        assert_eq!(got.payload, r#"{"a":1}"#);

        // Scoped to the webhook.
        assert!(get_run(&pool, "other-hook", &r1).await.unwrap().is_none());

        // The limit caps the list.
        assert_eq!(list_runs(&pool, &hook.id, 1).await.unwrap().len(), 1);
    }
}
