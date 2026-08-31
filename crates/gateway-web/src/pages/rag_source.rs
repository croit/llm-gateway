// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The source-kind half of the `/rag` create and edit forms.
//!
//! Everything here is driven by [`ProviderFactory::config_fields`]: the
//! picker is built from the registered providers, and each provider's inputs
//! are rendered from the fields it declares. **No provider is named in this
//! file.** That is the point — a page that matched on `"webdav"` to decide
//! which inputs to draw would put the extensibility back where it started,
//! and adding Dropbox would mean editing the admin UI.
//!
//! `git` is the one special case, and deliberately so: it is not a
//! [`FileProvider`] (a clone materialises the tree on disk, which the worker
//! reads directly) so it has no factory to enumerate. It is offered as the
//! first option and maps to [`SourceSpec::default`].
//!
//! Secrets never round-trip to the browser. On the edit form a stored secret
//! renders as an empty input labelled "stored"; leaving it empty keeps what
//! is stored, and a **Clear** checkbox is the only way to remove one.
//!
//! [`FileProvider`]: gateway_features::server::rag::source::FileProvider

use std::collections::BTreeMap;

use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::rag as rag_db;
use gateway_features::server::rag::source;
use gateway_features::server::rag::source::{
    ConfigField, FieldKind, ProviderConfig, ProviderRegistry,
};
use plait::{Html, ToHtml, html};
use session_core::i18n::{Lang, t};

/// The `source_kind` value standing for the original git behaviour.
pub const GIT_KIND: &str = "git";

/// Form-field name for one provider setting. Namespaced by kind so every
/// provider's inputs can be present in the DOM at once and only the selected
/// set is read on submit — which is what lets the picker switch client-side
/// with no server round-trip.
fn field_name(kind: &str, key: &str) -> String {
    format!("src_{kind}_{key}")
}

/// Checkbox name for "clear this stored secret".
fn clear_name(kind: &str, key: &str) -> String {
    format!("clearsrc_{kind}_{key}")
}

/// The signal expression that shows a block only for `kind`.
fn show_for(kind: &str) -> String {
    format!("$sourceKind === '{kind}'")
}

/// The source-kind `<select>`, plus the signal store the field sets react to.
///
/// `data-init` seeds the signal from the live DOM rather than trusting the
/// server-rendered default, so an edit form opened on an existing collection
/// shows the right field set before any `change` event fires.
pub fn source_picker(lang: Lang, registry: &ProviderRegistry, selected: &str) -> Html {
    let options: Vec<(String, String, String, bool)> = std::iter::once((
        GIT_KIND.to_string(),
        t(lang, "rag-source-git-label"),
        t(lang, "rag-source-git-help"),
        selected == GIT_KIND,
    ))
    .chain(registry.factories().iter().map(|f| {
        (
            f.kind().to_string(),
            f.label().to_string(),
            f.description().to_string(),
            selected == f.kind(),
        )
    }))
    .collect();
    let help: Vec<(String, String)> = options
        .iter()
        .map(|(kind, _, description, _)| (show_for(kind), description.clone()))
        .collect();

    html! {
        div(class: "flex flex-col gap-1 w-full md:col-span-2") {
            label(class: "flex flex-col gap-1 w-full") {
                div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-source-kind")) } }
                select(
                    name: "source_kind",
                    class: "select select-bordered w-full",
                    "data-on:change": "$sourceKind = evt.target.value"
                ) {
                    for (kind, label, _, is_selected) in options.iter() {
                        (super::select_option(kind, label, *is_selected))
                    }
                }
            }
            for (show, description) in help.iter() {
                p(
                    class: "text-xs opacity-70",
                    "data-show": (show.clone()),
                    style: "display:none"
                ) {
                    (description.clone())
                }
            }
        }
    }
    .to_html()
}

/// Hidden signal store for the picker. Rendered once per form.
pub fn source_signals(selected: &str) -> Html {
    let signals = format!("{{sourceKind: '{}'}}", selected.replace('\'', ""));
    html! {
        div(
            "data-signals": (signals),
            "data-init": "$sourceKind = document.querySelector('[name=source_kind]')?.value ?? $sourceKind",
            style: "display:none"
        ) {}
    }
    .to_html()
}

/// One provider's field set, shown only while that provider is selected.
///
/// `existing` supplies current values when editing a collection of this same
/// kind; a different kind renders empty, so switching kinds never carries
/// another provider's settings across.
pub fn provider_fields(
    lang: Lang,
    registry: &ProviderRegistry,
    existing: Option<&rag_db::SourceSpec>,
    consent: Option<ConsentState<'_>>,
) -> Html {
    let blocks: Vec<(String, Vec<FieldView>, Html)> = registry
        .factories()
        .iter()
        .map(|f| {
            let current = existing.filter(|s| s.kind == f.kind());
            (
                show_for(f.kind()),
                f.config_fields()
                    .iter()
                    .map(|field| FieldView::build(f.kind(), field, current))
                    .collect(),
                consent_block(lang, f.as_ref(), consent),
            )
        })
        .collect();

    html! {
        for (show, fields, consent) in blocks.iter() {
            div(
                class: "grid grid-cols-1 md:grid-cols-2 gap-4 md:col-span-2",
                "data-show": (show.clone()),
                style: "display:none"
            ) {
                for f in fields.iter() {
                    (f.render(lang))
                }
                (consent.clone())
            }
        }
    }
    .to_html()
}

/// What the form knows about a collection's browser consent.
///
/// `None` on the create form: there is no collection to hang a consent on
/// yet, which is itself the thing the operator needs told.
#[derive(Debug, Clone, Copy)]
pub struct ConsentState<'a> {
    pub collection_id: i64,
    /// Whether a refresh token is already stored for this source.
    pub connected: bool,
    /// The account the corpus is read as, when one is recorded.
    pub account: Option<&'a str>,
}

/// The connect / reconnect control for a provider that needs browser consent.
///
/// Empty for a provider authorised by typed credentials — asked of
/// [`ProviderFactory::auth`], so this file still names no provider.
fn consent_block(
    lang: Lang,
    factory: &dyn source::ProviderFactory,
    consent: Option<ConsentState<'_>>,
) -> Html {
    if !matches!(factory.auth(), source::AuthKind::OAuth2 { .. }) {
        return html! {}.to_html();
    }
    let Some(state) = consent else {
        // Create form: the client id and secret have to be stored before
        // there is anything to consent *with*, so say so rather than offering
        // a button that cannot work yet.
        return html! {
            div(class: "md:col-span-2 alert alert-info text-sm") {
                (t(lang, "rag-source-consent-save-first"))
            }
        }
        .to_html();
    };
    let href = format!("/rag/{}/connect", state.collection_id);
    let (badge_class, badge) = if state.connected {
        (
            "badge badge-success",
            t(lang, "rag-source-consent-connected"),
        )
    } else {
        (
            "badge badge-warning",
            t(lang, "rag-source-consent-not-connected"),
        )
    };
    let action = if state.connected {
        t(lang, "rag-source-consent-reconnect")
    } else {
        t(lang, "rag-source-consent-connect")
    };
    // Naming the account is the point: everyone who can search this
    // collection reads through it, so leaving it to a "Test connection"
    // toast made the one fact with a security consequence the hardest to
    // find.
    let account = state.account.unwrap_or_default().to_string();
    let has_account = !account.is_empty();
    html! {
        div(class: "md:col-span-2 flex flex-col gap-1") {
            div(class: "flex flex-wrap items-center gap-3") {
                span(class: (badge_class)) { (badge) }
                // A plain link, not a form post: the browser has to leave for
                // the provider's consent screen, and this page is otherwise
                // driven by datastar over SSE, which cannot navigate away.
                a(href: (href), class: "btn btn-sm btn-primary") { (action) }
                if has_account {
                    span(class: "text-xs font-mono opacity-80") { (account) }
                }
            }
            span(class: "text-xs opacity-70") { (t(lang, "rag-source-consent-help")) }
        }
    }
    .to_html()
}

/// One rendered input, flattened out of `ConfigField` so the `html!` macro
/// only ever sees owned values.
struct FieldView {
    name: String,
    clear_name: String,
    label: String,
    help: String,
    value: String,
    input_type: &'static str,
    required: bool,
    secret_stored: bool,
    is_bool: bool,
    checked: bool,
}

impl FieldView {
    fn build(kind: &str, field: &ConfigField, current: Option<&rag_db::SourceSpec>) -> Self {
        let stored = current.and_then(|s| s.config.get(field.key)).cloned();
        // A secret's value is never sent back to the browser; only the fact
        // that one is stored, so the operator can tell "not set" from "set,
        // leave it alone". The sealed bundle is opaque here, so its presence
        // is the available signal — which errs toward "leave it alone".
        let secret_stored =
            field.kind == FieldKind::Secret && current.is_some_and(|s| s.secrets.is_some());
        let value = match field.kind {
            FieldKind::Secret => String::new(),
            _ => stored
                .clone()
                .or_else(|| field.default.map(str::to_string))
                .unwrap_or_default(),
        };
        Self {
            name: field_name(kind, field.key),
            clear_name: clear_name(kind, field.key),
            label: field.label.to_string(),
            help: field.help.to_string(),
            value,
            input_type: match field.kind {
                FieldKind::Secret => "password",
                FieldKind::Url => "url",
                _ => "text",
            },
            required: field.required && field.kind != FieldKind::Secret,
            secret_stored,
            is_bool: field.kind == FieldKind::Bool,
            checked: stored.as_deref() == Some("true"),
        }
    }

    fn render(&self, lang: Lang) -> Html {
        if self.is_bool {
            return self.render_checkbox();
        }
        let stored_note = if self.secret_stored {
            t(lang, "rag-source-secret-stored")
        } else {
            String::new()
        };
        let placeholder = if self.secret_stored {
            t(lang, "rag-source-secret-placeholder")
        } else {
            String::new()
        };
        // `required` is emitted only when true: an `required="false"`
        // attribute is still honoured by browsers (see `pages/mod.rs`).
        let field = if self.required {
            html! {
                input(
                    name: (self.name.clone()),
                    type: (self.input_type),
                    value: (self.value.clone()),
                    placeholder: (placeholder.clone()),
                    class: "input input-bordered w-full",
                    required: "required"
                );
            }
            .to_html()
        } else {
            html! {
                input(
                    name: (self.name.clone()),
                    type: (self.input_type),
                    value: (self.value.clone()),
                    placeholder: (placeholder.clone()),
                    class: "input input-bordered w-full"
                );
            }
            .to_html()
        };
        html! {
            label(class: "flex flex-col gap-1 w-full") {
                div(class: "label") {
                    span(class: "label-text") { (self.label.clone()) }
                    if self.secret_stored {
                        span(class: "badge badge-ghost badge-sm") { (stored_note.clone()) }
                    }
                }
                (field)
                p(class: "text-xs opacity-70") { (self.help.clone()) }
                if self.secret_stored {
                    label(class: "flex items-center gap-2 cursor-pointer") {
                        input(
                            name: (self.clear_name.clone()),
                            type: "checkbox",
                            class: "checkbox checkbox-xs"
                        );
                        span(class: "text-xs opacity-70") { (t(lang, "rag-source-secret-clear")) }
                    }
                }
            }
        }
        .to_html()
    }

    fn render_checkbox(&self) -> Html {
        let input = if self.checked {
            html! {
                input(
                    name: (self.name.clone()),
                    type: "checkbox",
                    value: "true",
                    class: "checkbox checkbox-sm",
                    checked: "checked"
                );
            }
            .to_html()
        } else {
            html! {
                input(
                    name: (self.name.clone()),
                    type: "checkbox",
                    value: "true",
                    class: "checkbox checkbox-sm"
                );
            }
            .to_html()
        };
        html! {
            label(class: "flex items-start gap-3 cursor-pointer w-full") {
                (input)
                span(class: "min-w-0") {
                    span(class: "label-text") { (self.label.clone()) }
                    p(class: "text-xs opacity-70") { (self.help.clone()) }
                }
            }
        }
        .to_html()
    }
}

/// What a submitted form said about the source, before sealing.
pub struct ParsedSource {
    pub kind: String,
    /// Every submitted value for the selected kind. Which of these are
    /// secret is the provider's call, so the split happens in [`to_spec`],
    /// not here.
    pub config: BTreeMap<String, String>,
    /// Secret keys the operator explicitly ticked to clear.
    pub cleared: Vec<String>,
}

/// Pull the selected kind and its namespaced fields out of a submitted form.
///
/// Form pairs, not a typed struct: the field set is owned by the provider, so
/// there is nothing to derive `Deserialize` for. Fields belonging to
/// providers other than the selected one are ignored, which is what makes
/// rendering every provider's inputs at once safe.
pub fn parse_form(pairs: &[(String, String)]) -> ParsedSource {
    let kind = pairs
        .iter()
        .find(|(k, _)| k == "source_kind")
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| GIT_KIND.to_string());

    let value_prefix = format!("src_{kind}_");
    let clear_prefix = format!("clearsrc_{kind}_");
    let mut config = BTreeMap::new();
    let mut cleared = Vec::new();
    for (k, v) in pairs {
        if let Some(key) = k.strip_prefix(&clear_prefix) {
            cleared.push(key.to_string());
        } else if let Some(key) = k.strip_prefix(&value_prefix) {
            config.insert(key.to_string(), v.trim().to_string());
        }
    }
    ParsedSource {
        kind,
        config,
        cleared,
    }
}

/// Validate a parsed source against its provider and produce the storable
/// [`rag_db::SourceSpec`], sealing any secrets.
///
/// `existing` carries the currently stored spec on an edit, so a secret left
/// blank keeps its stored value instead of being wiped — the single most
/// annoying way for an admin form to lose a credential.
pub fn to_spec(
    lang: Lang,
    parsed: ParsedSource,
    registry: &ProviderRegistry,
    crypto: &Crypto,
    existing: Option<&rag_db::SourceSpec>,
    http: reqwest::Client,
) -> Result<rag_db::SourceSpec, String> {
    if parsed.kind == GIT_KIND {
        return Ok(rag_db::SourceSpec::default());
    }
    let factory = registry
        .get(&parsed.kind)
        .ok_or_else(|| t(lang, "rag-source-unknown-kind"))?;

    // Split the submitted values by what the provider says is secret.
    let secret_keys = factory.secret_keys();
    let mut config = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    for (k, v) in parsed.config {
        if secret_keys.contains(&k.as_str()) {
            if !v.is_empty() {
                secrets.insert(k, v);
            }
        } else {
            config.insert(k, v);
        }
    }

    // Merge with what is already stored: a blank secret input means "keep",
    // and only an explicit tick clears one.
    let stored = existing
        .filter(|s| s.kind == parsed.kind)
        .map(|s| s.open_secrets(crypto))
        .unwrap_or_default();
    let mut merged = stored;
    for key in &parsed.cleared {
        merged.remove(key);
    }
    merged.extend(secrets);
    merged.retain(|_, v| !v.is_empty());

    let cfg = ProviderConfig::new(config.clone(), merged.clone());
    factory.validate(&cfg).map_err(|e| e.to_string())?;
    // Construct once here so a bad URL is a form error rather than a build
    // failure discovered minutes later on the indexing timeline.
    //
    // Except before consent: an OAuth source has no usable credential until
    // someone has clicked through the provider's consent screen, and that
    // cannot happen until the client id and secret are *saved*. Demanding a
    // buildable provider here would deadlock the two against each other.
    if !source::awaiting_consent(factory.as_ref(), &merged) {
        factory.build(&cfg, http).map_err(|e| e.to_string())?;
    }

    seal_spec(parsed.kind, config, merged, crypto)
}

/// Whether this source already holds a refresh token, i.e. someone has been
/// through the provider's consent screen.
///
/// Opening the sealed blob is the only way to tell: the secrets are one AEAD
/// ciphertext, so "which keys are in it" is not visible without the at-rest
/// key. A blob that will not open reads as not-connected, which points the
/// operator at Connect — the correct recovery either way.
pub fn has_refresh_token(spec: &rag_db::SourceSpec, crypto: &Crypto) -> bool {
    spec.open_secrets(crypto)
        .contains_key(source::REFRESH_TOKEN_KEY)
}

fn seal_spec(
    kind: String,
    config: BTreeMap<String, String>,
    secrets: BTreeMap<String, String>,
    crypto: &Crypto,
) -> Result<rag_db::SourceSpec, String> {
    let sealed = if secrets.is_empty() {
        None
    } else {
        let json = serde_json::to_string(&secrets).map_err(|e| e.to_string())?;
        Some(crypto.seal_str(&json).map_err(|e| e.to_string())?)
    };
    Ok(rag_db::SourceSpec {
        kind,
        config,
        secrets: sealed,
    })
}

/// May this collection's stored secrets stand in for a blank password field?
///
/// Only when the rest of the submitted settings are byte-for-byte what is
/// stored. "Test connection" otherwise becomes a credential exfiltrator: an
/// admin (or anything that can post as one) submits an existing
/// `collection_id`, an empty password, and a `base_url` pointing anywhere,
/// and the probe cheerfully presents that collection's app password to the
/// named host as `Authorization: Basic`.
///
/// Comparing the whole non-secret config rather than trying to name the
/// "destination" fields is deliberate: which settings decide where the bytes
/// go is provider knowledge, and a page that guessed would be wrong for the
/// first provider that puts its host somewhere unexpected. The cost is that
/// after editing a setting the password must be retyped to test — which the
/// error message says.
pub fn stored_secrets_may_stand_in(
    factory: &dyn source::ProviderFactory,
    submitted: &BTreeMap<String, String>,
    stored: &rag_db::SourceSpec,
) -> bool {
    if stored.kind != factory.kind() {
        return false;
    }
    let secret_keys = factory.secret_keys();
    let submitted_public: BTreeMap<&str, &str> = submitted
        .iter()
        .filter(|(k, _)| !secret_keys.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let stored_public: BTreeMap<&str, &str> = stored
        .config
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    submitted_public == stored_public
}

/// Build a live provider from a submitted form, for the "Test connection"
/// button. Secrets come from the form when given and from storage otherwise,
/// so testing an existing collection does not require retyping the password.
pub fn provider_for_probe(
    lang: Lang,
    parsed: ParsedSource,
    registry: &ProviderRegistry,
    crypto: &Crypto,
    existing: Option<&rag_db::SourceSpec>,
    http: reqwest::Client,
) -> Result<std::sync::Arc<dyn gateway_features::server::rag::source::FileProvider>, String> {
    let spec = to_spec(lang, parsed, registry, crypto, existing, http.clone())?;
    let secrets = spec.open_secrets(crypto);
    registry
        .build(&spec.kind, &ProviderConfig::new(spec.config, secrets), http)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_features::server::rag::source::ProviderRegistry;

    fn pairs(kv: &[(&str, &str)]) -> Vec<(String, String)> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn crypto() -> Crypto {
        Crypto::from_key([3u8; 32])
    }

    /// "Test connection" must not lend a stored credential to a host the
    /// submitter chose.
    ///
    /// Regression: the probe merged the stored secrets of whatever
    /// `collection_id` the form named over a blank password field, then built
    /// the provider from the *form's* `base_url` — so posting an existing
    /// collection id, an empty password and an attacker's host presented that
    /// collection's app password to the attacker as `Authorization: Basic`.
    #[test]
    fn a_stored_secret_is_only_lent_back_to_the_settings_it_was_stored_for() {
        let reg = ProviderRegistry::with_builtins();
        let factory = reg.get("webdav").unwrap();
        let stored = rag_db::SourceSpec {
            kind: "webdav".into(),
            config: [
                (
                    "base_url".to_string(),
                    "https://cloud.example.com".to_string(),
                ),
                ("username".to_string(), "svc".to_string()),
            ]
            .into_iter()
            .collect(),
            secrets: None,
        };

        let same: BTreeMap<String, String> = [
            (
                "base_url".to_string(),
                "https://cloud.example.com".to_string(),
            ),
            ("username".to_string(), "svc".to_string()),
            // A blank password is the whole point: it means "keep".
            ("password".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        assert!(
            stored_secrets_may_stand_in(factory.as_ref(), &same, &stored),
            "unchanged settings still test without retyping the password"
        );

        let elsewhere: BTreeMap<String, String> = [
            (
                "base_url".to_string(),
                "https://attacker.example".to_string(),
            ),
            ("username".to_string(), "svc".to_string()),
            ("password".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        assert!(
            !stored_secrets_may_stand_in(factory.as_ref(), &elsewhere, &stored),
            "a different host must not be handed the stored password"
        );

        // Any other edited setting is refused too — which settings decide
        // where the bytes go is the provider's business, not this page's.
        let renamed: BTreeMap<String, String> = [
            (
                "base_url".to_string(),
                "https://cloud.example.com".to_string(),
            ),
            ("username".to_string(), "someone-else".to_string()),
            ("password".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        assert!(!stored_secrets_may_stand_in(
            factory.as_ref(),
            &renamed,
            &stored
        ));
    }

    #[test]
    fn a_form_with_no_source_kind_is_git() {
        let parsed = parse_form(&pairs(&[("name", "x")]));
        assert_eq!(parsed.kind, GIT_KIND);
    }

    #[test]
    fn only_the_selected_kinds_fields_are_read() {
        let parsed = parse_form(&pairs(&[
            ("source_kind", "webdav"),
            ("src_webdav_base_url", "https://cloud.example.com"),
            // A different provider's input, present in the DOM at the same
            // time. It must not leak into the saved config.
            ("src_dropbox_token", "should-be-ignored"),
        ]));
        assert_eq!(parsed.kind, "webdav");
        assert_eq!(
            parsed.config.get("base_url").map(String::as_str),
            Some("https://cloud.example.com")
        );
        assert!(!parsed.config.contains_key("token"));
    }

    #[test]
    fn secrets_are_sealed_and_never_stored_in_the_config_map() {
        let reg = ProviderRegistry::with_builtins();
        let c = crypto();
        let spec = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "app-pw"),
            ])),
            &reg,
            &c,
            None,
            reqwest::Client::new(),
        )
        .expect("a complete webdav form is valid");

        assert_eq!(spec.kind, "webdav");
        assert!(
            !spec.config.contains_key("password"),
            "a secret must never land in the plaintext config column"
        );
        let sealed = spec.secrets.expect("the password was sealed");
        let plain = c.open_str(&sealed.nonce, &sealed.ciphertext).unwrap();
        assert!(plain.contains("app-pw"));
    }

    #[test]
    fn a_blank_secret_on_edit_keeps_the_stored_one() {
        let reg = ProviderRegistry::with_builtins();
        let c = crypto();
        let first = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "app-pw"),
            ])),
            &reg,
            &c,
            None,
            reqwest::Client::new(),
        )
        .unwrap();

        // Re-submitting the edit form without retyping the password.
        let second = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", ""),
            ])),
            &reg,
            &c,
            Some(&first),
            reqwest::Client::new(),
        )
        .expect("an unchanged password must not invalidate the form");

        let sealed = second.secrets.expect("the stored password survived");
        let plain = c.open_str(&sealed.nonce, &sealed.ciphertext).unwrap();
        assert!(plain.contains("app-pw"), "the credential was silently lost");
    }

    #[test]
    fn ticking_clear_removes_the_stored_secret() {
        let reg = ProviderRegistry::with_builtins();
        let c = crypto();
        let first = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "app-pw"),
            ])),
            &reg,
            &c,
            None,
            reqwest::Client::new(),
        )
        .unwrap();

        // Clearing the only required secret leaves the form invalid, which is
        // the honest outcome: the provider cannot work without it.
        let err = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("clearsrc_webdav_password", "on"),
            ])),
            &reg,
            &c,
            Some(&first),
            reqwest::Client::new(),
        )
        .expect_err("clearing a required credential must not save silently");
        assert!(err.to_lowercase().contains("password"), "{err}");
    }

    #[test]
    fn a_missing_required_field_is_reported_by_label() {
        let reg = ProviderRegistry::with_builtins();
        let err = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", ""),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "pw"),
            ])),
            &reg,
            &crypto(),
            None,
            reqwest::Client::new(),
        )
        .expect_err("an empty server URL is not valid");
        assert!(err.to_lowercase().contains("server url"), "{err}");
    }

    #[test]
    fn an_unparseable_url_is_caught_on_save_not_at_index_time() {
        let reg = ProviderRegistry::with_builtins();
        let err = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "pw"),
            ])),
            &reg,
            &crypto(),
            None,
            reqwest::Client::new(),
        )
        .expect_err("a schemeless URL must be rejected by the form");
        assert!(err.contains("https://"), "{err}");
    }

    #[test]
    fn choosing_git_stores_the_default_spec_and_no_secrets() {
        let reg = ProviderRegistry::with_builtins();
        let spec = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "git"),
                // Leftovers from a provider field set the operator touched
                // before switching back to git.
                ("src_webdav_password", "app-pw"),
            ])),
            &reg,
            &crypto(),
            None,
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(spec.kind, "git");
        assert!(spec.config.is_empty());
        assert!(
            spec.secrets.is_none(),
            "switching to git must not carry another provider's credential"
        );
    }

    #[test]
    fn the_picker_offers_git_first_then_every_registered_provider() {
        let reg = ProviderRegistry::with_builtins();
        let html = source_picker(Lang::En, &reg, GIT_KIND).to_string();
        assert!(html.contains(r#"value="git""#));
        assert!(html.contains(r#"value="webdav""#));
        let git_at = html.find(r#"value="git""#).unwrap();
        let dav_at = html.find(r#"value="webdav""#).unwrap();
        assert!(git_at < dav_at, "git stays the first, default option");
    }

    #[test]
    fn provider_inputs_are_namespaced_and_rendered_from_declared_fields() {
        let reg = ProviderRegistry::with_builtins();
        let html = provider_fields(Lang::En, &reg, None, None).to_string();
        for key in ["base_url", "username", "password", "dav_path", "root"] {
            assert!(
                html.contains(&format!(r#"name="src_webdav_{key}""#)),
                "missing input for {key}"
            );
        }
        assert!(
            html.contains(r#"type="password""#),
            "the secret field renders as a password input"
        );
    }

    #[test]
    fn a_stored_secret_is_never_rendered_back_to_the_browser() {
        let reg = ProviderRegistry::with_builtins();
        let c = crypto();
        let spec = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "hunter2"),
            ])),
            &reg,
            &c,
            None,
            reqwest::Client::new(),
        )
        .unwrap();
        let html = provider_fields(Lang::En, &reg, Some(&spec), None).to_string();
        assert!(
            !html.contains("hunter2"),
            "the plaintext credential reached the page"
        );
        assert!(
            html.contains(r#"name="clearsrc_webdav_password""#),
            "a stored secret offers an explicit clear control"
        );
    }

    #[test]
    fn non_secret_values_are_prefilled_on_edit() {
        let reg = ProviderRegistry::with_builtins();
        let c = crypto();
        let spec = to_spec(
            Lang::En,
            parse_form(&pairs(&[
                ("source_kind", "webdav"),
                ("src_webdav_base_url", "https://cloud.example.com"),
                ("src_webdav_username", "svc"),
                ("src_webdav_password", "pw"),
                ("src_webdav_root", "Finance/Invoices"),
            ])),
            &reg,
            &c,
            None,
            reqwest::Client::new(),
        )
        .unwrap();
        let html = provider_fields(Lang::En, &reg, Some(&spec), None).to_string();
        assert!(html.contains("Finance/Invoices"));
        assert!(html.contains("https://cloud.example.com"));
    }

    #[test]
    fn field_sets_are_hidden_until_their_kind_is_selected() {
        let reg = ProviderRegistry::with_builtins();
        let html = provider_fields(Lang::En, &reg, None, None).to_string();
        // plait HTML-escapes attribute values, so match on the
        // escaping-stable prefix plus the kind rather than the whole literal.
        assert!(
            html.contains(r#"data-show="$sourceKind ==="#) && html.contains("webdav"),
            "each field set is gated on the picker's signal: {html}"
        );
        assert!(
            html.contains("display:none"),
            "hidden before hydration, so the wrong set never flashes"
        );
    }

    #[test]
    fn the_signal_store_seeds_itself_from_the_rendered_select() {
        let html = source_signals("webdav").to_string();
        assert!(html.contains("sourceKind:") && html.contains("webdav"));
        assert!(
            html.contains("data-init"),
            "an edit form must show the right field set on load, not only after a change"
        );
    }
}
