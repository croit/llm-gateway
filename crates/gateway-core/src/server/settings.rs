// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The operator settings that used to live in `gateway.toml`, now owned by the
//! database and edited at `/admin/settings`.
//!
//! This is the last step of the move the rest of the gateway already made:
//! upstream topology, groups, search and the OIDC provider are all database
//! rows seeded once from a config file that is then ignored. These blocks —
//! `[chat.ocr]`, `[chat.compaction]`, `[chat.s3]`, `[sandbox]`, `[comfyui]`,
//! `[rag]`, `[skills]`, `[typst]`, `[geoip]`, `[usage]`, `[limits]`,
//! `[feedback]`, `[push]`, and the session/token half of `[gateway]` — were
//! what still forced a file onto a deployment that wanted none.
//!
//! What is left in the file afterwards is `[db]` (the gateway has to find the
//! database before it can read settings out of it) and the two `[gateway]` keys
//! in [`GATEWAY_KEYS_STAYING_IN_THE_FILE`], each for a stated reason.
//!
//! # Shape
//!
//! One [`app_settings`] row per leaf field, keyed by its TOML path under a
//! `settings.` prefix: `[sandbox] runner_url` is `settings.sandbox.runner_url`.
//! Not one JSON blob per block, for the reason the sibling stores give: a row
//! an operator can read and patch in a `sqlite3` shell while debugging is worth
//! more than a compact encoding, and a secret then gets its own sealed row
//! instead of hiding inside an otherwise readable object.
//!
//! # How a value reaches the code that uses it
//!
//! It does not. Nothing outside this module reads these rows. [`apply`]
//! overwrites those blocks of an in-memory [`Config`], and every existing
//! reader keeps saying `state.config().chat.ocr.dpi` exactly as it did when the
//! value came from a file. That is deliberate: the alternative — teaching a
//! hundred call sites to consult a settings store — would have been a far
//! larger and far riskier change than the one this replaces, and it would have
//! made the config file and the database two live sources instead of one.
//!
//! # Drift
//!
//! [`SECTIONS`] and [`apply`] are two spellings of the same list, and a field
//! present in one and missing from the other is exactly the bug this design
//! invites: the editor would show a control that saves a row nobody reads, or
//! the code would read a row the editor cannot set. [`Settings`] therefore
//! records every key [`apply`] reads, and
//! `every_declared_field_is_actually_applied` asserts the two sets are equal.
//! Add a field to one and the test names the other.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::server::config::{
    ChatConfig, ComfyuiConfig, CompactionConfig, Config, FeedbackConfig, GatewayConfig,
    GeoipConfig, LimitsConfig, OcrConfig, PushConfig, RagConfig, S3Config, SandboxConfig,
    SkillsConfig, TypstConfig, UsageConfig,
};
use crate::server::crypto::Crypto;
use crate::server::db::{DbError, Pool, app_settings};
use crate::server::upstreams::config::PoolKind;

/// Prefix for every row this module owns, so the settings namespace cannot
/// collide with `oidc.*`, `setup.*`, `gateway.public_url` or the seed markers.
const PREFIX: &str = "settings.";

/// Marks that the one-time import from a config file has run. Gated on the
/// marker rather than on the rows being empty, so an operator who deliberately
/// clears a setting does not get the file's value resurrected on next boot —
/// the same rule as `topology.seeded`, `rbac.seeded` and `setup.config_imported`.
const IMPORT_MARKER_KEY: &str = "settings.imported";

/// What kind of control edits a field, and how its text form is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `"true"` / `"false"`.
    Bool,
    /// A signed integer. Covers every `usize`/`u32`/`u64`/`i64` field; the
    /// accessors range-check on the way out.
    Int,
    Float,
    Text,
    /// A filesystem path. Same storage as [`Kind::Text`]; rendered with a hint
    /// that it is resolved inside the container, which is the single most
    /// common way these get set wrong.
    Path,
    /// Sealed at rest and write-only in the UI: the editor shows whether one is
    /// set, never the value, and an empty submission leaves the stored value
    /// alone rather than clearing it.
    Secret,
    /// A list of strings, stored as a JSON array.
    List,
    /// A model id, offered as a dropdown of the models actually configured in
    /// the pool of this kind — plus an "automatic" choice for the empty value,
    /// which every one of these fields already treats as "pick the first
    /// available".
    ///
    /// Carrying the [`PoolKind`] here rather than leaving these as free text is
    /// what makes the control answerable: the gateway knows precisely which
    /// models exist, so asking an operator to type one is asking them to
    /// reproduce a list the page could have shown them. It also pins the *right*
    /// list — `chat.ocr.model` has to be a model served by an `ocr`-kind pool,
    /// which is not a fact anybody should have to infer.
    Model(PoolKind),
}

/// How much of a section's two-column row a field's control occupies.
///
/// A DPI or a timeout is four digits; giving it the full width of the card
/// makes a section of seven numbers into a page of scrolling. A URL, a path or
/// a model id genuinely needs the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Its own row.
    Full,
    /// Half a row, so it pairs with the next `Half` field.
    Half,
}

/// The span a [`Kind`] gets unless the field says otherwise: numbers and
/// toggles are short, everything else holds a URL, a path or a list.
const fn span_for(kind: Kind) -> Span {
    match kind {
        Kind::Int | Kind::Float | Kind::Bool => Span::Half,
        Kind::Text | Kind::Path | Kind::Secret | Kind::List | Kind::Model(_) => Span::Full,
    }
}

/// One editable field.
pub struct FieldSpec {
    /// The TOML path this field had in `gateway.toml`, e.g.
    /// `sandbox.runner_url`, used verbatim as the row key (under [`PREFIX`]),
    /// as the form field name, and as the stem of this field's two i18n keys
    /// (see [`FieldSpec::label_key`]).
    ///
    /// The identifier itself is never translated or prettified into "Runner
    /// URL": the editor renders a localised label *and* shows this string
    /// underneath, because an operator is also matching it against
    /// `gateway.example.toml`, the docs, a log line or a support thread, all
    /// of which call it `runner_url`.
    pub key: &'static str,
    pub kind: Kind,
    /// Whether changing this only takes effect on the next restart.
    ///
    /// Every field that was merely *derived* at boot is now rebuilt on save
    /// (see `AppState::reload_settings`), so what is left has one thing in
    /// common: it owns a **long-running background worker**. The RAG indexer
    /// and the ComfyUI job scheduler are spawned tasks that discard their
    /// `JoinHandle`, and replacing one means stopping work that is in flight —
    /// an aborted ComfyUI poll can leave a job row `pending` whose asset is
    /// never fetched, an aborted index pass a half-written index. A restart is
    /// the clean quiesce, and these are tuning values and one URL, so the
    /// trade is not worth data integrity.
    ///
    /// `rag.data_dir` is restart-only for a second, independent reason: the
    /// existing index tree does not move with it, so a silent hot swap would
    /// quietly orphan every index. What *should* happen — reindex, copy, start
    /// empty — is a product decision, not plumbing.
    ///
    /// The editor shows a badge next to these and says so in the save toast.
    pub restart: bool,
    /// How wide the control renders. Defaults from the kind via [`span_for`];
    /// [`f_half`] overrides it for a text field that holds something short.
    pub span: Span,
}

impl FieldSpec {
    /// Fluent key of this field's human label, e.g.
    /// `settings-f-sandbox-runner_url`.
    ///
    /// Derived from [`Self::key`] rather than declared, so a field cannot ship
    /// with a label key nobody wrote: the drift test in the settings page
    /// walks [`SECTIONS`] and asserts every derived key resolves. Only the
    /// dots become dashes — Fluent identifiers reject `.` but allow `_`, so
    /// the mapping stays reversible and greppable.
    pub fn label_key(&self) -> String {
        format!("settings-f-{}", self.key.replace('.', "-"))
    }

    /// Fluent key of this field's one-line explanation.
    pub fn help_key(&self) -> String {
        format!("{}-help", self.label_key())
    }
}

/// A group of fields, rendered as one card.
pub struct SectionSpec {
    /// TOML block name, e.g. `chat.ocr`. Also the anchor in the page and the
    /// stem of the card's two i18n keys (see [`SectionSpec::title_key`]).
    pub name: &'static str,
    /// Which [`Category`] tab this card appears under. Mandatory, so adding a
    /// section forces a decision about where an operator will look for it
    /// rather than silently appending it to a list nobody can scan.
    pub category: Category,
    pub fields: &'static [FieldSpec],
}

impl SectionSpec {
    /// Fluent key of the card's title, e.g. `settings-s-chat-ocr`. Derived
    /// from [`Self::name`] the same way [`FieldSpec::label_key`] is.
    pub fn title_key(&self) -> String {
        format!("settings-s-{}", self.name.replace('.', "-"))
    }

    /// Fluent key of the sentence under the card's title.
    pub fn blurb_key(&self) -> String {
        format!("{}-blurb", self.title_key())
    }
}

/// The tabs `/admin/settings` groups its cards into.
///
/// Fourteen cards in one column is not a page anyone reads, so the editor shows
/// one category at a time. An enum rather than a string: the page dispatches on
/// it, and a typo'd category would otherwise mean a card that exists but is on
/// no tab — visible to the drift tests as nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// What happens inside a conversation.
    Chat,
    /// Things the model can call out to.
    Tools,
    /// Where content and indexes are kept.
    Data,
    /// Who may do what, and for how long.
    Access,
    /// How people are told about things.
    Notifications,
}

impl Category {
    /// Tab order, left to right. The first is the default tab.
    ///
    /// Chat leads because it is what an operator changes most; Access is late
    /// because its defaults are usually right.
    pub const ALL: &'static [Category] = &[
        Category::Chat,
        Category::Tools,
        Category::Data,
        Category::Access,
        Category::Notifications,
    ];

    /// URL slug — `/admin/settings?tab=chat`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Tools => "tools",
            Self::Data => "data",
            Self::Access => "access",
            Self::Notifications => "notifications",
        }
    }

    /// Parse a `?tab=` value. `None` for anything unrecognised, so a stale
    /// bookmark falls back to the default tab rather than rendering an empty
    /// page.
    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.slug() == s)
    }

    /// Fluent key for the tab label. Localised, unlike the field identifiers:
    /// this is navigation chrome, not something an operator matches against a
    /// config file or a log line.
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Chat => "settings-tab-chat",
            Self::Tools => "settings-tab-tools",
            Self::Data => "settings-tab-data",
            Self::Access => "settings-tab-access",
            Self::Notifications => "settings-tab-notifications",
        }
    }

    /// The cards on this tab, in declaration order.
    pub fn sections(self) -> impl Iterator<Item = &'static SectionSpec> {
        SECTIONS.iter().filter(move |s| s.category == self)
    }
}

const fn f(key: &'static str, kind: Kind) -> FieldSpec {
    FieldSpec {
        key,
        kind,
        restart: false,
        span: span_for(kind),
    }
}

/// Same as [`f`], for a field whose kind defaults to full width but whose
/// values are short in practice — a currency code, a region, a repository name.
const fn f_half(key: &'static str, kind: Kind) -> FieldSpec {
    FieldSpec {
        key,
        kind,
        restart: false,
        span: Span::Half,
    }
}

/// Same as [`f`], for a field whose new value is only picked up on restart.
const fn r(key: &'static str, kind: Kind) -> FieldSpec {
    FieldSpec {
        key,
        kind,
        restart: true,
        span: span_for(kind),
    }
}

/// Every field the settings editor knows about.
///
/// The order is the order of the page. Blocks that were `Option<…>` in the
/// config file each gained an explicit `enabled` field: "leave the URL blank to
/// turn it off" is a workable rule in a file an operator writes by hand and a
/// bad one in a form, where a cleared field looks like a mistake rather than a
/// decision.
///
/// Titles, labels and help text live in `session-core/locales/*/settings.ftl`
/// under the keys these entries derive (see [`FieldSpec::label_key`]), not
/// here. Two copies of operator-facing prose — one in Rust for `en` and one in
/// Fluent for everyone else — would drift on the first edit, so `en` is just
/// another locale file and this table stays a table.
pub static SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        name: "chat.ocr",
        category: Category::Chat,
        fields: &[
            f("chat.ocr.enabled", Kind::Bool),
            f("chat.ocr.model", Kind::Model(PoolKind::Ocr)),
            f("chat.ocr.max_tokens", Kind::Int),
            f("chat.ocr.ngram_window", Kind::Int),
            f("chat.ocr.max_bytes", Kind::Int),
            f("chat.ocr.max_pages", Kind::Int),
            f("chat.ocr.dpi", Kind::Int),
            f("chat.ocr.max_output_chars", Kind::Int),
            f("chat.ocr.timeout_secs", Kind::Int),
            f("chat.ocr.max_concurrency", Kind::Int),
            f("chat.ocr.auto_min_text_chars_per_page", Kind::Int),
        ],
    },
    SectionSpec {
        name: "chat.compaction",
        category: Category::Chat,
        fields: &[
            f("chat.compaction.enabled", Kind::Bool),
            f("chat.compaction.default_context_window", Kind::Int),
            f("chat.compaction.trigger_ratio", Kind::Float),
            f("chat.compaction.keep_recent_turns", Kind::Int),
            f("chat.compaction.min_turns_to_compact", Kind::Int),
            f("chat.compaction.summary_max_tokens", Kind::Int),
        ],
    },
    SectionSpec {
        name: "chat.s3",
        category: Category::Data,
        fields: &[
            f("chat.s3.enabled", Kind::Bool),
            f("chat.s3.endpoint", Kind::Text),
            f_half("chat.s3.region", Kind::Text),
            f_half("chat.s3.bucket", Kind::Text),
            f_half("chat.s3.key_prefix", Kind::Text),
            f("chat.s3.access_key", Kind::Secret),
            f("chat.s3.secret_key", Kind::Secret),
        ],
    },
    SectionSpec {
        name: "sandbox",
        category: Category::Tools,
        fields: &[
            f("sandbox.enabled", Kind::Bool),
            f("sandbox.runner_url", Kind::Text),
            f("sandbox.timeout_secs", Kind::Int),
            f("sandbox.max_artifact_bytes", Kind::Int),
        ],
    },
    SectionSpec {
        name: "comfyui",
        category: Category::Tools,
        fields: &[
            f("comfyui.enabled", Kind::Bool),
            // Restart-only: the job scheduler polling this URL is a running
            // task, and swapping the URL under it would abandon jobs it has
            // already queued.
            r("comfyui.base_url", Kind::Text),
            // Restart-only for the same reason; /admin/comfyui has a reload
            // button for the common case of adding a workflow.
            r("comfyui.content_dir", Kind::Path),
            f("comfyui.timeout_secs", Kind::Int),
            f("comfyui.queue_poll_interval_ms", Kind::Int),
            f("comfyui.max_concurrent_jobs", Kind::Int),
        ],
    },
    SectionSpec {
        name: "rag",
        category: Category::Data,
        fields: &[
            // Restart-only: stopping the indexer mid-pass could leave a
            // half-written index.
            r("rag.enabled", Kind::Bool),
            // Restart-only twice over: the indexer is a running task, and
            // existing indexes do not move with the directory, so a hot swap
            // would silently orphan every one of them.
            r("rag.data_dir", Kind::Path),
            // Restart-only: the worker pool is sized once, when the indexer
            // starts.
            r("rag.clone_concurrency", Kind::Int),
        ],
    },
    SectionSpec {
        name: "skills",
        category: Category::Data,
        fields: &[f("skills.enabled", Kind::Bool), f("skills.dir", Kind::Path)],
    },
    SectionSpec {
        name: "typst",
        category: Category::Tools,
        fields: &[
            f("typst.enabled", Kind::Bool),
            f("typst.templates_dir", Kind::Path),
        ],
    },
    SectionSpec {
        name: "geoip",
        category: Category::Tools,
        fields: &[
            f("geoip.enabled", Kind::Bool),
            f("geoip.db_path", Kind::Path),
            f("geoip.update_token", Kind::Secret),
        ],
    },
    SectionSpec {
        name: "usage",
        category: Category::Access,
        fields: &[
            f("usage.enabled", Kind::Bool),
            f("usage.retention_days", Kind::Int),
            f_half("usage.currency", Kind::Text),
        ],
    },
    SectionSpec {
        name: "limits",
        category: Category::Access,
        fields: &[f("limits.enabled", Kind::Bool)],
    },
    SectionSpec {
        name: "feedback",
        category: Category::Notifications,
        fields: &[
            f("feedback.enabled", Kind::Bool),
            f_half("feedback.github_owner", Kind::Text),
            f_half("feedback.github_repo", Kind::Text),
            f("feedback.github_token", Kind::Secret),
            f("feedback.github_api_base", Kind::Text),
            f("feedback.labels", Kind::List),
            f_half("feedback.assets_branch", Kind::Text),
            f("feedback.extraction_model", Kind::Model(PoolKind::Chat)),
            f("feedback.voice_model", Kind::Model(PoolKind::Transcription)),
        ],
    },
    SectionSpec {
        name: "push",
        category: Category::Notifications,
        fields: &[f("push.enabled", Kind::Bool), f("push.contact", Kind::Text)],
    },
    SectionSpec {
        name: "gateway",
        category: Category::Access,
        fields: &[
            f("gateway.token_ttl_days", Kind::Int),
            f("gateway.session_ttl_days", Kind::Int),
            f("gateway.session_absolute_max_days", Kind::Int),
            f("gateway.allow_impersonation", Kind::Bool),
        ],
    },
];

/// The `[gateway]` keys that deliberately did **not** move into the database,
/// and why. Named here rather than left implicit, because "why is this one
/// still in the file?" is the obvious question about the section above.
///
/// * `public_url` — owned by the setup wizard (`/setup`), which stores it in
///   its own row. The file's copy is a one-time import; see
///   [`crate::server::setup::import_config_once`].
/// * `bootstrap_admin_groups` — the anti-lockout anchor. It exists so a
///   break-glass admin works *regardless of what is in the database*, which
///   makes the database the one place it must not live: a botched group mapping
///   is exactly the situation it rescues you from. Same reasoning that keeps the
///   listen socket in the environment.
/// * `session_key_env` — vestigial. `$GATEWAY_SESSION_KEY` is read directly and
///   is mandatory, so naming a different variable has no effect; a stale value
///   is reported by [`Config::warn_about_ignored_blocks`].
///
/// [`Config::warn_about_ignored_blocks`]: crate::server::config::Config::warn_about_ignored_blocks
pub const GATEWAY_KEYS_STAYING_IN_THE_FILE: &[&str] =
    &["public_url", "bootstrap_admin_groups", "session_key_env"];

/// Every declared field, flattened.
pub fn all_fields() -> impl Iterator<Item = &'static FieldSpec> {
    SECTIONS.iter().flat_map(|s| s.fields.iter())
}

/// Look up one field's spec.
/// Whether a section's feature is switched on **in the configuration the
/// gateway is actually running**, or `None` for a section that has no master
/// switch.
///
/// Deliberately reads the effective [`Config`] rather than the stored rows.
/// A missing row is not "off": it means the built-in default applies, and for
/// `chat.compaction`, `usage`, `limits` and `push` that default is *on*. The
/// editor asked the rows directly at first, so on a database with no settings
/// rows yet — a fresh install, or a field just cleared — it drew those toggles
/// as off while the gateway was using them. A control that disagrees with the
/// running system is worse than no control.
///
/// Keeping this next to [`apply`] is the point: both answer "what is in force",
/// and a new section shows up in `every_section_with_a_switch_reports_its_state`
/// if this arm is forgotten.
/// The values currently **in force**, as the editor should display them.
///
/// Built from [`snapshot`] — the inverse of [`apply`] — rather than from the
/// stored rows, because a missing row is not an empty value: it means the
/// built-in default applies. Rendering the raw rows drew every toggle on a
/// fresh database as off, including compaction, usage, limits and push, which
/// default to on and were in fact running. A control that disagrees with the
/// running gateway is worse than no control.
///
/// This also means the editor and [`import_once`] agree by construction: both
/// read the same function, so there is no second place for a default to drift.
pub fn effective(config: &Config) -> Settings {
    Settings::from_map(snapshot(config).into_iter().collect())
}

/// Marks that a `restart`-flagged field has been changed and the process has
/// not come back yet.
///
/// A row rather than in-memory state on purpose: the operator who saves the
/// value and the operator who restarts the container are often not the same
/// person, and "is a restart still pending" has to survive the page being
/// closed. Cleared on boot, once the restart has actually happened.
const RESTART_PENDING_KEY: &str = "settings.restart_pending";

/// Record that a saved field only takes effect after a restart.
pub async fn mark_restart_pending(pool: &Pool, fields: &[String]) -> Result<(), DbError> {
    app_settings::set(pool, RESTART_PENDING_KEY, &fields.join(",")).await
}

/// Which `restart`-flagged fields are waiting for one, for the banner the
/// editor shows until the process comes back.
pub async fn restart_pending(pool: &Pool) -> Result<Vec<String>, DbError> {
    Ok(app_settings::get(pool, RESTART_PENDING_KEY)
        .await?
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// Clear the pending-restart marker. Called at boot: the process coming back
/// *is* the restart everything was waiting for.
pub async fn clear_restart_pending(pool: &Pool) -> Result<(), DbError> {
    app_settings::delete(pool, RESTART_PENDING_KEY).await
}

pub fn section_is_enabled(config: &Config, section: &SectionSpec) -> Option<bool> {
    Some(match section.name {
        "chat.ocr" => config.chat.ocr.enabled,
        "chat.compaction" => config.chat.compaction.enabled,
        "chat.s3" => config.chat.s3.is_some(),
        "sandbox" => config.sandbox.is_some(),
        "comfyui" => config.comfyui.is_some(),
        "rag" => config.rag.is_some(),
        "skills" => config.skills.is_some(),
        "typst" => config.typst.is_some(),
        "geoip" => config.geoip.is_some(),
        "usage" => config.usage.enabled,
        "limits" => config.limits.enabled,
        "feedback" => config.feedback.is_some(),
        "push" => config.push.enabled,
        // `[gateway]` is session and token lifetimes — always in force, no
        // master switch to report.
        _ => return None,
    })
}

pub fn field(key: &str) -> Option<&'static FieldSpec> {
    all_fields().find(|f| f.key == key)
}

// ---------------------------------------------------------------------------
// The loaded values

/// Every settings row, unsealed, keyed by TOML path (no [`PREFIX`]).
///
/// Also records which keys [`apply`] read, which is what
/// `every_declared_field_is_actually_applied` checks the declaration against.
#[derive(Debug, Default)]
pub struct Settings {
    values: HashMap<String, String>,
    /// A `Mutex` rather than a `RefCell` so `Settings` stays `Sync`. It is
    /// held across an `.await` in the `/admin/settings` handler, and a
    /// non-`Sync` value there would make that future non-`Send` — a confusing
    /// error to hit later from an unrelated refactor, for no gain: this is
    /// locked a few dozen times per boot.
    read: std::sync::Mutex<HashSet<&'static str>>,
}

impl Settings {
    /// Build from raw key→value pairs. Keys carry no [`PREFIX`].
    pub fn from_map(values: HashMap<String, String>) -> Self {
        Self {
            values,
            read: Default::default(),
        }
    }

    /// The raw stored text, if any. Marks the key read.
    fn raw(&self, key: &'static str) -> Option<&str> {
        // A poisoned lock here would mean a panic inside `apply`, which cannot
        // happen — but losing the record is not worth propagating either, since
        // it only feeds a test.
        if let Ok(mut read) = self.read.lock() {
            read.insert(key);
        }
        self.values.get(key).map(String::as_str)
    }

    /// The text the editor should display for a field. Does **not** mark the
    /// key read — rendering a control is not the same as a config field
    /// consuming it, and conflating them would let the drift test pass on a
    /// field the gateway ignores.
    ///
    /// Refuses to hand back a [`Kind::Secret`] value, whatever the map holds.
    /// [`effective`] builds its map from [`snapshot`], which resolves secrets
    /// to plaintext so the import can seal them — and that same map is what the
    /// editor renders from. One forgetful `value:` in a control would then put
    /// a live credential in the HTML. Blocking it here makes that impossible
    /// rather than merely unlikely; `secret_is_set` still answers the only
    /// question the editor needs.
    pub fn shown(&self, key: &str) -> Option<&str> {
        if field(key).is_some_and(|f| f.kind == Kind::Secret) {
            return None;
        }
        self.values.get(key).map(String::as_str)
    }

    /// Whether a secret is stored, without revealing it.
    pub fn secret_is_set(&self, key: &str) -> bool {
        self.values.get(key).is_some_and(|v| !v.is_empty())
    }

    fn bool(&self, key: &'static str, default: bool) -> bool {
        match self.raw(key) {
            Some(v) => matches!(v.trim(), "true" | "1" | "yes" | "on"),
            None => default,
        }
    }

    /// Parsed as `i64` and converted, because every integer field in the config
    /// is a different width (`usize`, `u32`, `u64`, `i64`) and none of them
    /// wants a separate accessor. Out-of-range and unparseable both fall back
    /// to the default rather than failing the boot: a settings row is operator
    /// input, and a gateway that refuses to start over one bad number is worse
    /// than one that logs it and uses the value it shipped with.
    fn int<T: TryFrom<i64>>(&self, key: &'static str, default: T) -> T {
        match self.raw(key) {
            Some(raw) => match raw
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|v| T::try_from(v).ok())
            {
                Some(v) => v,
                None => {
                    tracing::warn!(
                        "settings.{key} is not a usable number ({raw:?}); using the default"
                    );
                    default
                }
            },
            None => default,
        }
    }

    /// Accepts a comma as the decimal separator as well as a dot.
    ///
    /// A `type="number"` input renders `0.7` as `0,7` for a browser in a German
    /// (or French, or Spanish) locale, and this product ships in six languages.
    /// Chrome normalises the submitted value, but relying on every browser to do
    /// that means a locale-dependent silent fallback to the default — the worst
    /// kind of bug to notice, because the form looks like it saved.
    fn float(&self, key: &'static str, default: f64) -> f64 {
        let Some(raw) = self.raw(key) else {
            return default;
        };
        match raw.trim().replace(',', ".").parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!("settings.{key} is not a number ({raw:?}); using the default");
                default
            }
        }
    }

    /// Trimmed, with empty treated as absent — a cleared form field means "no
    /// value", not "the empty string".
    fn text(&self, key: &'static str) -> Option<String> {
        self.raw(key)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }

    fn text_or(&self, key: &'static str, default: &str) -> String {
        self.text(key).unwrap_or_else(|| default.to_owned())
    }

    fn path_or(&self, key: &'static str, default: PathBuf) -> PathBuf {
        self.text(key).map(PathBuf::from).unwrap_or(default)
    }

    fn list(&self, key: &'static str, default: Vec<String>) -> Vec<String> {
        match self.raw(key) {
            Some(raw) => serde_json::from_str(raw).unwrap_or(default),
            None => default,
        }
    }
}

// ---------------------------------------------------------------------------
// Applying

/// Overwrite the twelve settings-owned blocks of `config` from the database.
///
/// Everything else in `config` — `[bind]`, `[db]`, `[gateway]`, the upstream
/// topology and the seed-only blocks — is left exactly as the file left it.
pub fn apply(settings: &Settings, config: &mut Config) {
    config.chat = ChatConfig {
        ocr: ocr(settings),
        compaction: compaction(settings),
        s3: settings
            .bool("chat.s3.enabled", false)
            .then(|| s3(settings)),
    };
    config.sandbox = settings
        .bool("sandbox.enabled", false)
        .then(|| sandbox(settings));
    config.comfyui = settings
        .bool("comfyui.enabled", false)
        .then(|| comfyui(settings));
    config.rag = settings.bool("rag.enabled", true).then(|| rag(settings));
    config.skills = settings
        .bool("skills.enabled", false)
        .then(|| skills(settings));
    config.typst = settings
        .bool("typst.enabled", false)
        .then(|| typst(settings));
    config.geoip = settings
        .bool("geoip.enabled", false)
        .then(|| geoip(settings));
    config.usage = usage(settings);
    config.limits = LimitsConfig {
        enabled: settings.bool("limits.enabled", true),
    };
    config.feedback = settings
        .bool("feedback.enabled", false)
        .then(|| feedback(settings));
    config.push = push(settings);

    // Field-by-field, not `config.gateway = …` like the blocks above. Two keys
    // in this block are owned by the config file on purpose — the wizard's
    // `public_url` import and the `bootstrap_admin_groups` anti-lockout anchor —
    // and replacing the struct would erase them. See
    // [`GATEWAY_KEYS_STAYING_IN_THE_FILE`].
    // Missing rows fall back to the *built-in* defaults, not to whatever is
    // currently in memory — same as every block above, and what the editor's
    // clear button promises ("the built-in default applies again"). A first
    // boot writes every row from the file before this runs, so the file's
    // values are never lost to this.
    let d = GatewayConfig::default();
    let g = &mut config.gateway;
    g.token_ttl_days = settings.int("gateway.token_ttl_days", d.token_ttl_days);
    g.session_ttl_days = settings.int("gateway.session_ttl_days", d.session_ttl_days);
    g.session_absolute_max_days = settings.int(
        "gateway.session_absolute_max_days",
        d.session_absolute_max_days,
    );
    g.allow_impersonation = settings.bool("gateway.allow_impersonation", d.allow_impersonation);
}

fn ocr(s: &Settings) -> OcrConfig {
    let d = OcrConfig::default();
    OcrConfig {
        enabled: s.bool("chat.ocr.enabled", d.enabled),
        model: s.text("chat.ocr.model"),
        max_tokens: s.int("chat.ocr.max_tokens", d.max_tokens),
        ngram_window: s.int("chat.ocr.ngram_window", d.ngram_window),
        max_bytes: s.int("chat.ocr.max_bytes", d.max_bytes),
        max_pages: s.int("chat.ocr.max_pages", d.max_pages),
        dpi: s.int("chat.ocr.dpi", d.dpi),
        max_output_chars: s.int("chat.ocr.max_output_chars", d.max_output_chars),
        timeout_secs: s.int("chat.ocr.timeout_secs", d.timeout_secs),
        max_concurrency: s.int("chat.ocr.max_concurrency", d.max_concurrency),
        auto_min_text_chars_per_page: s.int(
            "chat.ocr.auto_min_text_chars_per_page",
            d.auto_min_text_chars_per_page,
        ),
    }
}

fn compaction(s: &Settings) -> CompactionConfig {
    let d = CompactionConfig::default();
    CompactionConfig {
        enabled: s.bool("chat.compaction.enabled", d.enabled),
        default_context_window: s.int(
            "chat.compaction.default_context_window",
            d.default_context_window,
        ),
        trigger_ratio: s.float("chat.compaction.trigger_ratio", d.trigger_ratio),
        keep_recent_turns: s.int("chat.compaction.keep_recent_turns", d.keep_recent_turns),
        min_turns_to_compact: s.int(
            "chat.compaction.min_turns_to_compact",
            d.min_turns_to_compact,
        ),
        summary_max_tokens: s.int("chat.compaction.summary_max_tokens", d.summary_max_tokens),
    }
}

fn s3(s: &Settings) -> S3Config {
    S3Config {
        endpoint: s.text_or("chat.s3.endpoint", ""),
        region: s.text_or("chat.s3.region", ""),
        bucket: s.text_or("chat.s3.bucket", ""),
        key_prefix: s.text_or("chat.s3.key_prefix", "chat-attachments"),
        access_key: s.text("chat.s3.access_key"),
        secret_key: s.text("chat.s3.secret_key"),
        // See `feedback()`: the sealed row replaces the env-var indirection.
        access_key_env: None,
        secret_key_env: None,
    }
}

fn sandbox(s: &Settings) -> SandboxConfig {
    SandboxConfig {
        enabled: true,
        runner_url: s.text_or("sandbox.runner_url", ""),
        timeout_secs: s.int("sandbox.timeout_secs", 120),
        max_artifact_bytes: s.int("sandbox.max_artifact_bytes", 25 * 1024 * 1024),
    }
}

fn comfyui(s: &Settings) -> ComfyuiConfig {
    ComfyuiConfig {
        enabled: true,
        base_url: s.text_or("comfyui.base_url", ""),
        content_dir: s.path_or(
            "comfyui.content_dir",
            PathBuf::from("data/comfyui-workflows"),
        ),
        timeout_secs: s.int("comfyui.timeout_secs", 600),
        queue_poll_interval_ms: s.int("comfyui.queue_poll_interval_ms", 500),
        max_concurrent_jobs: s.int("comfyui.max_concurrent_jobs", 1),
    }
}

fn rag(s: &Settings) -> RagConfig {
    let d = RagConfig::default();
    RagConfig {
        data_dir: s.path_or("rag.data_dir", d.data_dir),
        clone_concurrency: s.int("rag.clone_concurrency", d.clone_concurrency),
    }
}

fn skills(s: &Settings) -> SkillsConfig {
    SkillsConfig {
        dir: s.path_or("skills.dir", SkillsConfig::default().dir),
    }
}

fn typst(s: &Settings) -> TypstConfig {
    TypstConfig {
        templates_dir: s.path_or("typst.templates_dir", PathBuf::from("data/typst-templates")),
    }
}

fn geoip(s: &Settings) -> GeoipConfig {
    GeoipConfig {
        db_path: s.path_or(
            "geoip.db_path",
            PathBuf::from("data/ip2location/IP2LOCATION-LITE-DB11.BIN"),
        ),
        update_token: s.text("geoip.update_token"),
        // See `feedback()`: the sealed row replaces the env-var indirection.
        update_token_env: None,
    }
}

fn usage(s: &Settings) -> UsageConfig {
    let d = UsageConfig::default();
    UsageConfig {
        enabled: s.bool("usage.enabled", d.enabled),
        retention_days: s.int("usage.retention_days", d.retention_days),
        currency: s.text_or("usage.currency", &d.currency),
    }
}

fn feedback(s: &Settings) -> FeedbackConfig {
    FeedbackConfig {
        github_owner: s.text_or("feedback.github_owner", ""),
        github_repo: s.text_or("feedback.github_repo", ""),
        github_token: s.text("feedback.github_token"),
        // Legacy env-var indirection. The database holds the token sealed, so
        // nothing new writes this; it survives only for a deployment whose
        // config file still names a variable, and `github_token()` prefers the
        // direct value anyway.
        github_token_env: None,
        github_api_base: s.text_or("feedback.github_api_base", "https://api.github.com"),
        labels: s.list("feedback.labels", vec!["feedback".to_string()]),
        assets_branch: s.text_or("feedback.assets_branch", "feedback-assets"),
        extraction_model: s.text("feedback.extraction_model"),
        voice_model: s.text("feedback.voice_model"),
    }
}

fn push(s: &Settings) -> PushConfig {
    let d = PushConfig::default();
    PushConfig {
        enabled: s.bool("push.enabled", d.enabled),
        contact: s.text_or("push.contact", &d.contact),
    }
}

// ---------------------------------------------------------------------------
// Storage

/// Read every settings row, unsealing the secrets.
pub async fn load(pool: &Pool, crypto: &Crypto) -> Result<Settings, DbError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM app_settings WHERE key LIKE ?")
            .bind(format!("{PREFIX}%"))
            .fetch_all(pool)
            .await?;

    let mut values = HashMap::with_capacity(rows.len());
    for (key, value) in rows {
        let Some(key) = key.strip_prefix(PREFIX) else {
            continue;
        };
        // The import marker shares the namespace but is not a field.
        if field(key).is_none() {
            continue;
        }
        let value = match field(key).map(|f| f.kind) {
            Some(Kind::Secret) => match crypto.open_from_string(&value) {
                Some(plain) => plain,
                None => {
                    // Almost always the at-rest key changing. Treat it as unset
                    // — the editor then shows "not set" and the operator can
                    // re-enter it — but say so, because a silently vanished
                    // credential is a bad morning.
                    tracing::error!(
                        "settings.{key} could not be decrypted (GATEWAY_ENCRYPTION_KEY or \
                         GATEWAY_SESSION_KEY changed?); treating it as unset"
                    );
                    continue;
                }
            },
            _ => value,
        };
        values.insert(key.to_owned(), value);
    }
    Ok(Settings::from_map(values))
}

/// Write field values, sealing the secrets. Keys carry no [`PREFIX`], and one
/// that names no declared field is refused rather than stored — an unknown key
/// can only be a typo or a stale form, and a row nothing reads is invisible.
pub async fn store(
    pool: &Pool,
    crypto: &Crypto,
    pairs: &[(String, String)],
) -> Result<(), DbError> {
    for (key, value) in pairs {
        let Some(spec) = field(key) else {
            tracing::warn!("refusing to store unknown setting {key:?}");
            continue;
        };
        let row_key = format!("{PREFIX}{key}");
        if value.is_empty() && spec.kind == Kind::Secret {
            // An empty secret submission means "leave it alone" — the editor
            // never renders the stored value, so it cannot round-trip one. It
            // is the clear button that deletes.
            continue;
        }
        let stored = match spec.kind {
            Kind::Secret => crypto.seal_to_string(value)?,
            _ => value.clone(),
        };
        app_settings::set(pool, &row_key, &stored).await?;
    }
    Ok(())
}

/// Delete one field's row, so it falls back to its built-in default.
pub async fn clear(pool: &Pool, key: &str) -> Result<(), DbError> {
    app_settings::delete(pool, &format!("{PREFIX}{key}")).await
}

/// Copy the settings blocks out of a config file into empty settings, once.
///
/// This is what upgrades an existing file-driven deployment in place: on the
/// first boot after this release its `[sandbox]`, `[comfyui]`, `[chat]` and the
/// rest move into the database, and from then on `/admin/settings` owns them
/// and the file's copies are ignored. A fresh install imports the defaults, so
/// the editor opens on real rows rather than on emptiness.
///
/// # The missing-file case
///
/// "There is no config file" and "the config file was not mounted on this boot"
/// arrive here as the same [`Config`] full of defaults, and importing defaults
/// is right for the first and destructive for the second: it would burn the
/// marker and leave a running deployment's real `[sandbox]`, `[chat.s3]` and
/// `[comfyui]` settings sitting in a file that is never read again.
///
/// So a missing file is only trusted on a database nobody has used yet. On a
/// database with users or groups in it, this imports nothing and leaves the
/// marker unset, so the next boot that *does* see the file imports properly.
/// Nothing is lost in the meantime — with no rows, every field falls back to
/// the same built-in default the code shipped with.
///
/// The window that leaves open — an operator editing `/admin/settings` while
/// the marker is unset, then the file reappearing and overwriting them — is
/// closed by [`mark_imported`], which the editor calls on every save.
///
/// Returns whether anything was written, for logging.
pub async fn import_once(pool: &Pool, crypto: &Crypto, config: &Config) -> Result<bool, DbError> {
    if app_settings::get(pool, IMPORT_MARKER_KEY).await?.is_some() {
        return Ok(false);
    }
    if config.loaded_from.is_none() && has_been_used(pool).await? {
        tracing::warn!(
            "no config file was found, but this database already has users — so this looks \
             like an existing deployment whose config file is missing rather than a fresh \
             install. Not importing anything: every setting keeps its built-in default for \
             now, and the next start that finds the file will import it. Configure them at \
             /admin/settings to settle it either way."
        );
        return Ok(false);
    }
    let pairs = snapshot(config);
    store(pool, crypto, &pairs).await?;
    mark_imported(pool).await?;
    Ok(true)
}

/// Record that the config file is no longer authoritative for these settings.
///
/// Called by [`import_once`] and by every save in the editor. The second is
/// what makes a human editing a value final: after that, a config file
/// appearing (or reappearing) on a later boot cannot overwrite what they chose.
pub async fn mark_imported(pool: &Pool) -> Result<(), DbError> {
    app_settings::set(pool, IMPORT_MARKER_KEY, "1").await
}

/// Has this database ever been used? True as soon as one person has signed in
/// or one gateway group exists.
///
/// Same question, and the same phrasing of it, as the guard in
/// [`crate::server::setup`]: "is anybody here?" rather than anything about what
/// the config file happens to contain.
async fn has_been_used(pool: &Pool) -> Result<bool, DbError> {
    let used: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users) OR EXISTS(SELECT 1 FROM gateway_groups)",
    )
    .fetch_one(pool)
    .await?;
    Ok(used != 0)
}

/// Every field's current value, as text, taken from `config`.
///
/// The inverse of [`apply`], and only used by [`import_once`]. Written as a
/// flat match on the key rather than as twelve serializers so that the
/// declaration in [`SECTIONS`] stays the only list of fields — an entry with no
/// arm here fails `every_declared_field_can_be_imported`.
fn snapshot(c: &Config) -> Vec<(String, String)> {
    let ocr = &c.chat.ocr;
    let comp = &c.chat.compaction;
    let s3 = c.chat.s3.as_ref();
    let sb = c.sandbox.as_ref();
    let cf = c.comfyui.as_ref();
    let rag = c.rag.as_ref();
    let sk = c.skills.as_ref();
    let ty = c.typst.as_ref();
    let gi = c.geoip.as_ref();
    let fb = c.feedback.as_ref();

    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |key: &str, value: String| out.push((key.to_owned(), value));

    put("chat.ocr.enabled", ocr.enabled.to_string());
    put("chat.ocr.model", ocr.model.clone().unwrap_or_default());
    put("chat.ocr.max_tokens", ocr.max_tokens.to_string());
    put("chat.ocr.ngram_window", ocr.ngram_window.to_string());
    put("chat.ocr.max_bytes", ocr.max_bytes.to_string());
    put("chat.ocr.max_pages", ocr.max_pages.to_string());
    put("chat.ocr.dpi", ocr.dpi.to_string());
    put(
        "chat.ocr.max_output_chars",
        ocr.max_output_chars.to_string(),
    );
    put("chat.ocr.timeout_secs", ocr.timeout_secs.to_string());
    put("chat.ocr.max_concurrency", ocr.max_concurrency.to_string());
    put(
        "chat.ocr.auto_min_text_chars_per_page",
        ocr.auto_min_text_chars_per_page.to_string(),
    );

    put("chat.compaction.enabled", comp.enabled.to_string());
    put(
        "chat.compaction.default_context_window",
        comp.default_context_window.to_string(),
    );
    put(
        "chat.compaction.trigger_ratio",
        comp.trigger_ratio.to_string(),
    );
    put(
        "chat.compaction.keep_recent_turns",
        comp.keep_recent_turns.to_string(),
    );
    put(
        "chat.compaction.min_turns_to_compact",
        comp.min_turns_to_compact.to_string(),
    );
    put(
        "chat.compaction.summary_max_tokens",
        comp.summary_max_tokens.to_string(),
    );

    put("chat.s3.enabled", s3.is_some().to_string());
    put("chat.s3.endpoint", opt(s3.map(|v| v.endpoint.clone())));
    put("chat.s3.region", opt(s3.map(|v| v.region.clone())));
    put("chat.s3.bucket", opt(s3.map(|v| v.bucket.clone())));
    put("chat.s3.key_prefix", opt(s3.map(|v| v.key_prefix.clone())));
    put("chat.s3.access_key", opt(s3.and_then(|v| v.access_key())));
    put("chat.s3.secret_key", opt(s3.and_then(|v| v.secret_key())));

    put("sandbox.enabled", sb.is_some_and(|v| v.enabled).to_string());
    put("sandbox.runner_url", opt(sb.map(|v| v.runner_url.clone())));
    put("sandbox.timeout_secs", num(sb.map(|v| v.timeout_secs), 120));
    put(
        "sandbox.max_artifact_bytes",
        num(sb.map(|v| v.max_artifact_bytes), 25 * 1024 * 1024),
    );

    put("comfyui.enabled", cf.is_some_and(|v| v.enabled).to_string());
    put("comfyui.base_url", opt(cf.map(|v| v.base_url.clone())));
    put(
        "comfyui.content_dir",
        path(cf.map(|v| v.content_dir.clone())),
    );
    put("comfyui.timeout_secs", num(cf.map(|v| v.timeout_secs), 600));
    put(
        "comfyui.queue_poll_interval_ms",
        num(cf.map(|v| v.queue_poll_interval_ms), 500),
    );
    put(
        "comfyui.max_concurrent_jobs",
        num(cf.map(|v| v.max_concurrent_jobs as u64), 1),
    );

    // Absent `[rag]` has always meant "run with defaults", not "off".
    put("rag.enabled", "true".into());
    let rag_default = RagConfig::default();
    put(
        "rag.data_dir",
        path(Some(
            rag.map(|v| v.data_dir.clone())
                .unwrap_or(rag_default.data_dir),
        )),
    );
    put(
        "rag.clone_concurrency",
        num(
            Some(
                rag.map(|v| v.clone_concurrency)
                    .unwrap_or(rag_default.clone_concurrency) as u64,
            ),
            4,
        ),
    );

    put("skills.enabled", sk.is_some().to_string());
    put(
        "skills.dir",
        path(Some(
            sk.map(|v| v.dir.clone())
                .unwrap_or(SkillsConfig::default().dir),
        )),
    );

    put("typst.enabled", ty.is_some().to_string());
    put(
        "typst.templates_dir",
        path(ty.map(|v| v.templates_dir.clone())),
    );

    put("geoip.enabled", gi.is_some().to_string());
    put("geoip.db_path", path(gi.map(|v| v.db_path.clone())));
    put("geoip.update_token", opt(gi.and_then(|v| v.update_token())));

    put("usage.enabled", c.usage.enabled.to_string());
    put("usage.retention_days", c.usage.retention_days.to_string());
    put("usage.currency", c.usage.currency.clone());

    put("limits.enabled", c.limits.enabled.to_string());

    put("feedback.enabled", fb.is_some().to_string());
    put(
        "feedback.github_owner",
        opt(fb.map(|v| v.github_owner.clone())),
    );
    put(
        "feedback.github_repo",
        opt(fb.map(|v| v.github_repo.clone())),
    );
    put(
        "feedback.github_token",
        opt(fb.and_then(|v| v.github_token())),
    );
    put(
        "feedback.github_api_base",
        opt(fb.map(|v| v.github_api_base.clone())),
    );
    put(
        "feedback.labels",
        serde_json::to_string(
            &fb.map(|v| v.labels.clone())
                .unwrap_or_else(|| vec!["feedback".to_string()]),
        )
        .unwrap_or_else(|_| "[]".into()),
    );
    put(
        "feedback.assets_branch",
        opt(fb.map(|v| v.assets_branch.clone())),
    );
    put(
        "feedback.extraction_model",
        opt(fb.and_then(|v| v.extraction_model.clone())),
    );
    put(
        "feedback.voice_model",
        opt(fb.and_then(|v| v.voice_model.clone())),
    );

    put("push.enabled", c.push.enabled.to_string());
    put("push.contact", c.push.contact.clone());

    put(
        "gateway.token_ttl_days",
        c.gateway.token_ttl_days.to_string(),
    );
    put(
        "gateway.session_ttl_days",
        c.gateway.session_ttl_days.to_string(),
    );
    put(
        "gateway.session_absolute_max_days",
        c.gateway.session_absolute_max_days.to_string(),
    );
    put(
        "gateway.allow_impersonation",
        c.gateway.allow_impersonation.to_string(),
    );

    out
}

fn opt(v: Option<String>) -> String {
    v.unwrap_or_default()
}

fn num(v: Option<u64>, default: u64) -> String {
    v.unwrap_or(default).to_string()
}

fn path(v: Option<PathBuf>) -> String {
    v.map(|p| p.display().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_of(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn every_declared_field_is_actually_applied() {
        // The drift guard this module is built around. `SECTIONS` (what the
        // editor shows) and `apply` (what the gateway reads) are two spellings
        // of one list, and a field in only one of them is silently useless: a
        // control that saves a row nobody reads, or a row nobody can set.
        // Every optional block turned on, because `apply` only builds a
        // block's fields when its `enabled` flag says to — with everything off
        // the guard would report the whole of `[sandbox]`, `[comfyui]`,
        // `[feedback]` and friends as unread and prove nothing.
        let settings = Settings::from_map(
            all_fields()
                .filter(|f| f.key.ends_with(".enabled"))
                .map(|f| (f.key.to_owned(), "true".to_owned()))
                .collect(),
        );
        let mut config = Config::default();
        apply(&settings, &mut config);

        let declared: HashSet<&str> = all_fields().map(|f| f.key).collect();
        let read: HashSet<&str> = settings.read.lock().unwrap().iter().copied().collect();

        let unread: Vec<_> = declared.difference(&read).collect();
        assert!(
            unread.is_empty(),
            "declared in SECTIONS but never read by apply(): {unread:?}"
        );
        let undeclared: Vec<_> = read.difference(&declared).collect();
        assert!(
            undeclared.is_empty(),
            "read by apply() but not declared in SECTIONS: {undeclared:?}"
        );
    }

    #[test]
    fn every_declared_field_can_be_imported() {
        // The other half of the same guard: `snapshot` is what carries an
        // existing config file into the database, and a field it forgets is a
        // setting that silently reverts to its default on upgrade.
        let declared: HashSet<&str> = all_fields().map(|f| f.key).collect();
        let snapshotted: HashSet<String> = snapshot(&Config::default())
            .into_iter()
            .map(|(k, _)| k)
            .collect();

        let missing: Vec<_> = declared
            .iter()
            .filter(|k| !snapshotted.contains(**k))
            .collect();
        assert!(
            missing.is_empty(),
            "declared but never imported: {missing:?}"
        );

        let extra: Vec<_> = snapshotted
            .iter()
            .filter(|k| !declared.contains(k.as_str()))
            .collect();
        assert!(extra.is_empty(), "imported but not declared: {extra:?}");
    }

    #[test]
    fn every_section_appears_on_exactly_one_tab() {
        // The editor renders one category at a time, so a card whose category
        // is not in `Category::ALL` is a card no operator can reach — invisible
        // to every other test here, which all iterate `SECTIONS` directly.
        let on_a_tab: Vec<&str> = Category::ALL
            .iter()
            .flat_map(|c| c.sections())
            .map(|s| s.name)
            .collect();

        for section in SECTIONS {
            let times = on_a_tab.iter().filter(|n| **n == section.name).count();
            assert_eq!(
                times, 1,
                "{} appears on {times} tabs; it must appear on exactly one",
                section.name
            );
        }
        assert_eq!(
            on_a_tab.len(),
            SECTIONS.len(),
            "a tab lists a section that is not in SECTIONS"
        );
    }

    #[test]
    fn tab_slugs_round_trip_and_reject_nonsense() {
        for c in Category::ALL {
            assert_eq!(Category::from_slug(c.slug()), Some(*c));
        }
        assert_eq!(Category::from_slug("no-such-tab"), None);
        assert_eq!(Category::from_slug(""), None);
    }

    #[test]
    fn every_section_with_a_switch_reports_its_state() {
        // `section_is_enabled` is the third spelling of the same list (after
        // `SECTIONS` and `apply`), and the one the UI draws its toggles from.
        // A section with an `enabled` field but no arm here would render as
        // permanently off while the gateway happily used the feature.
        let config = Config::default();
        for section in SECTIONS {
            let has_switch = section
                .fields
                .first()
                .is_some_and(|f| f.key == format!("{}.enabled", section.name));
            assert_eq!(
                section_is_enabled(&config, section).is_some(),
                has_switch,
                "{} has a master switch: {has_switch}, but section_is_enabled disagrees",
                section.name
            );
        }
    }

    #[test]
    fn a_missing_row_reports_the_built_in_default_not_off() {
        // The regression: the editor used to read the stored rows, so on a
        // database with no settings rows every toggle drew as off — including
        // compaction, usage, limits and push, which default to *on* and were
        // in fact running.
        let mut config = Config::default();
        apply(&Settings::default(), &mut config);
        let by_name = |n: &str| SECTIONS.iter().find(|s| s.name == n).unwrap();

        for on_by_default in ["chat.compaction", "usage", "limits", "push"] {
            assert_eq!(
                section_is_enabled(&config, by_name(on_by_default)),
                Some(true),
                "{on_by_default} defaults to on and must report on"
            );
        }
        for off_by_default in ["chat.ocr", "sandbox", "comfyui", "chat.s3"] {
            assert_eq!(
                section_is_enabled(&config, by_name(off_by_default)),
                Some(false),
                "{off_by_default} defaults to off"
            );
        }
    }

    #[test]
    fn no_tab_is_empty() {
        // An empty tab renders as a heading over nothing, which reads as a bug.
        for c in Category::ALL {
            assert!(c.sections().next().is_some(), "{:?} has no sections", c);
        }
    }

    #[test]
    fn field_keys_are_unique_and_live_under_their_section() {
        let mut seen = HashSet::new();
        for section in SECTIONS {
            for field in section.fields {
                assert!(seen.insert(field.key), "duplicate field {}", field.key);
                assert!(
                    field.key.starts_with(&format!("{}.", section.name)),
                    "{} does not belong to section {}",
                    field.key,
                    section.name
                );
            }
        }
    }

    #[test]
    fn an_empty_database_reproduces_the_built_in_defaults() {
        // A fresh gateway with no settings rows must behave exactly like one
        // running on `Config::default()` — otherwise this module changes the
        // product's behaviour rather than just moving where it is configured.
        let mut applied = Config::default();
        apply(&Settings::default(), &mut applied);
        let plain = Config::default();

        assert_eq!(applied.chat.ocr.dpi, plain.chat.ocr.dpi);
        assert_eq!(applied.chat.ocr.enabled, plain.chat.ocr.enabled);
        assert_eq!(
            applied.chat.compaction.enabled,
            plain.chat.compaction.enabled
        );
        assert_eq!(
            applied.chat.compaction.trigger_ratio,
            plain.chat.compaction.trigger_ratio
        );
        assert_eq!(applied.usage.retention_days, plain.usage.retention_days);
        assert_eq!(applied.push.enabled, plain.push.enabled);
        assert_eq!(applied.limits.enabled, plain.limits.enabled);
        assert!(applied.sandbox.is_none(), "no sandbox unless configured");
        assert!(applied.comfyui.is_none());
        assert!(applied.chat.s3.is_none());
        assert!(
            applied.rag.is_some(),
            "absent [rag] has always meant defaults"
        );
    }

    #[test]
    fn values_override_the_defaults() {
        let settings = settings_of(&[
            ("chat.ocr.enabled", "true"),
            ("chat.ocr.dpi", "150"),
            ("chat.compaction.trigger_ratio", "0.9"),
            ("sandbox.enabled", "true"),
            ("sandbox.runner_url", "http://runner:9000"),
            ("sandbox.timeout_secs", "45"),
            ("usage.currency", "EUR"),
            ("feedback.enabled", "true"),
            ("feedback.labels", r#"["bug","ui"]"#),
        ]);
        let mut config = Config::default();
        apply(&settings, &mut config);

        assert!(config.chat.ocr.enabled);
        assert_eq!(config.chat.ocr.dpi, 150);
        assert_eq!(config.chat.compaction.trigger_ratio, 0.9);
        let sandbox = config.sandbox.expect("enabled");
        assert_eq!(sandbox.runner_url, "http://runner:9000");
        assert_eq!(sandbox.timeout_secs, 45);
        assert_eq!(config.usage.currency, "EUR");
        assert_eq!(
            config.feedback.expect("enabled").labels,
            vec!["bug".to_string(), "ui".to_string()]
        );
    }

    #[test]
    fn a_disabled_block_is_absent_no_matter_what_else_is_stored() {
        // The `enabled` flag replaces "leave the URL blank to turn it off". A
        // filled-in but disabled block must not register its tools.
        let settings = settings_of(&[
            ("sandbox.enabled", "false"),
            ("sandbox.runner_url", "http://runner:9000"),
            ("comfyui.enabled", "false"),
            ("comfyui.base_url", "http://comfy:8188"),
        ]);
        let mut config = Config::default();
        apply(&settings, &mut config);
        assert!(config.sandbox.is_none());
        assert!(config.comfyui.is_none());
    }

    #[test]
    fn a_decimal_comma_is_accepted() {
        // A `type="number"` input in a German locale shows 0.7 as "0,7", and
        // this product ships in six languages. Rejecting the comma would mean a
        // locale-dependent silent fallback to the default on a form that looked
        // like it saved.
        let mut config = Config::default();
        apply(
            &settings_of(&[("chat.compaction.trigger_ratio", "0,85")]),
            &mut config,
        );
        assert_eq!(config.chat.compaction.trigger_ratio, 0.85);
    }

    #[test]
    fn a_bad_number_falls_back_instead_of_killing_the_boot() {
        let settings = settings_of(&[
            ("chat.ocr.dpi", "not a number"),
            // Negative into a `usize` — parses as i64, then fails the
            // conversion, which is the case a plain `.parse::<usize>()` would
            // have handled differently.
            ("chat.ocr.max_pages", "-4"),
        ]);
        let mut config = Config::default();
        apply(&settings, &mut config);
        assert_eq!(config.chat.ocr.dpi, OcrConfig::default().dpi);
        assert_eq!(config.chat.ocr.max_pages, OcrConfig::default().max_pages);
    }

    #[test]
    fn a_cleared_text_field_reads_as_unset_not_as_empty() {
        let settings = settings_of(&[("chat.ocr.model", "   "), ("usage.currency", "")]);
        let mut config = Config::default();
        apply(&settings, &mut config);
        assert_eq!(config.chat.ocr.model, None);
        assert_eq!(
            config.usage.currency,
            UsageConfig::default().currency,
            "an empty value means 'no value', so the default applies"
        );
    }

    #[test]
    fn applying_settings_never_touches_the_file_owned_gateway_keys() {
        // `apply` replaces whole blocks, but `[gateway]` is shared: four keys
        // moved into the database and two did not. `bootstrap_admin_groups` is
        // the anti-lockout anchor — it exists so a break-glass admin works
        // regardless of what the database says, which is precisely why erasing
        // it from a database read would be the worst possible bug here.
        // `public_url` is owned by the setup wizard.
        let mut config = Config::default();
        config.gateway.bootstrap_admin_groups = vec!["ops-oncall".into()];
        config.gateway.public_url_import_only = "https://gw.example.com".into();

        apply(
            &settings_of(&[("gateway.token_ttl_days", "7")]),
            &mut config,
        );

        assert_eq!(
            config.gateway.bootstrap_admin_groups,
            vec!["ops-oncall".to_string()],
            "the break-glass admin list must survive a settings reload"
        );
        assert_eq!(
            config.gateway.public_url_import_only,
            "https://gw.example.com"
        );
        assert_eq!(config.gateway.token_ttl_days, 7, "and the move still works");
    }

    #[test]
    fn the_gateway_keys_left_in_the_file_are_not_also_declared() {
        // The two lists must not overlap: a key that is both file-owned and
        // editable at /admin/settings has two live sources, and the file's copy
        // would silently lose on every boot.
        for key in GATEWAY_KEYS_STAYING_IN_THE_FILE {
            let full = format!("gateway.{key}");
            assert!(
                field(&full).is_none(),
                "{full} is documented as file-owned but is also a settings field"
            );
        }
    }

    #[test]
    fn session_and_token_lifetimes_come_from_the_database() {
        let mut config = Config::default();
        apply(
            &settings_of(&[
                ("gateway.token_ttl_days", "30"),
                ("gateway.session_ttl_days", "3"),
                ("gateway.session_absolute_max_days", "14"),
                ("gateway.allow_impersonation", "true"),
            ]),
            &mut config,
        );
        assert_eq!(config.gateway.token_ttl_days, 30);
        assert_eq!(config.gateway.session_ttl_days, 3);
        assert_eq!(config.gateway.session_absolute_max_days, 14);
        assert!(config.gateway.allow_impersonation);
    }

    // ---- import_once ----------------------------------------------------

    async fn fresh_db() -> Pool {
        crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    fn crypto() -> Crypto {
        Crypto::from_key([9u8; 32])
    }

    /// A config as it would arrive from an actual file, with one setting that
    /// differs from the built-in default so an import is observable.
    fn config_from_a_file() -> Config {
        let mut c = Config {
            loaded_from: Some(PathBuf::from("/etc/gateway/config.toml")),
            ..Default::default()
        };
        c.chat.ocr.dpi = 111;
        c
    }

    async fn seed_a_user(pool: &Pool) {
        let now = jiff::Timestamp::now();
        crate::server::db::users::upsert(
            pool,
            &crate::server::db::users::User {
                id: "someone".into(),
                email: "someone@example.com".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
                speech_voice: None,
            },
        )
        .await
        .unwrap();
    }

    async fn stored_dpi(pool: &Pool, crypto: &Crypto) -> Option<String> {
        load(pool, crypto)
            .await
            .unwrap()
            .shown("chat.ocr.dpi")
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn an_existing_config_file_is_imported_once_and_then_ignored() {
        // The upgrade path: the file's values move into the database on the
        // first boot after this release, and later edits to the file do not
        // resurrect themselves over what the operator has since chosen.
        let pool = fresh_db().await;
        let c = crypto();
        let config = config_from_a_file();

        assert!(import_once(&pool, &c, &config).await.unwrap());
        assert_eq!(stored_dpi(&pool, &c).await.as_deref(), Some("111"));

        // Operator changes it in the UI...
        store(&pool, &c, &[("chat.ocr.dpi".into(), "222".into())])
            .await
            .unwrap();
        // ...and a second boot, file unchanged, must not undo that.
        assert!(!import_once(&pool, &c, &config).await.unwrap());
        assert_eq!(stored_dpi(&pool, &c).await.as_deref(), Some("222"));
    }

    #[tokio::test]
    async fn a_genuinely_fresh_install_imports_the_defaults() {
        // No file and an empty database: the editor should open on real rows,
        // not on emptiness, so the defaults are written and the marker burned.
        let pool = fresh_db().await;
        let c = crypto();
        assert!(import_once(&pool, &c, &Config::default()).await.unwrap());
        assert_eq!(
            stored_dpi(&pool, &c).await.as_deref(),
            Some(OcrConfig::default().dpi.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn a_missing_config_file_on_a_used_database_imports_nothing() {
        // The regression this pins: an existing deployment booting with its
        // config bind-mount absent looks exactly like a fresh install. Writing
        // defaults and burning the marker there would strand its real
        // `[sandbox]` / `[chat.s3]` / `[comfyui]` settings in a file that is
        // never read again.
        let pool = fresh_db().await;
        let c = crypto();
        seed_a_user(&pool).await;

        assert!(
            !import_once(&pool, &c, &Config::default()).await.unwrap(),
            "nothing may be imported from a file that is not there"
        );
        assert!(
            stored_dpi(&pool, &c).await.is_none(),
            "and no rows written, so every field keeps its built-in default"
        );

        // The file comes back on a later boot; now it imports properly.
        assert!(import_once(&pool, &c, &config_from_a_file()).await.unwrap());
        assert_eq!(stored_dpi(&pool, &c).await.as_deref(), Some("111"));
    }

    #[tokio::test]
    async fn an_operator_edit_makes_the_config_file_non_authoritative() {
        // Closes the window the previous test leaves open: while the marker is
        // unset, someone configures the gateway at /admin/settings. A file
        // appearing afterwards must not overwrite them.
        let pool = fresh_db().await;
        let c = crypto();
        seed_a_user(&pool).await;
        assert!(!import_once(&pool, &c, &Config::default()).await.unwrap());

        store(&pool, &c, &[("chat.ocr.dpi".into(), "333".into())])
            .await
            .unwrap();
        mark_imported(&pool).await.unwrap();

        assert!(
            !import_once(&pool, &c, &config_from_a_file()).await.unwrap(),
            "the operator has spoken; the file no longer gets a say"
        );
        assert_eq!(stored_dpi(&pool, &c).await.as_deref(), Some("333"));
    }
}
