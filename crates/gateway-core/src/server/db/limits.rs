// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rate-limit / quota rules: persistence + resolution.
//!
//! One row per rule in the `limits` table (see `migrations/0038_limits.sql`).
//! A rule caps one [`Dimension`] over one sliding [`Window`], optionally
//! scoped to a single model, for one subject (global / role / user).
//!
//! [`effective_limits`] collapses all the rules that apply to a caller into
//! the ones actually in force, resolving the global → role → user hierarchy
//! per (model-scope, dimension, window) cell. The enforcement layer
//! (`server::limits`) then compares each against the caller's recent usage.

use jiff::{SignedDuration, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// Where a rule attaches. `Global` is the deployment default; `Role` keys on
/// a `[[roles]]` id; `User` keys on a `users.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectType {
    Global,
    Role,
    User,
    /// One API token (`subject_id` = `tokens.id`). Unlike the other three,
    /// a token rule is not part of the global → role → user hierarchy: it is
    /// an *additional* ceiling checked alongside the owner's budget, so a
    /// token can only ever narrow what its owner may spend. See
    /// `limits::Enforcer::check_token`.
    Token,
}

impl SubjectType {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectType::Global => "global",
            SubjectType::Role => "role",
            SubjectType::User => "user",
            SubjectType::Token => "token",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "global" => Some(SubjectType::Global),
            "role" => Some(SubjectType::Role),
            "user" => Some(SubjectType::User),
            "token" => Some(SubjectType::Token),
            _ => None,
        }
    }
}

/// What a rule counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Requests,
    Tokens,
    Cost,
}

impl Dimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Requests => "requests",
            Dimension::Tokens => "tokens",
            Dimension::Cost => "cost",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "requests" => Some(Dimension::Requests),
            "tokens" => Some(Dimension::Tokens),
            "cost" => Some(Dimension::Cost),
            _ => None,
        }
    }
    pub const ALL: [Dimension; 3] = [Dimension::Requests, Dimension::Tokens, Dimension::Cost];
}

/// A sliding window, snapped to the top of the hour (per-second precision is
/// deliberately not offered — see the design note in the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Window {
    Hour,
    Day,
    Week,
    Month,
}

impl Window {
    pub fn as_str(self) -> &'static str {
        match self {
            Window::Hour => "hour",
            Window::Day => "day",
            Window::Week => "week",
            Window::Month => "month",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hour" => Some(Window::Hour),
            "day" => Some(Window::Day),
            "week" => Some(Window::Week),
            "month" => Some(Window::Month),
            _ => None,
        }
    }
    pub const ALL: [Window; 4] = [Window::Hour, Window::Day, Window::Week, Window::Month];

    /// The window's length in hours (a month is a flat 30 days).
    pub fn hours(self) -> i64 {
        match self {
            Window::Hour => 1,
            Window::Day => 24,
            Window::Week => 24 * 7,
            Window::Month => 24 * 30,
        }
    }

    /// Start of the sliding window for `now`: the top of the current hour
    /// minus the window length. Usage from this instant to `now` is what the
    /// rule measures. Snapping to the hour makes the window advance in clean
    /// hourly steps rather than drifting every second.
    pub fn since(self, now: Timestamp) -> Timestamp {
        let floored = hour_floor(now);
        floored
            .checked_sub(SignedDuration::from_hours(self.hours()))
            .unwrap_or(floored)
    }

    /// When the window next advances — the next top of the hour. Shown as the
    /// "refreshes at" hint on the user's usage bars.
    pub fn next_refresh(self, now: Timestamp) -> Timestamp {
        let floored = hour_floor(now);
        floored
            .checked_add(SignedDuration::from_hours(1))
            .unwrap_or(now)
    }
}

/// Floor an instant to the top of its UTC hour.
fn hour_floor(now: Timestamp) -> Timestamp {
    let secs = now.as_second();
    Timestamp::from_second(secs - secs.rem_euclid(3_600)).unwrap_or(now)
}

/// Who set a rule, and therefore who may change it.
///
/// Only token rules have two possible authors. An owner may edit their own
/// token's self-service rules; an admin's cap on that same token is
/// read-only to them, or capping a token would be advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedBy {
    /// Set on `/admin/limits`. Every global / role / user rule is this.
    Admin,
    /// Set by the token's owner on `/tokens`.
    Owner,
}

impl ManagedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            ManagedBy::Admin => "admin",
            ManagedBy::Owner => "owner",
        }
    }
    /// Unknown values read as `Admin`: the stricter of the two, so a row
    /// written by some future version is never editable by an owner on the
    /// strength of a string this build does not understand.
    pub fn parse(s: &str) -> Self {
        match s {
            "owner" => ManagedBy::Owner,
            _ => ManagedBy::Admin,
        }
    }
}

/// One stored rule.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitRule {
    pub id: String,
    pub subject_type: SubjectType,
    pub subject_id: String,
    /// `None` = the aggregate over all metered models; `Some` = one model id.
    pub model: Option<String>,
    pub dimension: Dimension,
    pub window: Window,
    pub value: f64,
    /// Who set this rule — the gate on who may edit or delete it.
    pub managed_by: ManagedBy,
}

fn map_row(row: &SqliteRow) -> Result<LimitRule, DbError> {
    let subject_type_s: String = row.try_get("subject_type")?;
    let dimension_s: String = row.try_get("dimension")?;
    let window_s: String = row.try_get("window_kind")?;
    let decode = |col: &'static str| DbError::Decode {
        column: col,
        source: anyhow::anyhow!("unknown enum value in column `{col}`"),
    };
    Ok(LimitRule {
        id: row.try_get("id")?,
        subject_type: SubjectType::parse(&subject_type_s).ok_or_else(|| decode("subject_type"))?,
        subject_id: row.try_get("subject_id")?,
        model: row.try_get("model")?,
        dimension: Dimension::parse(&dimension_s).ok_or_else(|| decode("dimension"))?,
        window: Window::parse(&window_s).ok_or_else(|| decode("window_kind"))?,
        value: row.try_get("value")?,
        managed_by: ManagedBy::parse(&row.try_get::<String, _>("managed_by")?),
    })
}

const SELECT_COLS: &str = "id, subject_type, subject_id, model, dimension, window_kind, value, \
     managed_by FROM limits";

/// Normalise a model scope: an empty/whitespace string means "all models".
fn norm_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// What an [`upsert`] did — an authorization outcome the caller can act on.
///
/// The refusal is a *decision*, not a storage error. Letting it surface as a
/// UNIQUE violation would make every other integrity failure look like an
/// admin-owned rule, and force the caller to re-query to reconstruct a reason
/// this transaction already knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upserted {
    Created,
    Updated,
    /// An admin owns the rule at these coordinates and the caller is the
    /// token's owner. Nothing was written.
    RefusedAdminOwned,
}

/// Insert a rule, or update the `value` of the existing rule at the same
/// (subject, model-scope, dimension, window) coordinates. `value` is clamped
/// to ≥ 0. Runs in a transaction so the check-then-write can't race the unique
/// index.
/// Admin-managed only, and deliberately so: it discards the outcome, which is
/// safe when the writer is an admin (their save is never refused) and unsafe
/// for anyone else. An owner's save *can* be refused, and routing it through
/// here would swallow that refusal as `Ok(())` — the token owner would be told
/// their quota was saved while the admin's cap stood. Self-service writes go
/// through [`upsert_checked`] and must handle [`Upserted::RefusedAdminOwned`];
/// leaving `managed_by` off this signature is what makes the mistake
/// impossible to make.
pub async fn upsert(
    pool: &Pool,
    subject_type: SubjectType,
    subject_id: &str,
    model: Option<&str>,
    dimension: Dimension,
    window: Window,
    value: f64,
) -> Result<(), DbError> {
    upsert_checked(
        pool,
        subject_type,
        subject_id,
        model,
        dimension,
        window,
        value,
        ManagedBy::Admin,
    )
    .await
    .map(|_| ())
}

/// [`upsert`], reporting what it did. The self-service path needs to tell a
/// refusal from a write; the admin path, which may overwrite anything, does
/// not.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_checked(
    pool: &Pool,
    subject_type: SubjectType,
    subject_id: &str,
    model: Option<&str>,
    dimension: Dimension,
    window: Window,
    value: f64,
    managed_by: ManagedBy,
) -> Result<Upserted, DbError> {
    let now = Timestamp::now().to_string();
    let value = value.max(0.0);
    let model = norm_model(model);
    let mut tx = pool.begin().await?;
    // Who, if anyone, already holds these coordinates. Read inside the
    // transaction so the answer cannot change before the write, and stated as
    // a decision rather than left to the unique index to signal.
    let held: Option<String> = sqlx::query_scalar(
        "SELECT managed_by FROM limits \
         WHERE subject_type = ? AND subject_id = ? AND IFNULL(model, '') = IFNULL(?, '') \
           AND dimension = ? AND window_kind = ?",
    )
    .bind(subject_type.as_str())
    .bind(subject_id)
    .bind(model.as_deref())
    .bind(dimension.as_str())
    .bind(window.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    // An owner never overwrites an admin's rule; an admin may overwrite
    // either, because an operator's cap outranks the token owner's own.
    if managed_by == ManagedBy::Owner
        && held.as_deref().map(ManagedBy::parse) == Some(ManagedBy::Admin)
    {
        return Ok(Upserted::RefusedAdminOwned);
    }
    let updated = sqlx::query(
        "UPDATE limits SET value = ?, updated_at = ?, managed_by = ? \
         WHERE subject_type = ? AND subject_id = ? AND IFNULL(model, '') = IFNULL(?, '') \
           AND dimension = ? AND window_kind = ?",
    )
    .bind(value)
    .bind(&now)
    .bind(managed_by.as_str())
    .bind(subject_type.as_str())
    .bind(subject_id)
    .bind(model.as_deref())
    .bind(dimension.as_str())
    .bind(window.as_str())
    .execute(&mut *tx)
    .await?;
    let outcome = if updated.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO limits \
               (id, subject_type, subject_id, model, dimension, window_kind, value, created_at, \
                updated_at, managed_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(subject_type.as_str())
        .bind(subject_id)
        .bind(model.as_deref())
        .bind(dimension.as_str())
        .bind(window.as_str())
        .bind(value)
        .bind(&now)
        .bind(&now)
        .bind(managed_by.as_str())
        .execute(&mut *tx)
        .await?;
        Upserted::Created
    } else {
        Upserted::Updated
    };
    tx.commit().await?;
    Ok(outcome)
}

/// Delete a rule by id. No-op if it's gone.
pub async fn delete(pool: &Pool, id: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM limits WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete one of a token's *own* rules — the self-service path. Scoped to the
/// token and to `managed_by = 'owner'` in the statement itself rather than
/// checked beforehand, so neither another subject's rule nor an admin's cap
/// on this token can be removed by id. `Ok(false)` when nothing matched.
pub async fn delete_owner_rule(pool: &Pool, token_id: &str, id: &str) -> Result<bool, DbError> {
    let res = sqlx::query(
        "DELETE FROM limits \
         WHERE id = ? AND subject_type = 'token' AND subject_id = ? AND managed_by = 'owner'",
    )
    .bind(id)
    .bind(token_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Drop every rule attached to a token — called when the token itself is
/// deleted. `limits` has no foreign key onto `tokens` (its subject is
/// polymorphic), so without this a deleted token's rules linger forever on
/// `/admin/limits` under an id nothing resolves any more.
/// Takes an executor rather than the pool so a token delete can run it inside
/// its own transaction: the token row and its rules go together or not at all.
pub async fn delete_for_token<'e, E>(exec: E, token_id: &str) -> Result<(), DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM limits WHERE subject_type = 'token' AND subject_id = ?")
        .bind(token_id)
        .execute(exec)
        .await?;
    Ok(())
}

/// Every rule, for the admin editor. Ordered so the page groups cleanly.
pub async fn list_all(pool: &Pool) -> Result<Vec<LimitRule>, DbError> {
    let sql = format!(
        "SELECT {SELECT_COLS} ORDER BY subject_type, subject_id, \
         IFNULL(model, ''), dimension, window_kind"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// Rules that could apply to a caller: the global defaults, any rule for one
/// of the caller's `role_ids`, and any rule targeting the caller directly.
pub async fn applicable(
    pool: &Pool,
    user_id: &str,
    role_ids: &[String],
) -> Result<Vec<LimitRule>, DbError> {
    // Build the role IN-list placeholders (empty is fine — the clause then
    // matches nothing, which SQLite handles via the `IN ()`-avoiding guard).
    let mut sql = format!(
        "SELECT {SELECT_COLS} WHERE subject_type = 'global' \
         OR (subject_type = 'user' AND subject_id = ?)"
    );
    if !role_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", role_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(
            " OR (subject_type = 'role' AND subject_id IN ({placeholders}))"
        ));
    }
    let mut q = sqlx::query(&sql).bind(user_id);
    for r in role_ids {
        q = q.bind(r);
    }
    let rows = q.fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// The rules attached to one API token. Deliberately separate from
/// [`applicable`]: token rules are a second, independent ceiling rather than
/// another tier of the global → role → user hierarchy, so they are never
/// collapsed together with the owner's budget. Feed the result through
/// [`effective_limits`] to get the same per-cell shape.
pub async fn applicable_for_token(pool: &Pool, token_id: &str) -> Result<Vec<LimitRule>, DbError> {
    let sql = format!("SELECT {SELECT_COLS} WHERE subject_type = 'token' AND subject_id = ?");
    let rows = sqlx::query(&sql).bind(token_id).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// Every rule attached to any token owned by `user_id`, paired with its token
/// id — the admin's per-token limit column, in one query instead of N.
pub async fn for_tokens_of_user(pool: &Pool, user_id: &str) -> Result<Vec<LimitRule>, DbError> {
    let sql = format!(
        "SELECT {SELECT_COLS} WHERE subject_type = 'token' \
         AND subject_id IN (SELECT id FROM tokens WHERE user_id = ?)"
    );
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// A resolved, in-force limit for a caller: exactly one per
/// (model-scope, dimension, window) cell after the hierarchy is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveLimit {
    pub model: Option<String>,
    pub dimension: Dimension,
    pub window: Window,
    pub value: f64,
    /// Which level supplied the winning value — for the UI to label the bar.
    pub source: SubjectType,
}

/// Collapse applicable rules into the ones actually in force. Per
/// (model-scope, dimension, window) cell: a user rule wins outright; else the
/// most-generous (largest `value`) role rule; else the global default. Pure —
/// takes the rules already fetched by [`applicable`].
pub fn effective_limits(rules: &[LimitRule]) -> Vec<EffectiveLimit> {
    use std::collections::HashMap;
    // key = (model-scope, dimension, window); value = best rule seen so far.
    type Key = (Option<String>, &'static str, &'static str);
    let mut best: HashMap<Key, &LimitRule> = HashMap::new();

    // Precedence rank: user (2) beats role (1) beats global (0). Within the
    // same level (only possible for roles), the larger value wins.
    let rank = |s: SubjectType| match s {
        // A token rule is only ever resolved against other token rules (see
        // `applicable_for_token`), where the unique index already guarantees
        // one per cell. Ranking it highest is belt-and-braces: if one ever
        // reached the user hierarchy it would narrow, never widen.
        SubjectType::Token => 3u8,
        SubjectType::User => 2,
        SubjectType::Role => 1,
        SubjectType::Global => 0,
    };

    for r in rules {
        let key = (r.model.clone(), r.dimension.as_str(), r.window.as_str());
        match best.get(&key) {
            None => {
                best.insert(key, r);
            }
            Some(cur) => {
                let (rc, cc) = (rank(r.subject_type), rank(cur.subject_type));
                let replace = rc > cc || (rc == cc && r.value > cur.value);
                if replace {
                    best.insert(key, r);
                }
            }
        }
    }

    let mut out: Vec<EffectiveLimit> = best
        .into_values()
        .map(|r| EffectiveLimit {
            model: r.model.clone(),
            dimension: r.dimension,
            window: r.window,
            value: r.value,
            source: r.subject_type,
        })
        .collect();
    // Stable, readable order for the UI: by model, then dimension, then window.
    out.sort_by(|a, b| {
        a.model
            .cmp(&b.model)
            .then(a.dimension.as_str().cmp(b.dimension.as_str()))
            .then_with(|| a.window.hours().cmp(&b.window.hours()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        super::super::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    #[test]
    fn window_since_snaps_to_hour() {
        // 12:34:56Z → hour floor 12:00:00Z; day window starts 24h before that.
        let now: Timestamp = "2026-06-20T12:34:56Z".parse().unwrap();
        assert_eq!(
            Window::Day.since(now).to_string(),
            "2026-06-19T12:00:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .to_string()
        );
        assert_eq!(
            Window::Hour.since(now).to_string(),
            "2026-06-20T11:00:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .to_string()
        );
        // Refresh is the next top of the hour.
        assert_eq!(
            Window::Hour.next_refresh(now).to_string(),
            "2026-06-20T13:00:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .to_string()
        );
    }

    /// An owner's save must not touch an admin's rule at the same
    /// coordinates. Without the `managed_by` clause the UPDATE matches it and
    /// silently rewrites the cap — which would make an admin's per-token
    /// limit advisory, since the owner could simply raise it.
    #[tokio::test]
    async fn an_owner_save_cannot_overwrite_an_admin_rule() {
        let pool = pool().await;
        upsert(
            &pool,
            SubjectType::Token,
            "tok-a",
            None,
            Dimension::Requests,
            Window::Day,
            10.0,
        )
        .await
        .unwrap();

        // The owner tries to raise it to 10_000 at the same coordinates.
        let res = upsert_checked(
            &pool,
            SubjectType::Token,
            "tok-a",
            None,
            Dimension::Requests,
            Window::Day,
            10_000.0,
            ManagedBy::Owner,
        )
        .await
        .expect("a refusal is an outcome, not a storage error");
        assert_eq!(
            res,
            Upserted::RefusedAdminOwned,
            "the write must report the refusal, not swallow it"
        );

        let rules = applicable_for_token(&pool, "tok-a").await.unwrap();
        assert_eq!(rules.len(), 1, "no second rule at the same slot: {rules:?}");
        assert_eq!(rules[0].value, 10.0, "the admin's value stands");
        assert_eq!(rules[0].managed_by, ManagedBy::Admin);
    }

    /// The owner still owns their own rules — this must narrow, not freeze.
    #[tokio::test]
    async fn an_owner_can_update_and_delete_their_own_rule() {
        let pool = pool().await;
        upsert_checked(
            &pool,
            SubjectType::Token,
            "tok-a",
            None,
            Dimension::Requests,
            Window::Day,
            10.0,
            ManagedBy::Owner,
        )
        .await
        .unwrap();
        upsert_checked(
            &pool,
            SubjectType::Token,
            "tok-a",
            None,
            Dimension::Requests,
            Window::Day,
            5.0,
            ManagedBy::Owner,
        )
        .await
        .unwrap();
        let rules = applicable_for_token(&pool, "tok-a").await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value, 5.0);

        assert!(
            delete_owner_rule(&pool, "tok-a", &rules[0].id)
                .await
                .unwrap()
        );
        assert!(
            applicable_for_token(&pool, "tok-a")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// `delete_owner_rule` is scoped in the statement: not another subject's
    /// rule (the id comes from the client), and not an admin's cap on this
    /// very token.
    #[tokio::test]
    async fn delete_owner_rule_refuses_admin_and_foreign_rules() {
        let pool = pool().await;
        upsert(
            &pool,
            SubjectType::Token,
            "tok-a",
            None,
            Dimension::Requests,
            Window::Day,
            10.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::Global,
            "",
            None,
            Dimension::Requests,
            Window::Day,
            99.0,
        )
        .await
        .unwrap();
        let all = list_all(&pool).await.unwrap();
        let admin_cap = all
            .iter()
            .find(|r| r.subject_type == SubjectType::Token)
            .unwrap();
        let global = all
            .iter()
            .find(|r| r.subject_type == SubjectType::Global)
            .unwrap();

        assert!(
            !delete_owner_rule(&pool, "tok-a", &admin_cap.id)
                .await
                .unwrap(),
            "an admin's cap on this token is not the owner's to delete"
        );
        assert!(
            !delete_owner_rule(&pool, "tok-a", &global.id).await.unwrap(),
            "a global rule must not be deletable via a token's own endpoint"
        );
        assert_eq!(list_all(&pool).await.unwrap().len(), 2, "nothing removed");
    }

    /// An unknown `managed_by` string reads as admin — the stricter side, so
    /// a row from a future version is never editable by an owner on the
    /// strength of a value this build doesn't understand.
    #[test]
    fn unknown_managed_by_values_read_as_admin() {
        assert_eq!(ManagedBy::parse("owner"), ManagedBy::Owner);
        assert_eq!(ManagedBy::parse("admin"), ManagedBy::Admin);
        assert_eq!(ManagedBy::parse("something-new"), ManagedBy::Admin);
        assert_eq!(ManagedBy::parse(""), ManagedBy::Admin);
    }

    #[tokio::test]
    async fn upsert_updates_value_in_place() {
        let pool = pool().await;
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            None,
            Dimension::Tokens,
            Window::Week,
            5_000_000.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            None,
            Dimension::Tokens,
            Window::Week,
            9_000_000.0,
        )
        .await
        .unwrap();
        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 1, "same coordinates update, not duplicate");
        assert_eq!(all[0].value, 9_000_000.0);
    }

    #[tokio::test]
    async fn model_scope_is_distinct_from_aggregate() {
        let pool = pool().await;
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            None,
            Dimension::Tokens,
            Window::Week,
            5.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            Some("Fable"),
            Dimension::Tokens,
            Window::Week,
            1.0,
        )
        .await
        .unwrap();
        // Empty-string model scope folds into the aggregate slot.
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            Some("  "),
            Dimension::Tokens,
            Window::Week,
            7.0,
        )
        .await
        .unwrap();
        let all = list_all(&pool).await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "aggregate + Fable; the blank scope is the aggregate"
        );
        let agg = all.iter().find(|r| r.model.is_none()).unwrap();
        assert_eq!(agg.value, 7.0);
    }

    #[tokio::test]
    async fn effective_prefers_user_then_generous_role_then_global() {
        let pool = pool().await;
        // Same cell (aggregate tokens/week) at all three levels.
        upsert(
            &pool,
            SubjectType::Global,
            "",
            None,
            Dimension::Tokens,
            Window::Week,
            1.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::Role,
            "staff",
            None,
            Dimension::Tokens,
            Window::Week,
            10.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::Role,
            "eng",
            None,
            Dimension::Tokens,
            Window::Week,
            50.0,
        )
        .await
        .unwrap();
        upsert(
            &pool,
            SubjectType::User,
            "alice",
            None,
            Dimension::Tokens,
            Window::Week,
            3.0,
        )
        .await
        .unwrap();

        // A user in both roles: the user rule wins outright (even though it's
        // smaller than the role rules).
        let rules = applicable(&pool, "alice", &["staff".into(), "eng".into()])
            .await
            .unwrap();
        let eff = effective_limits(&rules);
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].value, 3.0);
        assert_eq!(eff[0].source, SubjectType::User);

        // A different user with the two roles but no user rule → most-generous
        // role wins (50, not 10).
        let rules = applicable(&pool, "bob", &["staff".into(), "eng".into()])
            .await
            .unwrap();
        let eff = effective_limits(&rules);
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].value, 50.0);
        assert_eq!(eff[0].source, SubjectType::Role);

        // A user with no matching role → the global default.
        let rules = applicable(&pool, "carol", &[]).await.unwrap();
        let eff = effective_limits(&rules);
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].value, 1.0);
        assert_eq!(eff[0].source, SubjectType::Global);
    }
}
