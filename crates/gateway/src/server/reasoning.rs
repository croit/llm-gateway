// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The "Denkaufwand" (effort) control: one user-chosen level that drives both
//! the upstream reasoning budget *and* the per-turn tool-round cap.
//!
//! A single knob keeps the UI simple (mirrors ChatGPT's "Denkaufwand"): the
//! user picks Fast / Standard / Deep / Max, and we translate that into
//!
//!   - a backend-specific reasoning parameter ([`apply_effort`]), because the
//!     five backends we target express "think harder" differently:
//!     `Qwen` → `chat_template_kwargs.enable_thinking` (bool) + optional
//!     `thinking_token_budget` (token cap); `OpenAI` → `reasoning_effort`
//!     ("low"|"medium"|"high"); `GLM`/z.AI → `thinking.type`
//!     ("enabled"|"disabled") + `reasoning_effort` ("none"…"max") intensity;
//!     `Anthropic` → `thinking.{type,budget_tokens}`; everything else →
//!     nothing. The per-effort budgets/levels have built-in defaults but can
//!     be tuned per model on `/admin/models` via [`ReasoningOverrides`].
//!   - a tool-round cap ([`Effort::max_rounds`]), so an agentic task that
//!     needs many tool calls can be given more headroom without a second knob.
//!
//! Like `model_defaults`, the merge is *client-wins*: a parameter the request
//! already carries is never overwritten. The chat composer never sets these,
//! so on the chat path the effort always applies; a `/v1` client that sets its
//! own reasoning param keeps it.

use serde_json::{Value, json};

/// Hard ceiling on the per-turn tool-round cap, regardless of effort. Matches
/// the most-headroom effort level ([`Effort::Max`]) and bounds the blast radius
/// of a runaway tool loop.
pub const HARD_ROUND_CAP: u32 = 64;

/// The user-chosen effort level for a conversation. Persisted as the lowercase
/// string in `chat_session_settings.effort`; [`Effort::Standard`] is the
/// default for a missing row / unknown value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Effort {
    /// Reasoning off (or minimal), fewest tool rounds — snappy everyday chat.
    Fast,
    #[default]
    Standard,
    /// More reasoning, more tool headroom — complex questions.
    Deep,
    /// Maximum reasoning + tool headroom — the hardest multi-step tasks.
    Max,
}

impl Effort {
    /// Parse the stored string. `None` / unknown → [`Effort::Standard`], so a
    /// missing row or a future value degrades to the sensible default.
    pub fn from_db(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("fast") => Self::Fast,
            Some("deep") => Self::Deep,
            Some("max") => Self::Max,
            _ => Self::Standard,
        }
    }

    /// The canonical lowercase string persisted in the DB and posted by the UI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Deep => "deep",
            Self::Max => "max",
        }
    }

    /// UI label shown in the composer's effort picker. English — the product's
    /// chrome is English (users may chat in any language, but the UI isn't
    /// localised).
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Standard => "Standard",
            Self::Deep => "Deep",
            Self::Max => "Max",
        }
    }

    /// Per-turn tool-round cap for this level. Bounded by [`HARD_ROUND_CAP`].
    pub fn max_rounds(self) -> u32 {
        match self {
            Self::Fast => 8,
            Self::Standard => 16,
            Self::Deep => 32,
            Self::Max => HARD_ROUND_CAP,
        }
    }

    /// Whether reasoning is enabled at all at this level (Fast turns it off
    /// where the backend supports a toggle).
    fn reasoning_on(self) -> bool {
        !matches!(self, Self::Fast)
    }
}

/// How a model expresses its reasoning budget on the wire. Configured per model
/// on `/admin/models` (`model_defaults.reasoning_style`); `None`/unset
/// auto-detects from the model name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReasoningStyle {
    /// No reasoning support — the effort knob is a no-op (e.g. Voxtral).
    #[default]
    None,
    /// vLLM chat-template flag (`chat_template_kwargs.enable_thinking`), the
    /// Qwen3 convention.
    Qwen,
    /// OpenAI `reasoning_effort` ("low"|"medium"|"high").
    OpenAi,
    /// z.AI / GLM `thinking.type` ("enabled"|"disabled").
    Glm,
    /// Anthropic `thinking.{type, budget_tokens}`.
    Anthropic,
}

impl ReasoningStyle {
    /// Resolve the effective style: an explicit admin choice wins, otherwise
    /// auto-detect from the model name. An explicit `"none"` is honoured (lets
    /// an admin silence a model that name-detection would otherwise enable).
    pub fn resolve(explicit: Option<&str>, model: &str) -> Self {
        match explicit.map(str::trim) {
            Some("qwen") => Self::Qwen,
            Some("openai") => Self::OpenAi,
            Some("glm") => Self::Glm,
            Some("anthropic") => Self::Anthropic,
            Some("none") => Self::None,
            // Empty string / "auto" / unknown / missing → detect.
            _ => Self::detect(model),
        }
    }

    /// Best-effort guess from a model id. Conservative: an unrecognised model
    /// maps to `None` (the effort knob simply does nothing) rather than risk
    /// injecting a parameter the backend rejects.
    pub fn detect(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        // Order matters only where substrings could overlap; these don't.
        if m.contains("qwen") {
            Self::Qwen
        } else if m.contains("gpt")
            || m.starts_with("o1")
            || m.starts_with("o3")
            || m.starts_with("o4")
        {
            Self::OpenAi
        } else if m.contains("glm") || m.contains("z-ai") || m.contains("zhipu") {
            Self::Glm
        } else if m.contains("claude") || m.contains("anthropic") {
            Self::Anthropic
        } else {
            // Voxtral and any unrecognised model: no reasoning parameter.
            Self::None
        }
    }

    /// Canonical string (round-trips with [`Self::resolve`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Qwen => "qwen",
            Self::OpenAi => "openai",
            Self::Glm => "glm",
            Self::Anthropic => "anthropic",
        }
    }

    /// Whether this style supports a reasoning budget at all (drives whether the
    /// UI greys out the effort control for the selected model).
    pub fn supports_reasoning(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this style is tuned by a numeric *token budget* per effort
    /// (`thinking_token_budget` / `thinking.budget_tokens`). The admin UI shows
    /// integer token fields for these.
    pub fn uses_token_budget(self) -> bool {
        matches!(self, Self::Qwen | Self::Anthropic)
    }

    /// Whether this style is tuned by a categorical `reasoning_effort` level
    /// per effort (OpenAI, GLM/z.AI — neither exposes a token cap). The admin
    /// UI shows a level dropdown for these.
    pub fn uses_effort_level(self) -> bool {
        matches!(self, Self::OpenAi | Self::Glm)
    }

    /// Allowed `reasoning_effort` values for this style, most→least thinking.
    /// Empty for token-budget / no-reasoning styles. Single source of truth for
    /// both the admin dropdown and save-time validation.
    pub fn effort_levels(self) -> &'static [&'static str] {
        match self {
            // OpenAI reasoning models accept only these three.
            Self::OpenAi => &["high", "medium", "low"],
            // z.AI / GLM accepts the full intensity scale.
            Self::Glm => &["max", "xhigh", "high", "medium", "low", "minimal", "none"],
            _ => &[],
        }
    }
}

/// Anthropic thinking-token budgets per level. Standard/Deep/Max only; Fast
/// disables thinking. Kept modest so `max_tokens` (which must exceed the budget)
/// stays reasonable.
fn anthropic_budget(effort: Effort) -> Option<u32> {
    match effort {
        Effort::Fast => None,
        Effort::Standard => Some(4_096),
        Effort::Deep => Some(16_384),
        Effort::Max => Some(32_768),
    }
}

/// OpenAI `reasoning_effort` value per level. (OpenAI reasoning models always
/// reason; Fast maps to the cheapest "low" rather than off.)
fn openai_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Fast => "low",
        Effort::Standard => "medium",
        Effort::Deep | Effort::Max => "high",
    }
}

/// GLM / z.AI `reasoning_effort` value per *thinking* level. Fast disables
/// thinking entirely (handled separately), so it has no level here. z.AI's
/// own default is `"max"` (the model always thinks hard); mapping Standard to
/// a lower intensity is what makes the effort knob actually rein GLM in.
fn glm_effort(effort: Effort) -> &'static str {
    match effort {
        // Unused (Fast → thinking disabled), kept total for exhaustiveness.
        Effort::Fast => "low",
        Effort::Standard => "medium",
        Effort::Deep => "high",
        Effort::Max => "max",
    }
}

/// Per-model, per-effort overrides for the reasoning budget, configured on
/// `/admin/models` and stored in `model_defaults`. Two parallel
/// representations because backends differ (see [`ReasoningStyle`]):
///
///   * `budget_*` — integer token caps, used by token-budget styles
///     ([`ReasoningStyle::uses_token_budget`]).
///   * `effort_*` — categorical `reasoning_effort` levels, used by
///     effort-level styles ([`ReasoningStyle::uses_effort_level`]).
///
/// A `None` field means "use the built-in default for that style+level", so an
/// all-default value reproduces the pre-override behaviour exactly. There is no
/// Fast field: Fast means reasoning-off / minimal and keeps its built-in
/// behaviour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReasoningOverrides {
    pub budget_standard: Option<u32>,
    pub budget_deep: Option<u32>,
    pub budget_max: Option<u32>,
    pub effort_standard: Option<String>,
    pub effort_deep: Option<String>,
    pub effort_max: Option<String>,
}

impl ReasoningOverrides {
    /// The token-budget override for `effort`, if any. Fast never has one.
    fn budget(&self, effort: Effort) -> Option<u32> {
        match effort {
            Effort::Fast => None,
            Effort::Standard => self.budget_standard,
            Effort::Deep => self.budget_deep,
            Effort::Max => self.budget_max,
        }
    }

    /// The `reasoning_effort` override for `effort`, if any. Fast never has one.
    fn effort_level(&self, effort: Effort) -> Option<&str> {
        match effort {
            Effort::Fast => None,
            Effort::Standard => self.effort_standard.as_deref(),
            Effort::Deep => self.effort_deep.as_deref(),
            Effort::Max => self.effort_max.as_deref(),
        }
    }
}

/// Translate `effort` into the backend-specific reasoning parameter for
/// `style` and merge it into `body`. `overrides` carries the per-model,
/// per-effort tuning from `/admin/models`; pass [`ReasoningOverrides::default`]
/// for the built-in behaviour. Client-wins: a key the request already set is
/// left untouched. No-op for [`ReasoningStyle::None`].
pub fn apply_effort(
    style: ReasoningStyle,
    effort: Effort,
    overrides: &ReasoningOverrides,
    body: &mut Value,
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match style {
        ReasoningStyle::None => {}
        ReasoningStyle::Qwen => {
            // chat_template_kwargs is a nested object; merge the flag without
            // clobbering other kwargs the client may have set.
            let kwargs = obj
                .entry("chat_template_kwargs")
                .or_insert_with(|| json!({}));
            if let Some(k) = kwargs.as_object_mut()
                && !k.contains_key("enable_thinking")
            {
                k.insert("enable_thinking".into(), Value::Bool(effort.reasoning_on()));
            }
            // vLLM caps reasoning at `thinking_token_budget` tokens (forces the
            // reasoning-end token once hit). Only meaningful when thinking is on.
            if effort.reasoning_on()
                && let Some(budget) = overrides.budget(effort)
                && !obj.contains_key("thinking_token_budget")
            {
                obj.insert("thinking_token_budget".into(), json!(budget));
            }
        }
        ReasoningStyle::OpenAi => {
            if !obj.contains_key("reasoning_effort") {
                let level = overrides
                    .effort_level(effort)
                    .unwrap_or(openai_effort(effort));
                obj.insert("reasoning_effort".into(), Value::String(level.into()));
            }
        }
        ReasoningStyle::Glm => {
            if !obj.contains_key("thinking") {
                let kind = if effort.reasoning_on() {
                    "enabled"
                } else {
                    "disabled"
                };
                obj.insert("thinking".into(), json!({ "type": kind }));
            }
            // z.AI has no token cap; intensity is `reasoning_effort` and only
            // takes effect while thinking is enabled. Its native default is
            // "max", so mapping the effort knob here is what limits GLM.
            if effort.reasoning_on() && !obj.contains_key("reasoning_effort") {
                let level = overrides.effort_level(effort).unwrap_or(glm_effort(effort));
                obj.insert("reasoning_effort".into(), Value::String(level.into()));
            }
        }
        ReasoningStyle::Anthropic => {
            if !obj.contains_key("thinking") {
                // An explicit per-model budget wins over the built-in default.
                match overrides
                    .budget(effort)
                    .or_else(|| anthropic_budget(effort))
                {
                    Some(budget) => {
                        obj.insert(
                            "thinking".into(),
                            json!({ "type": "enabled", "budget_tokens": budget }),
                        );
                    }
                    None => {
                        obj.insert("thinking".into(), json!({ "type": "disabled" }));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_round_trips_and_defaults() {
        assert_eq!(Effort::from_db(Some("fast")), Effort::Fast);
        assert_eq!(Effort::from_db(Some("deep")), Effort::Deep);
        assert_eq!(Effort::from_db(Some("max")), Effort::Max);
        assert_eq!(Effort::from_db(Some("standard")), Effort::Standard);
        // Unknown / missing → standard.
        assert_eq!(Effort::from_db(None), Effort::Standard);
        assert_eq!(Effort::from_db(Some("bogus")), Effort::Standard);
        for e in [Effort::Fast, Effort::Standard, Effort::Deep, Effort::Max] {
            assert_eq!(Effort::from_db(Some(e.as_str())), e);
        }
    }

    #[test]
    fn rounds_scale_with_effort_and_are_capped() {
        assert_eq!(Effort::Fast.max_rounds(), 8);
        assert_eq!(Effort::Standard.max_rounds(), 16);
        assert_eq!(Effort::Deep.max_rounds(), 32);
        assert_eq!(Effort::Max.max_rounds(), HARD_ROUND_CAP);
        assert!(Effort::Max.max_rounds() <= HARD_ROUND_CAP);
    }

    #[test]
    fn style_detect_by_name() {
        assert_eq!(
            ReasoningStyle::detect("Qwen/Qwen3-32B"),
            ReasoningStyle::Qwen
        );
        assert_eq!(ReasoningStyle::detect("gpt-5"), ReasoningStyle::OpenAi);
        assert_eq!(ReasoningStyle::detect("o3-mini"), ReasoningStyle::OpenAi);
        assert_eq!(ReasoningStyle::detect("glm-4.6"), ReasoningStyle::Glm);
        assert_eq!(
            ReasoningStyle::detect("claude-opus-4-8"),
            ReasoningStyle::Anthropic
        );
        // Voxtral and unknowns → no reasoning.
        assert_eq!(
            ReasoningStyle::detect("Voxtral-Small"),
            ReasoningStyle::None
        );
        assert_eq!(
            ReasoningStyle::detect("mystery-model"),
            ReasoningStyle::None
        );
    }

    #[test]
    fn style_explicit_overrides_detection() {
        // An admin can force a style the name wouldn't detect…
        assert_eq!(
            ReasoningStyle::resolve(Some("anthropic"), "mystery"),
            ReasoningStyle::Anthropic
        );
        // …or silence one the name would enable.
        assert_eq!(
            ReasoningStyle::resolve(Some("none"), "Qwen/Qwen3"),
            ReasoningStyle::None
        );
        // Empty/auto → fall back to detection.
        assert_eq!(
            ReasoningStyle::resolve(Some(""), "gpt-4o"),
            ReasoningStyle::OpenAi
        );
        assert_eq!(
            ReasoningStyle::resolve(None, "gpt-4o"),
            ReasoningStyle::OpenAi
        );
    }

    #[test]
    fn none_style_is_a_noop() {
        let mut body = json!({"model": "x", "messages": []});
        apply_effort(
            ReasoningStyle::None,
            Effort::Max,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body, json!({"model": "x", "messages": []}));
    }

    #[test]
    fn qwen_toggles_enable_thinking() {
        let mut body = json!({"model": "Qwen3"});
        apply_effort(
            ReasoningStyle::Qwen,
            Effort::Fast,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );

        let mut body = json!({"model": "Qwen3"});
        apply_effort(
            ReasoningStyle::Qwen,
            Effort::Deep,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
    }

    #[test]
    fn qwen_preserves_other_chat_template_kwargs() {
        let mut body = json!({"chat_template_kwargs": {"foo": 1}});
        apply_effort(
            ReasoningStyle::Qwen,
            Effort::Standard,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["chat_template_kwargs"]["foo"], json!(1));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
    }

    #[test]
    fn openai_sets_reasoning_effort() {
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::OpenAi,
            Effort::Fast,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["reasoning_effort"], json!("low"));
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::OpenAi,
            Effort::Standard,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["reasoning_effort"], json!("medium"));
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::OpenAi,
            Effort::Max,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn glm_sets_thinking_type_and_effort() {
        // Fast → thinking off, no reasoning_effort.
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::Glm,
            Effort::Fast,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["thinking"]["type"], json!("disabled"));
        assert!(body.get("reasoning_effort").is_none());
        // Deep → thinking on + the default intensity for the level.
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::Glm,
            Effort::Deep,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn anthropic_sets_budget() {
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::Anthropic,
            Effort::Fast,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["thinking"]["type"], json!("disabled"));
        let mut body = json!({});
        apply_effort(
            ReasoningStyle::Anthropic,
            Effort::Max,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], json!(32_768));
    }

    #[test]
    fn client_value_wins() {
        // A pre-set reasoning param is never overwritten.
        let mut body = json!({"reasoning_effort": "high"});
        apply_effort(
            ReasoningStyle::OpenAi,
            Effort::Fast,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["reasoning_effort"], json!("high"));

        let mut body = json!({"thinking": {"type": "disabled"}});
        apply_effort(
            ReasoningStyle::Anthropic,
            Effort::Max,
            &ReasoningOverrides::default(),
            &mut body,
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    /// A per-model token budget caps Qwen thinking on the thinking levels and
    /// is omitted on Fast (thinking off).
    #[test]
    fn qwen_token_budget_override() {
        let ov = ReasoningOverrides {
            budget_standard: Some(1_024),
            budget_deep: Some(4_096),
            ..Default::default()
        };
        let mut body = json!({"model": "Qwen3"});
        apply_effort(ReasoningStyle::Qwen, Effort::Deep, &ov, &mut body);
        assert_eq!(body["thinking_token_budget"], json!(4_096));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));

        // Fast → thinking off → no budget even if one is configured.
        let mut body = json!({"model": "Qwen3"});
        apply_effort(ReasoningStyle::Qwen, Effort::Fast, &ov, &mut body);
        assert!(body.get("thinking_token_budget").is_none());

        // A level with no override stays uncapped (backend default).
        let mut body = json!({"model": "Qwen3"});
        apply_effort(ReasoningStyle::Qwen, Effort::Max, &ov, &mut body);
        assert!(body.get("thinking_token_budget").is_none());
    }

    /// A client-supplied budget is never overwritten.
    #[test]
    fn qwen_token_budget_client_wins() {
        let ov = ReasoningOverrides {
            budget_deep: Some(4_096),
            ..Default::default()
        };
        let mut body = json!({"thinking_token_budget": 99});
        apply_effort(ReasoningStyle::Qwen, Effort::Deep, &ov, &mut body);
        assert_eq!(body["thinking_token_budget"], json!(99));
    }

    /// OpenAI / GLM effort-level overrides replace the built-in mapping.
    #[test]
    fn effort_level_overrides() {
        let ov = ReasoningOverrides {
            effort_standard: Some("high".into()),
            effort_deep: Some("minimal".into()),
            ..Default::default()
        };
        let mut body = json!({});
        apply_effort(ReasoningStyle::OpenAi, Effort::Standard, &ov, &mut body);
        assert_eq!(body["reasoning_effort"], json!("high"));

        let mut body = json!({});
        apply_effort(ReasoningStyle::Glm, Effort::Deep, &ov, &mut body);
        assert_eq!(body["reasoning_effort"], json!("minimal"));
        assert_eq!(body["thinking"]["type"], json!("enabled"));
    }

    /// A per-model Anthropic budget overrides the built-in default.
    #[test]
    fn anthropic_budget_override() {
        let ov = ReasoningOverrides {
            budget_max: Some(50_000),
            ..Default::default()
        };
        let mut body = json!({});
        apply_effort(ReasoningStyle::Anthropic, Effort::Max, &ov, &mut body);
        assert_eq!(body["thinking"]["budget_tokens"], json!(50_000));
    }

    #[test]
    fn effort_levels_and_kind_per_style() {
        assert!(ReasoningStyle::Qwen.uses_token_budget());
        assert!(ReasoningStyle::Anthropic.uses_token_budget());
        assert!(ReasoningStyle::OpenAi.uses_effort_level());
        assert!(ReasoningStyle::Glm.uses_effort_level());
        assert_eq!(ReasoningStyle::OpenAi.effort_levels().len(), 3);
        assert_eq!(ReasoningStyle::Glm.effort_levels().len(), 7);
        assert!(ReasoningStyle::Qwen.effort_levels().is_empty());
        assert!(ReasoningStyle::None.effort_levels().is_empty());
    }
}
