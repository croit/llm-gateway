// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Usage accounting: persistence + aggregation for per-user / per-backend
//! request metrics.
//!
//! Two tables (see `migrations/0022_usage.sql`), written together by the
//! batched writer in `server::usage`:
//!
//!   * `usage_events` — one raw row per upstream backend call, kept for a
//!     configurable recent window. Every period the UI offers (today …
//!     last month) falls inside that window, so they're answered from here
//!     with time-zone-correct boundaries and full detail.
//!   * `usage_daily` — daily (UTC-day) rollups kept forever, accumulated in
//!     place via UPSERT. The read source for ranges older than the raw
//!     window.
//!
//! Timestamps are stored second-precision RFC 3339 UTC
//! (`%Y-%m-%dT%H:%M:%SZ`) — the same convention `scheduled` uses — so a
//! `>= ? AND < ?` range compares lexically without the fractional-seconds
//! sort hazard (`...00.5Z` would sort *before* `...00Z`).

use std::collections::HashMap;

use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::{DbError, Pool};

/// How an event entered the gateway. The "source = access method" half of
/// the source dimension (the other half is the API token, carried on the
/// record for `/v1` calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// The OpenAI-compatible `/v1` proxy (bearer token).
    V1Api,
    /// The built-in chat UI (session cookie).
    Chat,
    /// A cron-fired scheduled action (headless).
    Scheduled,
}

impl UsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageSource::V1Api => "v1_api",
            UsageSource::Chat => "chat",
            UsageSource::Scheduled => "scheduled",
        }
    }
}

/// What kind of upstream the call hit — mirrors `upstreams::PoolKind`, but
/// kept local so the metrics layer doesn't couple to the registry's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    Chat,
    Embedding,
    Transcription,
}

impl UsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageKind::Chat => "chat",
            UsageKind::Embedding => "embedding",
            UsageKind::Transcription => "transcription",
        }
    }
}

/// One measured upstream call, emitted by a hot path onto the usage channel
/// (`server::usage`) and later flushed by the batched writer. Cheap to
/// build and `Send` so it can ride the channel.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub created_at: Timestamp,
    pub user_id: String,
    pub user_email: Option<String>,
    pub token_id: Option<String>,
    pub token_name: Option<String>,
    pub source: UsageSource,
    pub kind: UsageKind,
    pub backend: String,
    pub model: String,
    pub status: u16,
    pub duration_ms: i64,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

/// Pull the OpenAI `usage` token counts out of a completion body or a
/// trailing SSE `usage` frame. Missing fields come back `None` (e.g.
/// transcription responses carry no `usage`; embeddings omit
/// `completion_tokens`).
pub fn usage_from_value(v: &Value) -> (Option<i64>, Option<i64>, Option<i64>) {
    let usage = v.get("usage");
    let get = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_i64);
    (
        get("prompt_tokens"),
        get("completion_tokens"),
        get("total_tokens"),
    )
}

fn fmt_ts(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn fmt_day(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%d").to_string()
}

/// Insert a batch of records in a single transaction: one raw
/// `usage_events` row each, plus an UPSERT that accumulates the matching
/// `usage_daily` rollup. Called by the writer task; collapses thousands of
/// requests into a handful of transactions per second.
pub async fn insert_batch(pool: &Pool, recs: &[UsageRecord]) -> Result<(), DbError> {
    if recs.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for r in recs {
        let created = fmt_ts(r.created_at);
        let day = fmt_day(r.created_at);
        sqlx::query(
            "INSERT INTO usage_events
               (id, created_at, user_id, user_email, token_id, token_name,
                source, kind, backend, model, status, duration_ms,
                prompt_tokens, completion_tokens, total_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&created)
        .bind(&r.user_id)
        .bind(r.user_email.as_deref())
        .bind(r.token_id.as_deref())
        .bind(r.token_name.as_deref())
        .bind(r.source.as_str())
        .bind(r.kind.as_str())
        .bind(&r.backend)
        .bind(&r.model)
        .bind(i64::from(r.status))
        .bind(r.duration_ms)
        .bind(r.prompt_tokens)
        .bind(r.completion_tokens)
        .bind(r.total_tokens)
        .execute(&mut *tx)
        .await?;

        let is_error = i64::from(r.status >= 400);
        sqlx::query(
            "INSERT INTO usage_daily
               (day, user_id, user_email, source, kind, backend, model,
                req_count, error_count, prompt_tokens, completion_tokens, total_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)
             ON CONFLICT(day, user_id, source, kind, backend, model) DO UPDATE SET
                req_count         = req_count         + 1,
                error_count       = error_count       + excluded.error_count,
                prompt_tokens     = prompt_tokens     + excluded.prompt_tokens,
                completion_tokens = completion_tokens + excluded.completion_tokens,
                total_tokens      = total_tokens      + excluded.total_tokens",
        )
        .bind(&day)
        .bind(&r.user_id)
        .bind(r.user_email.as_deref())
        .bind(r.source.as_str())
        .bind(r.kind.as_str())
        .bind(&r.backend)
        .bind(&r.model)
        .bind(is_error)
        .bind(r.prompt_tokens.unwrap_or(0))
        .bind(r.completion_tokens.unwrap_or(0))
        .bind(r.total_tokens.unwrap_or(0))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Delete raw `usage_events` rows older than `older_than` (RFC 3339 UTC).
/// Rollups are never pruned. Returns the number of rows removed.
pub async fn prune(pool: &Pool, older_than: Timestamp) -> Result<u64, DbError> {
    let res = sqlx::query("DELETE FROM usage_events WHERE created_at < ?")
        .bind(fmt_ts(older_than))
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ------------------------------------------------------------------ queries

/// A reporting period. All of these are day-aligned in the viewer's
/// timezone except `Last24h`, which is a rolling sub-day window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Last24h,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
}

impl Period {
    /// Parse the `period` query param; unknown / missing → `Today`.
    pub fn parse(s: Option<&str>) -> Period {
        match s.unwrap_or("today") {
            "24h" => Period::Last24h,
            "this_week" => Period::ThisWeek,
            "last_week" => Period::LastWeek,
            "this_month" => Period::ThisMonth,
            "last_month" => Period::LastMonth,
            _ => Period::Today,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Last24h => "24h",
            Period::ThisWeek => "this_week",
            Period::LastWeek => "last_week",
            Period::ThisMonth => "this_month",
            Period::LastMonth => "last_month",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Period::Today => "Today",
            Period::Last24h => "Last 24 hours",
            Period::ThisWeek => "This week",
            Period::LastWeek => "Last week",
            Period::ThisMonth => "This month",
            Period::LastMonth => "Last month",
        }
    }

    pub const ALL: [Period; 6] = [
        Period::Today,
        Period::Last24h,
        Period::ThisWeek,
        Period::LastWeek,
        Period::ThisMonth,
        Period::LastMonth,
    ];
}

/// A half-open UTC instant range `[start, end)` for a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub start: Timestamp,
    pub end: Timestamp,
}

/// Resolve a period to a UTC instant range, with day boundaries taken in
/// `tz_name` (falling back to UTC for an unknown zone). `now` is passed in
/// rather than read from the clock so this is pure and unit-testable.
pub fn period_bounds(period: Period, tz_name: &str, now: Timestamp) -> Bounds {
    let tz = jiff::tz::TimeZone::get(tz_name).unwrap_or(jiff::tz::TimeZone::UTC);
    let today = now.to_zoned(tz.clone()).date();

    // Start of a civil day, in `tz`, as a UTC instant.
    let day_start = |date: jiff::civil::Date| -> Timestamp {
        date.to_zoned(tz.clone())
            .map(|z| z.timestamp())
            .unwrap_or(now)
    };
    let add_days = |date: jiff::civil::Date, n: i64| -> jiff::civil::Date {
        date.checked_add(jiff::Span::new().days(n)).unwrap_or(date)
    };
    let add_months = |date: jiff::civil::Date, n: i64| -> jiff::civil::Date {
        date.checked_add(jiff::Span::new().months(n))
            .unwrap_or(date)
    };

    match period {
        Period::Today => Bounds {
            start: day_start(today),
            end: day_start(add_days(today, 1)),
        },
        Period::Last24h => Bounds {
            start: now
                .checked_sub(SignedDuration::from_hours(24))
                .unwrap_or(now),
            end: now,
        },
        Period::ThisWeek => {
            let from_monday = i64::from(today.weekday().to_monday_zero_offset());
            let monday = add_days(today, -from_monday);
            Bounds {
                start: day_start(monday),
                end: day_start(add_days(monday, 7)),
            }
        }
        Period::LastWeek => {
            let from_monday = i64::from(today.weekday().to_monday_zero_offset());
            let this_monday = add_days(today, -from_monday);
            Bounds {
                start: day_start(add_days(this_monday, -7)),
                end: day_start(this_monday),
            }
        }
        Period::ThisMonth => {
            let first = today.first_of_month();
            Bounds {
                start: day_start(first),
                end: day_start(add_months(first, 1)),
            }
        }
        Period::LastMonth => {
            let first = today.first_of_month();
            Bounds {
                start: day_start(add_months(first, -1)),
                end: day_start(first),
            }
        }
    }
}

/// Optional slicers applied to every aggregation query.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub source: Option<String>,
    pub backend: Option<String>,
    /// Scopes results to one user — set for the self-view, `None` for admin.
    pub user_id: Option<String>,
}

impl Filter {
    fn where_sql(&self) -> (String, Vec<String>) {
        let mut sql = String::new();
        let mut binds = Vec::new();
        if let Some(s) = self.source.as_ref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND source = ?");
            binds.push(s.clone());
        }
        if let Some(b) = self.backend.as_ref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND backend = ?");
            binds.push(b.clone());
        }
        if let Some(u) = self.user_id.as_ref().filter(|s| !s.is_empty()) {
            sql.push_str(" AND user_id = ?");
            binds.push(u.clone());
        }
        (sql, binds)
    }
}

/// Top-line totals for a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub requests: i64,
    pub total_tokens: i64,
    pub unique_users: i64,
    pub errors: i64,
}

/// One grouped breakdown row (by user / backend / source / model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCount {
    /// The grouping key (user_id, backend name, source, or model).
    pub key: String,
    /// Display label (user email for by-user; same as key otherwise).
    pub label: String,
    pub requests: i64,
    pub total_tokens: i64,
    pub errors: i64,
}

/// Everything the usage page renders for one window+filter.
#[derive(Debug, Clone, Default)]
pub struct Aggregates {
    pub summary: Summary,
    /// Empty for the self-view (a user is the only user in their own data).
    pub by_user: Vec<GroupCount>,
    pub by_backend: Vec<GroupCount>,
    pub by_source: Vec<GroupCount>,
    pub by_model: Vec<GroupCount>,
}

/// Which physical table answers a window. Raw is precise (and serves every
/// UI period); daily rollups serve ranges older than the retention window.
struct ReadPlan {
    table: &'static str,
    time_col: &'static str,
    start: String,
    end: String,
    /// `usage_daily` pre-aggregates counts; `usage_events` is row-per-call.
    rollup: bool,
}

impl ReadPlan {
    fn req_expr(&self) -> &'static str {
        if self.rollup {
            "COALESCE(SUM(req_count), 0)"
        } else {
            "COUNT(*)"
        }
    }
    fn err_expr(&self) -> &'static str {
        if self.rollup {
            "COALESCE(SUM(error_count), 0)"
        } else {
            "COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0)"
        }
    }
}

/// Aggregate a window into summary + breakdowns. Picks the raw table when
/// `bounds.start` is within `retention_days` of `now` (always true for the
/// UI's periods), else the daily rollups.
pub async fn aggregate(
    pool: &Pool,
    bounds: Bounds,
    filter: &Filter,
    retention_days: i64,
    now: Timestamp,
    include_by_user: bool,
) -> Result<Aggregates, DbError> {
    // Clamp to ≥1 day (a zero/negative window would route every query to the
    // coarser rollups); `saturating_mul` guards an absurd config from
    // overflowing.
    let horizon = now
        .checked_sub(SignedDuration::from_hours(
            retention_days.max(1).saturating_mul(24),
        ))
        .unwrap_or(now);
    let plan = if bounds.start < horizon {
        ReadPlan {
            table: "usage_daily",
            time_col: "day",
            start: fmt_day(bounds.start),
            end: fmt_day(bounds.end),
            rollup: true,
        }
    } else {
        ReadPlan {
            table: "usage_events",
            time_col: "created_at",
            start: fmt_ts(bounds.start),
            end: fmt_ts(bounds.end),
            rollup: false,
        }
    };

    let summary = query_summary(pool, &plan, filter).await?;
    let by_backend = query_group(pool, &plan, filter, "backend", "backend").await?;
    let by_source = query_group(pool, &plan, filter, "source", "source").await?;
    let by_model = query_group(pool, &plan, filter, "model", "model").await?;
    let by_user = if include_by_user {
        query_group(pool, &plan, filter, "user_id", "MAX(user_email)").await?
    } else {
        Vec::new()
    };

    Ok(Aggregates {
        summary,
        by_user,
        by_backend,
        by_source,
        by_model,
    })
}

async fn query_summary(pool: &Pool, plan: &ReadPlan, filter: &Filter) -> Result<Summary, DbError> {
    let (fsql, binds) = filter.where_sql();
    let sql = format!(
        "SELECT {req} AS requests, COALESCE(SUM(total_tokens), 0) AS total_tokens, \
                COUNT(DISTINCT user_id) AS unique_users, {err} AS errors \
         FROM {table} WHERE {col} >= ? AND {col} < ?{fsql}",
        req = plan.req_expr(),
        err = plan.err_expr(),
        table = plan.table,
        col = plan.time_col,
    );
    let mut q = sqlx::query(&sql).bind(&plan.start).bind(&plan.end);
    for b in &binds {
        q = q.bind(b);
    }
    let row = q.fetch_one(pool).await?;
    Ok(Summary {
        requests: row.try_get("requests")?,
        total_tokens: row.try_get("total_tokens")?,
        unique_users: row.try_get("unique_users")?,
        errors: row.try_get("errors")?,
    })
}

async fn query_group(
    pool: &Pool,
    plan: &ReadPlan,
    filter: &Filter,
    key_col: &str,
    label_expr: &str,
) -> Result<Vec<GroupCount>, DbError> {
    let (fsql, binds) = filter.where_sql();
    let sql = format!(
        "SELECT {key} AS k, {label} AS label, {req} AS requests, \
                COALESCE(SUM(total_tokens), 0) AS total_tokens, {err} AS errors \
         FROM {table} WHERE {col} >= ? AND {col} < ?{fsql} \
         GROUP BY {key} ORDER BY requests DESC, k ASC",
        key = key_col,
        label = label_expr,
        req = plan.req_expr(),
        err = plan.err_expr(),
        table = plan.table,
        col = plan.time_col,
    );
    let mut q = sqlx::query(&sql).bind(&plan.start).bind(&plan.end);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            let key: String = row.try_get("k")?;
            let label: Option<String> = row.try_get("label")?;
            Ok(GroupCount {
                label: label.unwrap_or_else(|| key.clone()),
                key,
                requests: row.try_get("requests")?,
                total_tokens: row.try_get("total_tokens")?,
                errors: row.try_get("errors")?,
            })
        })
        .collect()
}

/// Per-backend request counts bucketed across the recent window, oldest →
/// newest, for the backends-page sparkline. `bucket_minutes` is each
/// bucket's width and `buckets` how many (e.g. 5 × 12 = the last hour).
/// Backends with no recent activity are absent from the map. Counts every
/// source (a backend's total load), not just one access method.
///
/// Bucketing is done with Rust-computed RFC 3339 boundary strings + lexical
/// `>=`/`<` comparisons (the convention used everywhere else here), so it
/// doesn't depend on SQLite parsing the stored `…Z` timestamps.
pub async fn recent_buckets_by_backend(
    pool: &Pool,
    now: Timestamp,
    bucket_minutes: i64,
    buckets: i64,
) -> Result<HashMap<String, Vec<i64>>, DbError> {
    if buckets <= 0 || bucket_minutes <= 0 {
        return Ok(HashMap::new());
    }
    // `buckets + 1` edges, oldest first; bucket k spans [edges[k], edges[k+1]).
    let edges: Vec<String> = (0..=buckets)
        .rev()
        .map(|i| {
            let t = now
                .checked_sub(SignedDuration::from_mins(i * bucket_minutes))
                .unwrap_or(now);
            fmt_ts(t)
        })
        .collect();

    let mut cols = String::new();
    for k in 0..buckets {
        cols.push_str(&format!(
            ", SUM(CASE WHEN created_at >= ? AND created_at < ? THEN 1 ELSE 0 END) AS b{k}"
        ));
    }
    let sql =
        format!("SELECT backend{cols} FROM usage_events WHERE created_at >= ? GROUP BY backend");
    let mut q = sqlx::query(&sql);
    for k in 0..buckets as usize {
        q = q.bind(&edges[k]).bind(&edges[k + 1]);
    }
    q = q.bind(&edges[0]);

    let rows = q.fetch_all(pool).await?;
    let mut map = HashMap::new();
    for row in &rows {
        let backend: String = row.try_get("backend")?;
        let mut v = vec![0i64; buckets as usize];
        for (k, slot) in v.iter_mut().enumerate() {
            *slot = row.try_get(format!("b{k}").as_str())?;
        }
        map.insert(backend, v);
    }
    Ok(map)
}

/// Distinct backend names seen in the raw window — populates the page's
/// backend filter dropdown. Raw-only (the UI windows are all in-window).
pub async fn distinct_backends(pool: &Pool, bounds: Bounds) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query(
        "SELECT DISTINCT backend FROM usage_events \
         WHERE created_at >= ? AND created_at < ? ORDER BY backend",
    )
    .bind(fmt_ts(bounds.start))
    .bind(fmt_ts(bounds.end))
    .fetch_all(pool)
    .await?;
    rows.iter().map(|r| Ok(r.try_get("backend")?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        super::super::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    fn rec(
        user: &str,
        backend: &str,
        source: UsageSource,
        total: i64,
        at: Timestamp,
    ) -> UsageRecord {
        UsageRecord {
            created_at: at,
            user_id: user.into(),
            user_email: Some(format!("{user}@x")),
            token_id: None,
            token_name: None,
            source,
            kind: UsageKind::Chat,
            backend: backend.into(),
            model: "qwen".into(),
            status: 200,
            duration_ms: 10,
            prompt_tokens: Some(total / 2),
            completion_tokens: Some(total / 2),
            total_tokens: Some(total),
        }
    }

    #[test]
    fn usage_from_value_reads_openai_shape() {
        let v = serde_json::json!({
            "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
        });
        assert_eq!(usage_from_value(&v), (Some(12), Some(7), Some(19)));
    }

    #[test]
    fn usage_from_value_missing_is_none() {
        let v = serde_json::json!({"choices": []});
        assert_eq!(usage_from_value(&v), (None, None, None));
        // Embeddings: prompt+total but no completion.
        let e = serde_json::json!({"usage": {"prompt_tokens": 5, "total_tokens": 5}});
        assert_eq!(usage_from_value(&e), (Some(5), None, Some(5)));
    }

    #[test]
    fn period_bounds_today_is_local_midnight() {
        // 2026-06-20T08:00:00Z is 2026-06-20T10:00 in Berlin (UTC+2 in June),
        // so "today" starts at 2026-06-19T22:00:00Z (Berlin midnight).
        let now: Timestamp = "2026-06-20T08:00:00Z".parse().unwrap();
        let b = period_bounds(Period::Today, "Europe/Berlin", now);
        assert_eq!(fmt_ts(b.start), "2026-06-19T22:00:00Z");
        assert_eq!(fmt_ts(b.end), "2026-06-20T22:00:00Z");
    }

    #[test]
    fn period_bounds_last_month_spans_previous_calendar_month() {
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        let b = period_bounds(Period::LastMonth, "UTC", now);
        assert_eq!(fmt_ts(b.start), "2026-05-01T00:00:00Z");
        assert_eq!(fmt_ts(b.end), "2026-06-01T00:00:00Z");
    }

    #[test]
    fn period_bounds_this_week_starts_monday() {
        // 2026-06-20 is a Saturday → week started Monday 2026-06-15.
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        let b = period_bounds(Period::ThisWeek, "UTC", now);
        assert_eq!(fmt_ts(b.start), "2026-06-15T00:00:00Z");
        assert_eq!(fmt_ts(b.end), "2026-06-22T00:00:00Z");
    }

    #[tokio::test]
    async fn insert_then_aggregate_round_trips() {
        let pool = pool().await;
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        let batch = vec![
            rec("alice", "gpu-01", UsageSource::V1Api, 100, now),
            rec("alice", "gpu-01", UsageSource::Chat, 50, now),
            rec("bob", "gpu-02", UsageSource::V1Api, 30, now),
        ];
        insert_batch(&pool, &batch).await.unwrap();

        let bounds = period_bounds(Period::Today, "UTC", now);
        let agg = aggregate(&pool, bounds, &Filter::default(), 90, now, true)
            .await
            .unwrap();
        assert_eq!(agg.summary.requests, 3);
        assert_eq!(agg.summary.total_tokens, 180);
        assert_eq!(agg.summary.unique_users, 2);

        // alice leads by request volume.
        assert_eq!(agg.by_user[0].key, "alice");
        assert_eq!(agg.by_user[0].requests, 2);
        assert_eq!(agg.by_user[0].label, "alice@x");

        // Backend split.
        let gpu01 = agg.by_backend.iter().find(|g| g.key == "gpu-01").unwrap();
        assert_eq!(gpu01.requests, 2);
        assert_eq!(gpu01.total_tokens, 150);
    }

    #[tokio::test]
    async fn rollup_accumulates_in_place() {
        let pool = pool().await;
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        // Five events, identical dimensions → one rollup row, req_count 5.
        let batch: Vec<_> = (0..5)
            .map(|_| rec("alice", "gpu-01", UsageSource::V1Api, 10, now))
            .collect();
        insert_batch(&pool, &batch).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "same dimensions collapse to one rollup row");
        let req: i64 = sqlx::query_scalar("SELECT req_count FROM usage_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(req, 5);
        let tot: i64 = sqlx::query_scalar("SELECT total_tokens FROM usage_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tot, 50);
    }

    #[tokio::test]
    async fn filter_narrows_by_source_and_backend() {
        let pool = pool().await;
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        insert_batch(
            &pool,
            &[
                rec("alice", "gpu-01", UsageSource::V1Api, 10, now),
                rec("alice", "gpu-02", UsageSource::Chat, 10, now),
            ],
        )
        .await
        .unwrap();
        let bounds = period_bounds(Period::Today, "UTC", now);
        let f = Filter {
            source: Some("chat".into()),
            ..Default::default()
        };
        let agg = aggregate(&pool, bounds, &f, 90, now, true).await.unwrap();
        assert_eq!(agg.summary.requests, 1);
        assert_eq!(agg.by_backend.len(), 1);
        assert_eq!(agg.by_backend[0].key, "gpu-02");
    }

    #[tokio::test]
    async fn recent_buckets_place_events_in_the_right_slot() {
        let pool = pool().await;
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        // One event ~2 min ago (newest bucket), one ~12 min ago (3rd-from-end
        // with 5-min buckets), one ~40 min ago, one ~90 min ago (out of a
        // 60-min window → dropped).
        let mins_ago = |m: i64| now.checked_sub(SignedDuration::from_mins(m)).unwrap();
        insert_batch(
            &pool,
            &[
                rec("alice", "gpu-01", UsageSource::V1Api, 1, mins_ago(2)),
                rec("alice", "gpu-01", UsageSource::Chat, 1, mins_ago(12)),
                rec("alice", "gpu-01", UsageSource::V1Api, 1, mins_ago(40)),
                rec("alice", "gpu-01", UsageSource::V1Api, 1, mins_ago(90)),
            ],
        )
        .await
        .unwrap();

        // 5-min buckets × 12 = last hour.
        let map = recent_buckets_by_backend(&pool, now, 5, 12).await.unwrap();
        let v = map.get("gpu-01").expect("backend present");
        assert_eq!(v.len(), 12);
        let total: i64 = v.iter().sum();
        assert_eq!(total, 3, "the 90-min-old event is outside the window");
        // Newest bucket (last slot) holds the 2-min-old event.
        assert_eq!(v[11], 1);
        // The 90-min event never lands in any slot.
        assert_eq!(v[0], 0);
    }

    #[tokio::test]
    async fn prune_drops_old_raw_rows_keeps_rollups() {
        let pool = pool().await;
        let now: Timestamp = "2026-06-20T12:00:00Z".parse().unwrap();
        let old: Timestamp = "2026-01-01T12:00:00Z".parse().unwrap();
        insert_batch(
            &pool,
            &[
                rec("alice", "gpu-01", UsageSource::V1Api, 10, old),
                rec("alice", "gpu-01", UsageSource::V1Api, 10, now),
            ],
        )
        .await
        .unwrap();
        let cutoff: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        let removed = prune(&pool, cutoff).await.unwrap();
        assert_eq!(removed, 1);
        let raw: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(raw, 1);
        // Rollups untouched (both days still present).
        let days: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(days, 2);
    }
}
