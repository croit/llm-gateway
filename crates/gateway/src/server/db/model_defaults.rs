// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-model sampling defaults — the rows behind `/admin/models`.
//!
//! Each row stores the raw TOML string the admin typed in the UI;
//! we round-trip it verbatim on edit so re-rendering doesn't
//! silently re-format the operator's input. Parsing happens at
//! request-merge time, with a save-time syntactic validation that
//! rejects obviously-broken submissions.
//!
//! Schema lives in `migrations/0006_model_defaults.sql`.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

/// One stored row, surface-exposed to the admin UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDefaults {
    pub model_name: String,
    /// Raw TOML — what the admin typed. Round-tripped verbatim on
    /// re-render. May be empty (operator cleared the textarea +
    /// hit save — equivalent to deleting the row, but tolerated).
    pub defaults_toml: String,
    /// How this model expresses its reasoning budget on the wire
    /// (`qwen` | `openai` | `glm` | `anthropic` | `none`). `None` =
    /// auto-detect from the model name at request time. Drives
    /// [`crate::server::reasoning::apply_effort`].
    pub reasoning_style: Option<String>,
    /// Per-effort token budgets for token-budget styles (Qwen, Anthropic).
    /// `None` = use the built-in default for that level. Stored as SQLite
    /// INTEGER; see migration 0030.
    pub thinking_budget_standard: Option<i64>,
    pub thinking_budget_deep: Option<i64>,
    pub thinking_budget_max: Option<i64>,
    /// Per-effort `reasoning_effort` levels for effort-level styles (OpenAI,
    /// GLM). `None` = use the built-in default for that level.
    pub reasoning_effort_standard: Option<String>,
    pub reasoning_effort_deep: Option<String>,
    pub reasoning_effort_max: Option<String>,
    /// The model's context window in tokens. Drives the auto-compaction
    /// trigger threshold (a fraction of this window). `None` = fall back
    /// to the global `[chat.compaction] default_context_window`. See
    /// migration 0032.
    pub context_window: Option<i64>,
    /// Price per 1,000,000 prompt tokens, in the deployment currency.
    /// `None` = unpriced (contributes 0 cost — the default for self-hosted
    /// models). Drives the `cost` column on usage rows. See migration 0037.
    pub input_price: Option<f64>,
    /// Price per 1,000,000 completion tokens. `None` = unpriced.
    pub output_price: Option<f64>,
    /// What this model can/cannot do (vision, tools, …). Tri-state per field:
    /// `None` = unknown, `Some(true)` = supported, `Some(false)` = confirmed
    /// unsupported. See [`ModelCapabilities`].
    pub capabilities: ModelCapabilities,
    pub updated_at: Timestamp,
}

/// Per-model capability flags and fallback references.
///
/// Each flag is tri-state: `None` = unknown (try and learn from upstream
/// errors), `Some(true)` = confirmed supported, `Some(false)` = confirmed
/// unsupported (either learned via [`mark_unsupported`] or set by an admin).
///
/// `fallback_*` holds the model id to use for transparent describe-and-inject
/// when the primary model lacks that capability. `None` = no fallback
/// configured (the request is rejected with a clear message instead).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCapabilities {
    pub vision: Option<bool>,
    pub audio_input: Option<bool>,
    pub pdf_input: Option<bool>,
    pub tools: Option<bool>,
    pub parallel_tools: Option<bool>,
    pub structured_output: Option<bool>,
    pub fallback_vision: Option<String>,
    pub fallback_tools: Option<String>,
}

fn map_row(row: &SqliteRow) -> Result<ModelDefaults, DbError> {
    let model_name: String = row.try_get("model_name")?;
    let defaults_toml: String = row.try_get("defaults_toml")?;
    let reasoning_style: Option<String> = row.try_get("reasoning_style")?;
    let thinking_budget_standard: Option<i64> = row.try_get("thinking_budget_standard")?;
    let thinking_budget_deep: Option<i64> = row.try_get("thinking_budget_deep")?;
    let thinking_budget_max: Option<i64> = row.try_get("thinking_budget_max")?;
    let reasoning_effort_standard: Option<String> = row.try_get("reasoning_effort_standard")?;
    let reasoning_effort_deep: Option<String> = row.try_get("reasoning_effort_deep")?;
    let reasoning_effort_max: Option<String> = row.try_get("reasoning_effort_max")?;
    let context_window: Option<i64> = row.try_get("context_window")?;
    let input_price: Option<f64> = row.try_get("input_price")?;
    let output_price: Option<f64> = row.try_get("output_price")?;
    let capabilities = map_capabilities(row)?;
    let updated_at_s: String = row.try_get("updated_at")?;
    let updated_at: Timestamp = updated_at_s
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "updated_at",
            source: e.into(),
        })?;
    Ok(ModelDefaults {
        model_name,
        defaults_toml,
        reasoning_style,
        thinking_budget_standard,
        thinking_budget_deep,
        thinking_budget_max,
        reasoning_effort_standard,
        reasoning_effort_deep,
        reasoning_effort_max,
        context_window,
        input_price,
        output_price,
        capabilities,
        updated_at,
    })
}

fn map_capabilities(row: &SqliteRow) -> Result<ModelCapabilities, DbError> {
    fn nb(row: &SqliteRow, col: &str) -> Result<Option<bool>, DbError> {
        let v: Option<i64> = row.try_get(col)?;
        Ok(v.map(|x| x != 0))
    }
    Ok(ModelCapabilities {
        vision: nb(row, "cap_vision")?,
        audio_input: nb(row, "cap_audio_input")?,
        pdf_input: nb(row, "cap_pdf_input")?,
        tools: nb(row, "cap_tools")?,
        parallel_tools: nb(row, "cap_parallel_tools")?,
        structured_output: nb(row, "cap_structured_output")?,
        fallback_vision: row.try_get("fallback_vision")?,
        fallback_tools: row.try_get("fallback_tools")?,
    })
}

/// Look up one row. `None` means "no defaults set" — callers fall
/// through to forwarding the client body verbatim.
pub async fn get(pool: &Pool, model_name: &str) -> Result<Option<ModelDefaults>, DbError> {
    let row = sqlx::query(
        r#"SELECT model_name, defaults_toml, reasoning_style,
                  thinking_budget_standard, thinking_budget_deep, thinking_budget_max,
                  reasoning_effort_standard, reasoning_effort_deep, reasoning_effort_max,
                  context_window, input_price, output_price,
                  cap_vision, cap_audio_input, cap_pdf_input,
                  cap_tools, cap_parallel_tools, cap_structured_output,
                  fallback_vision, fallback_tools,
                  updated_at
           FROM model_defaults
           WHERE model_name = ?"#,
    )
    .bind(model_name)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_row).transpose()
}

/// Set (or clear, with `None`) the reasoning style for a model without
/// touching its sampling defaults. Inserts a row with empty defaults if none
/// exists yet, so an admin can configure reasoning before any sampling
/// defaults; on conflict only `reasoning_style` is updated, preserving any
/// stored TOML.
pub async fn set_reasoning_style(
    pool: &Pool,
    model_name: &str,
    reasoning_style: Option<&str>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, reasoning_style, updated_at)
           VALUES (?, '', ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             reasoning_style = excluded.reasoning_style,
             updated_at      = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(reasoning_style)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set (or clear, with `None`) the model's context window in tokens without
/// touching its sampling defaults or reasoning config. Inserts a row with empty
/// defaults if none exists yet; on conflict only `context_window` is updated,
/// so it composes with the other setters in any order.
pub async fn set_context_window(
    pool: &Pool,
    model_name: &str,
    context_window: Option<i64>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, context_window, updated_at)
           VALUES (?, '', ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             context_window = excluded.context_window,
             updated_at     = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(context_window)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set (or clear, with `None`) the per-1M-token prices for a model without
/// touching its sampling defaults, reasoning config, or context window.
/// Inserts a row with empty defaults if none exists yet; on conflict only the
/// two price columns are updated, so it composes with the other setters in any
/// order. `None` clears a price (the model becomes unpriced → 0 cost).
pub async fn set_pricing(
    pool: &Pool,
    model_name: &str,
    input_price: Option<f64>,
    output_price: Option<f64>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, input_price, output_price, updated_at)
           VALUES (?, '', ?, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             input_price  = excluded.input_price,
             output_price = excluded.output_price,
             updated_at   = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(input_price)
    .bind(output_price)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set all capability flags + fallback references for a model. Admin-UI path.
/// Inserts a row with empty defaults if none exists yet; on conflict only the
/// capability columns are updated, composing with the other setters.
pub async fn set_capabilities(
    pool: &Pool,
    model_name: &str,
    caps: &ModelCapabilities,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults
              (model_name, defaults_toml,
               cap_vision, cap_audio_input, cap_pdf_input,
               cap_tools, cap_parallel_tools, cap_structured_output,
               fallback_vision, fallback_tools,
               cap_updated_at, updated_at)
           VALUES (?, '', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
               cap_vision            = excluded.cap_vision,
               cap_audio_input       = excluded.cap_audio_input,
               cap_pdf_input         = excluded.cap_pdf_input,
               cap_tools             = excluded.cap_tools,
               cap_parallel_tools    = excluded.cap_parallel_tools,
               cap_structured_output = excluded.cap_structured_output,
               fallback_vision       = excluded.fallback_vision,
               fallback_tools        = excluded.fallback_tools,
               cap_updated_at        = excluded.cap_updated_at,
               updated_at            = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(caps.vision.map(|b| b as i64))
    .bind(caps.audio_input.map(|b| b as i64))
    .bind(caps.pdf_input.map(|b| b as i64))
    .bind(caps.tools.map(|b| b as i64))
    .bind(caps.parallel_tools.map(|b| b as i64))
    .bind(caps.structured_output.map(|b| b as i64))
    .bind(&caps.fallback_vision)
    .bind(&caps.fallback_tools)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a single capability as unsupported. Used by auto-learning when the
/// gateway observes a rejection error from the upstream (e.g. a vision 400).
/// Only writes the one column — other capabilities and admin-set values are
/// preserved. Does NOT overwrite an existing `Some(true)` set by an admin.
pub async fn mark_unsupported(
    pool: &Pool,
    model_name: &str,
    field: CapabilityField,
) -> Result<(), DbError> {
    let col = field.column();
    let now = Timestamp::now().to_string();
    let sql = format!(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, {col}, cap_updated_at, updated_at)
           VALUES (?, '', 0, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
               {col} = CASE WHEN {col} = 1 THEN 1 ELSE 0 END,
               cap_updated_at = excluded.cap_updated_at,
               updated_at     = excluded.updated_at"#,
    );
    sqlx::query(&sql)
        .bind(model_name)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(())
}

/// Which capability column to mark in [`mark_unsupported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityField {
    Vision,
    AudioInput,
    PdfInput,
    Tools,
    ParallelTools,
    StructuredOutput,
}

impl CapabilityField {
    pub fn column(self) -> &'static str {
        match self {
            Self::Vision => "cap_vision",
            Self::AudioInput => "cap_audio_input",
            Self::PdfInput => "cap_pdf_input",
            Self::Tools => "cap_tools",
            Self::ParallelTools => "cap_parallel_tools",
            Self::StructuredOutput => "cap_structured_output",
        }
    }
}

/// The six per-effort override columns, grouped so the setter signature stays
/// small. `None` in any field clears that level (falls back to the built-in
/// default). Budgets are token counts; efforts are `reasoning_effort` levels.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReasoningOverrideCols {
    pub budget_standard: Option<i64>,
    pub budget_deep: Option<i64>,
    pub budget_max: Option<i64>,
    pub effort_standard: Option<String>,
    pub effort_deep: Option<String>,
    pub effort_max: Option<String>,
}

/// Set the per-effort reasoning overrides for a model without touching its
/// sampling defaults or reasoning style. Inserts a row with empty defaults if
/// none exists yet; on conflict only the six override columns are updated, so
/// it composes with [`upsert`] and [`set_reasoning_style`] in either order.
pub async fn set_reasoning_overrides(
    pool: &Pool,
    model_name: &str,
    cols: &ReasoningOverrideCols,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults
             (model_name, defaults_toml,
              thinking_budget_standard, thinking_budget_deep, thinking_budget_max,
              reasoning_effort_standard, reasoning_effort_deep, reasoning_effort_max,
              updated_at)
           VALUES (?, '', ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             thinking_budget_standard  = excluded.thinking_budget_standard,
             thinking_budget_deep      = excluded.thinking_budget_deep,
             thinking_budget_max       = excluded.thinking_budget_max,
             reasoning_effort_standard = excluded.reasoning_effort_standard,
             reasoning_effort_deep     = excluded.reasoning_effort_deep,
             reasoning_effort_max      = excluded.reasoning_effort_max,
             updated_at                = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(cols.budget_standard)
    .bind(cols.budget_deep)
    .bind(cols.budget_max)
    .bind(cols.effort_standard.as_deref())
    .bind(cols.effort_deep.as_deref())
    .bind(cols.effort_max.as_deref())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert or replace the row for `model_name`. The caller is
/// responsible for syntactic validation of `defaults_toml` — this
/// function only enforces the DB-level constraints (NOT NULL,
/// PRIMARY KEY).
pub async fn upsert(pool: &Pool, model_name: &str, defaults_toml: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, updated_at)
           VALUES (?, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             defaults_toml = excluded.defaults_toml,
             updated_at    = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(defaults_toml)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// A model's per-1M-token prices. Either side may be `None` (unpriced →
/// that side contributes 0 cost).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelPrice {
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
}

/// Load every priced model's prices into a map, keyed by model name. Rows
/// with no price on either side are skipped. Called once per flush by the
/// usage writer to turn token counts into `cost` without a per-row query —
/// `model_defaults` is small (a handful of rows), so this is cheap.
pub async fn all_prices(
    pool: &Pool,
) -> Result<std::collections::HashMap<String, ModelPrice>, DbError> {
    let rows = sqlx::query(
        "SELECT model_name, input_price, output_price FROM model_defaults \
         WHERE input_price IS NOT NULL OR output_price IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("model_name")?;
        map.insert(
            name,
            ModelPrice {
                input_price: row.try_get("input_price")?,
                output_price: row.try_get("output_price")?,
            },
        );
    }
    Ok(map)
}

/// Drop the row entirely. No-op if it didn't exist — callers don't
/// need to pre-check.
pub async fn delete(pool: &Pool, model_name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM model_defaults WHERE model_name = ?")
        .bind(model_name)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

    #[tokio::test]
    async fn round_trip_get_after_upsert() {
        let pool = fresh().await;
        upsert(&pool, "Qwen/Qwen3-72B", "temperature = 0.7\ntop_p = 0.95")
            .await
            .unwrap();
        let row = get(&pool, "Qwen/Qwen3-72B").await.unwrap().unwrap();
        assert_eq!(row.model_name, "Qwen/Qwen3-72B");
        assert!(row.defaults_toml.contains("temperature = 0.7"));
    }

    #[tokio::test]
    async fn upsert_replaces_existing_row() {
        let pool = fresh().await;
        upsert(&pool, "m", "a = 1").await.unwrap();
        upsert(&pool, "m", "b = 2").await.unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.defaults_toml, "b = 2");
    }

    #[tokio::test]
    async fn get_returns_none_when_missing() {
        let pool = fresh().await;
        assert!(get(&pool, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_missing_row() {
        let pool = fresh().await;
        delete(&pool, "never-existed").await.unwrap();
        upsert(&pool, "m", "x = 1").await.unwrap();
        delete(&pool, "m").await.unwrap();
        assert!(get(&pool, "m").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reasoning_overrides_round_trip() {
        let pool = fresh().await;
        let cols = ReasoningOverrideCols {
            budget_standard: Some(1_024),
            budget_deep: Some(4_096),
            budget_max: None,
            effort_standard: Some("medium".into()),
            effort_deep: None,
            effort_max: Some("max".into()),
        };
        set_reasoning_overrides(&pool, "m", &cols).await.unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.thinking_budget_standard, Some(1_024));
        assert_eq!(row.thinking_budget_deep, Some(4_096));
        assert_eq!(row.thinking_budget_max, None);
        assert_eq!(row.reasoning_effort_standard.as_deref(), Some("medium"));
        assert_eq!(row.reasoning_effort_deep, None);
        assert_eq!(row.reasoning_effort_max.as_deref(), Some("max"));
    }

    /// The three setters touch disjoint columns and compose in any order.
    #[tokio::test]
    async fn overrides_style_and_toml_are_independent() {
        let pool = fresh().await;
        let cols = ReasoningOverrideCols {
            budget_deep: Some(8_192),
            ..Default::default()
        };
        set_reasoning_overrides(&pool, "m", &cols).await.unwrap();
        set_reasoning_style(&pool, "m", Some("qwen")).await.unwrap();
        upsert(&pool, "m", "temperature = 0.5").await.unwrap();

        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.thinking_budget_deep, Some(8_192));
        assert_eq!(row.reasoning_style.as_deref(), Some("qwen"));
        assert_eq!(row.defaults_toml, "temperature = 0.5");
    }

    #[tokio::test]
    async fn pricing_round_trips_and_composes() {
        let pool = fresh().await;
        // Prices set on a model that already has TOML defaults — the setter
        // touches only the two price columns.
        upsert(&pool, "m", "temperature = 0.5").await.unwrap();
        set_pricing(&pool, "m", Some(3.0), Some(15.0))
            .await
            .unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.input_price, Some(3.0));
        assert_eq!(row.output_price, Some(15.0));
        assert_eq!(row.defaults_toml, "temperature = 0.5");

        // Clearing one side leaves the other intact.
        set_pricing(&pool, "m", None, Some(15.0)).await.unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.input_price, None);
        assert_eq!(row.output_price, Some(15.0));
    }

    #[tokio::test]
    async fn all_prices_skips_unpriced_models() {
        let pool = fresh().await;
        set_pricing(&pool, "cloud", Some(3.0), Some(15.0))
            .await
            .unwrap();
        // Priced on one side only — still included.
        set_pricing(&pool, "half", Some(1.0), None).await.unwrap();
        // A row that exists but was never priced — excluded from the map.
        upsert(&pool, "gpu-local", "temperature = 0.7")
            .await
            .unwrap();

        let map = all_prices(&pool).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["cloud"].input_price, Some(3.0));
        assert_eq!(map["cloud"].output_price, Some(15.0));
        assert_eq!(map["half"].input_price, Some(1.0));
        assert_eq!(map["half"].output_price, None);
        assert!(!map.contains_key("gpu-local"));
    }

    #[tokio::test]
    async fn capabilities_round_trip() {
        let pool = fresh().await;
        let caps = ModelCapabilities {
            vision: Some(false),
            tools: Some(true),
            structured_output: Some(false),
            fallback_vision: Some("qwen-vl".into()),
            ..Default::default()
        };
        set_capabilities(&pool, "m", &caps).await.unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.capabilities.vision, Some(false));
        assert_eq!(row.capabilities.tools, Some(true));
        assert_eq!(row.capabilities.structured_output, Some(false));
        assert_eq!(row.capabilities.audio_input, None);
        assert_eq!(row.capabilities.fallback_vision.as_deref(), Some("qwen-vl"));
    }

    #[tokio::test]
    async fn mark_unsupported_does_not_overwrite_admin_true() {
        let pool = fresh().await;
        // Admin sets vision = true.
        let caps = ModelCapabilities {
            vision: Some(true),
            ..Default::default()
        };
        set_capabilities(&pool, "m", &caps).await.unwrap();

        // Auto-learning tries to mark vision as unsupported — must NOT
        // overwrite the admin's explicit Some(true).
        mark_unsupported(&pool, "m", CapabilityField::Vision)
            .await
            .unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.capabilities.vision, Some(true));
    }

    #[tokio::test]
    async fn mark_unsupported_sets_false_on_unknown() {
        let pool = fresh().await;
        upsert(&pool, "m", "temperature = 0.5").await.unwrap();

        // Vision is None (unknown) → auto-learning sets it to Some(false).
        mark_unsupported(&pool, "m", CapabilityField::Vision)
            .await
            .unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.capabilities.vision, Some(false));
        assert_eq!(row.defaults_toml, "temperature = 0.5");
    }
}
