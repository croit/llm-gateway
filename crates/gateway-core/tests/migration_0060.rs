// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Migration 0060 rebuilds `usage_daily` to key on the API token, and
//! backfills the token attribution from whatever `usage_events` still holds.
//!
//! A rebuild-and-copy migration is the kind that loses data silently — the
//! new table is created, some rows do not make it across, and nothing fails.
//! These tests run the real migration set up to 0059, seed a database that
//! looks like a live one, then apply 0060 and check what survived.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Executor, Row, SqlitePool};

/// Every migration strictly before 0060, in order — a database as it looked
/// on the previous release.
async fn db_before_0060() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    for m in sqlx::migrate!("./migrations").iter() {
        if m.version >= 60 {
            continue;
        }
        pool.execute(sqlx::raw_sql(&m.sql))
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", m.version));
    }
    pool
}

/// Apply 0060 itself.
async fn apply_0060(pool: &SqlitePool) {
    let m = sqlx::migrate!("./migrations")
        .iter()
        .find(|m| m.version == 60)
        .expect("migration 0060 exists")
        .clone();
    pool.execute(sqlx::raw_sql(&m.sql))
        .await
        .expect("0060 applies");
}

async fn seed_event(pool: &SqlitePool, day: &str, user: &str, token: Option<&str>, total: i64) {
    sqlx::query(
        "INSERT INTO usage_events
           (id, created_at, user_id, user_email, token_id, token_name, source, kind,
            backend, model, status, duration_ms, prompt_tokens, completion_tokens,
            total_tokens, cost, enforce_limits, input_units, output_units)
         VALUES (?, ?, ?, ?, ?, ?, 'v1_api', 'chat', 'b', 'qwen', 200, 5, 0, ?, ?, 0, 1, 0, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(format!("{day}T12:00:00Z"))
    .bind(user)
    .bind(format!("{user}@example.com"))
    .bind(token)
    .bind(token.map(|t| format!("{t}-name")))
    .bind(total)
    .bind(total)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_rollup(pool: &SqlitePool, day: &str, user: &str, reqs: i64, total: i64) {
    sqlx::query(
        "INSERT INTO usage_daily
           (day, user_id, user_email, source, kind, backend, model,
            req_count, error_count, prompt_tokens, completion_tokens, total_tokens, cost)
         VALUES (?, ?, ?, 'v1_api', 'chat', 'b', 'qwen', ?, 0, 0, ?, ?, 0)",
    )
    .bind(day)
    .bind(user)
    .bind(format!("{user}@example.com"))
    .bind(reqs)
    .bind(total)
    .bind(total)
    .execute(pool)
    .await
    .unwrap();
}

async fn rows(pool: &SqlitePool) -> Vec<(String, String, i64, i64)> {
    sqlx::query(
        "SELECT day, token_id, req_count, total_tokens FROM usage_daily ORDER BY day, token_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get::<String, _>("day"),
            r.get::<String, _>("token_id"),
            r.get::<i64, _>("req_count"),
            r.get::<i64, _>("total_tokens"),
        )
    })
    .collect()
}

/// The days the raw table still covers get real token attribution — that is
/// the whole reason the migration is worth running now rather than later.
#[tokio::test]
async fn it_backfills_token_attribution_from_the_retained_raw_events() {
    let pool = db_before_0060().await;
    // Two days of raw events. The oldest is deliberately excluded from the
    // rebuild (see below), so put the interesting data on the newer one.
    seed_event(&pool, "2026-01-01", "alice", Some("tok-a"), 1).await;
    seed_rollup(&pool, "2026-01-01", "alice", 1, 1).await;

    seed_event(&pool, "2026-01-02", "alice", Some("tok-a"), 10).await;
    seed_event(&pool, "2026-01-02", "alice", Some("tok-a"), 10).await;
    seed_event(&pool, "2026-01-02", "alice", Some("tok-b"), 5).await;
    seed_event(&pool, "2026-01-02", "alice", None, 7).await;
    seed_rollup(&pool, "2026-01-02", "alice", 4, 32).await;

    apply_0060(&pool).await;

    let got = rows(&pool).await;
    // 2026-01-02 is rebuilt per token; the two tok-a calls collapse to one row.
    assert!(
        got.contains(&("2026-01-02".into(), "tok-a".into(), 2, 20)),
        "{got:?}"
    );
    assert!(
        got.contains(&("2026-01-02".into(), "tok-b".into(), 1, 5)),
        "{got:?}"
    );
    assert!(
        got.contains(&("2026-01-02".into(), String::new(), 1, 7)),
        "token-less traffic keeps its own row: {got:?}"
    );
    // Rebuilt totals still add up to what the old rollup said for that day.
    let day2: i64 = got
        .iter()
        .filter(|(d, ..)| d == "2026-01-02")
        .map(|(_, _, r, _)| r)
        .sum();
    assert_eq!(day2, 4, "no requests lost in the rebuild: {got:?}");
}

/// Pruning cuts at an instant, not a day boundary, so the oldest retained day
/// is a partial record. Rebuilding from it would undercount, so it keeps its
/// original rollup — unattributed, but correct.
#[tokio::test]
async fn it_leaves_the_oldest_partially_retained_day_alone() {
    let pool = db_before_0060().await;
    // The rollup says 100 requests that day; only one event survived pruning.
    seed_rollup(&pool, "2026-01-01", "alice", 100, 1000).await;
    seed_event(&pool, "2026-01-01", "alice", Some("tok-a"), 1).await;
    seed_event(&pool, "2026-01-02", "alice", Some("tok-a"), 5).await;
    seed_rollup(&pool, "2026-01-02", "alice", 1, 5).await;

    apply_0060(&pool).await;

    let got = rows(&pool).await;
    assert!(
        got.contains(&("2026-01-01".into(), String::new(), 100, 1000)),
        "the partial day must keep its full rollup, not the one surviving \
         event: {got:?}"
    );
}

/// History with nothing left in the raw table still has to survive the
/// rebuild — as unattributed rows, since the attribution is genuinely gone.
#[tokio::test]
async fn it_carries_over_history_with_no_raw_events_left() {
    let pool = db_before_0060().await;
    seed_rollup(&pool, "2025-06-01", "alice", 42, 4200).await;
    seed_rollup(&pool, "2025-06-02", "bob", 7, 700).await;

    apply_0060(&pool).await;

    let got = rows(&pool).await;
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|(_, tok, ..)| tok.is_empty()), "{got:?}");
    assert!(got.contains(&("2025-06-01".into(), String::new(), 42, 4200)));
}

/// A fresh install has neither table populated. The backfill's `MIN()`
/// subquery is NULL there, and every comparison against it is NULL — the
/// statements have to be no-ops rather than errors.
#[tokio::test]
async fn it_applies_cleanly_to_an_empty_database() {
    let pool = db_before_0060().await;
    apply_0060(&pool).await;
    assert!(rows(&pool).await.is_empty());
}

/// After the rebuild the upsert key must include the token, or the writer's
/// `ON CONFLICT (day, user_id, token_id, …)` clause has nothing to bind to.
#[tokio::test]
async fn the_rebuilt_table_is_keyed_on_the_token() {
    let pool = db_before_0060().await;
    apply_0060(&pool).await;
    let pk: Vec<String> =
        sqlx::query("SELECT name FROM pragma_table_info('usage_daily') WHERE pk > 0 ORDER BY pk")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
    assert!(
        pk.contains(&"token_id".to_string()),
        "primary key is {pk:?}"
    );
}
