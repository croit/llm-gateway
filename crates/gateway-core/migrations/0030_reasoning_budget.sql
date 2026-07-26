-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Per-model, per-effort reasoning overrides for the "Denkaufwand" knob.
--
-- Migration 0024 added `model_defaults.reasoning_style` (how a model expresses
-- reasoning on the wire) and the conversation `effort` level. This migration
-- lets an admin tune, per model and per *thinking* effort stage
-- (Standard / Deep / Max), how hard that model is allowed to think — overriding
-- the built-in per-style defaults in `server::reasoning`.
--
-- The control differs by backend, so we store two parallel representations and
-- `apply_effort` reads whichever fits the model's resolved reasoning style:
--
--   * token-budget styles (Qwen via vLLM `thinking_token_budget`, Anthropic
--     `thinking.budget_tokens`) read the integer `thinking_budget_*` columns —
--     a hard cap on reasoning tokens.
--   * effort-level styles (OpenAI / GLM via `reasoning_effort`) read the text
--     `reasoning_effort_*` columns — a categorical intensity
--     ("none"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max").
--
-- There is no Fast column on purpose: Fast means "reasoning off / minimal" and
-- keeps its built-in behaviour. NULL in any column = use the built-in default
-- for that style + level, which preserves today's behaviour for every existing
-- row.

ALTER TABLE model_defaults ADD COLUMN thinking_budget_standard INTEGER;
ALTER TABLE model_defaults ADD COLUMN thinking_budget_deep     INTEGER;
ALTER TABLE model_defaults ADD COLUMN thinking_budget_max      INTEGER;

ALTER TABLE model_defaults ADD COLUMN reasoning_effort_standard TEXT;
ALTER TABLE model_defaults ADD COLUMN reasoning_effort_deep     TEXT;
ALTER TABLE model_defaults ADD COLUMN reasoning_effort_max      TEXT;
