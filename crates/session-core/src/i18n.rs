// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! UI language selection + Fluent-backed string lookup.
//!
//! Mirrors `chrome::Theme` deliberately: a stateless `lang` cookie
//! read ad-hoc in every page handler (no request-extension/session
//! plumbing exists in this codebase — see `chrome::Theme` for the
//! precedent), threaded down through the same render-parameter chain
//! `theme` already rides. The one difference from `Theme` is that API
//! JSON responses (`json_err` call sites) need a *different* priority
//! order than page renders — see [`Lang::from_headers`] vs
//! [`Lang::from_request`].
//!
//! Translations are LLM-generated for the initial rollout and flagged
//! `unreviewed` at the top of each non-English `.ftl` file pending
//! native-speaker QA.

use std::borrow::Cow;
use std::collections::HashMap;

use fluent_bundle::FluentValue;
use fluent_templates::{LanguageIdentifier, Loader, langid, static_loader};
use rama::http::{HeaderMap, HeaderValue, header};

use crate::chrome::read_cookie;

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
        // Fluent's bidi-isolation marks (U+2068/U+2069) are meant for
        // mixed-direction terminal/GUI text; none of our 6 locales are
        // RTL and the invisible marks show up as literal glyphs in a
        // handful of monospace contexts (tool-call output blocks), so
        // they're more trouble than benefit here.
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

/// Cookie name carrying the user's language preference. Read on every
/// page render; written by [`crate::chrome::lang_set`].
pub const LANG_COOKIE: &str = "lang";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
    Fr,
    Es,
    Ru,
    Zh,
}

impl Lang {
    pub const ALL: [Lang; 6] = [Lang::En, Lang::De, Lang::Fr, Lang::Es, Lang::Ru, Lang::Zh];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
            Lang::Fr => "fr",
            Lang::Es => "es",
            Lang::Ru => "ru",
            Lang::Zh => "zh",
        }
    }

    /// The language's own endonym — shown in the switcher regardless of
    /// which language is currently active. Language names conventionally
    /// aren't translated (a French speaker still picks "Deutsch", not
    /// "German"), so this is the one piece of switcher UI that isn't a
    /// Fluent key.
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::De => "Deutsch",
            Lang::Fr => "Français",
            Lang::Es => "Español",
            Lang::Ru => "Русский",
            Lang::Zh => "中文",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "en" => Some(Lang::En),
            "de" => Some(Lang::De),
            "fr" => Some(Lang::Fr),
            "es" => Some(Lang::Es),
            "ru" => Some(Lang::Ru),
            "zh" => Some(Lang::Zh),
            _ => None,
        }
    }

    fn langid(self) -> LanguageIdentifier {
        match self {
            Lang::En => langid!("en"),
            Lang::De => langid!("de"),
            Lang::Fr => langid!("fr"),
            Lang::Es => langid!("es"),
            Lang::Ru => langid!("ru"),
            Lang::Zh => langid!("zh"),
        }
    }

    /// Page rendering: `lang` cookie first — an explicit in-app choice
    /// should stick regardless of what the browser's `Accept-Language`
    /// happens to say — falling back to `Accept-Language` only to pick a
    /// sane *first-visit* default, then English. Never writes the cookie
    /// itself; persistence only happens via an explicit switcher POST
    /// ([`crate::chrome::lang_set`]), so a sniffed default never gets
    /// silently locked in.
    pub fn from_headers(headers: &HeaderMap) -> Lang {
        if let Some(code) = read_cookie(headers, LANG_COOKIE)
            && let Some(lang) = Lang::from_code(&code)
        {
            return lang;
        }
        Lang::from_accept_language(headers).unwrap_or(Lang::En)
    }

    /// API/JSON responses (`json_err` sites): `Accept-Language` first,
    /// since bearer-token/CLI clients never carry the `lang` cookie;
    /// `lang` cookie as a fallback for same-origin browser `fetch()`
    /// calls that do carry it; English otherwise. Deliberately the
    /// opposite priority from [`Lang::from_headers`] — see the module
    /// docs.
    pub fn from_request(headers: &HeaderMap) -> Lang {
        if let Some(lang) = Lang::from_accept_language(headers) {
            return lang;
        }
        if let Some(code) = read_cookie(headers, LANG_COOKIE)
            && let Some(lang) = Lang::from_code(&code)
        {
            return lang;
        }
        Lang::En
    }

    /// Highest-`q` primary subtag that maps to one of the 6 supported
    /// languages. Deliberately not a general BCP-47 negotiator (region
    /// subtags, script subtags, `*` wildcards) — scoped down to exactly
    /// what picking among 6 fixed languages needs.
    fn from_accept_language(headers: &HeaderMap) -> Option<Lang> {
        let raw = headers.get(header::ACCEPT_LANGUAGE)?.to_str().ok()?;
        let mut best: Option<(f32, Lang)> = None;
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (tag, q) = match part.split_once(";q=") {
                Some((tag, q)) => (tag.trim(), q.trim().parse::<f32>().unwrap_or(1.0)),
                None => (part, 1.0),
            };
            let primary = tag.split(['-', '_']).next().unwrap_or(tag);
            let Some(lang) = Lang::from_code(&primary.to_ascii_lowercase()) else {
                continue;
            };
            if best.is_none_or(|(best_q, _)| q > best_q) {
                best = Some((q, lang));
            }
        }
        best.map(|(_, lang)| lang)
    }
}

/// `Set-Cookie` value for the language preference. 1-year max-age, same
/// shape as `chrome::set_theme_header`.
pub fn set_lang_header(lang: Lang) -> HeaderValue {
    let value = format!(
        "{LANG_COOKIE}={}; Path=/; SameSite=Lax; Max-Age={}",
        lang.code(),
        60 * 60 * 24 * 365
    );
    HeaderValue::try_from(value).expect("lang cookie value is ascii")
}

/// Look up a Fluent message. Never panics on a missing key — a
/// translation gap degrades to the raw key (logged) rather than
/// failing the page render, since these 6 locale files are LLM-
/// generated and will keep growing as new UI ships.
pub fn t(lang: Lang, key: &str) -> String {
    match LOCALES.try_lookup(&lang.langid(), key) {
        Some(s) => s,
        None => {
            tracing::warn!(lang = lang.code(), key, "missing i18n key");
            key.to_string()
        }
    }
}

/// Like [`t`], with Fluent interpolation/plural args. Build `args` with
/// [`args`].
pub fn t_args(lang: Lang, key: &str, args: &FluentArgs) -> String {
    match LOCALES.try_lookup_with_args(&lang.langid(), key, args) {
        Some(s) => s,
        None => {
            tracing::warn!(lang = lang.code(), key, "missing i18n key");
            key.to_string()
        }
    }
}

/// The args map type `fluent-templates`'s `Loader` trait expects.
/// `fluent-bundle` doesn't export a ready-made `FluentArgs` alias for
/// this shape, so we name it here rather than repeating the full type
/// at every call site.
pub type FluentArgs = HashMap<Cow<'static, str>, FluentValue<'static>>;

/// Build a [`FluentArgs`] from `(name, value)` pairs, e.g.
/// `t_args(lang, "key", &i18n::args([("name", user_name.into())]))`.
pub fn args(pairs: impl IntoIterator<Item = (&'static str, FluentValue<'static>)>) -> FluentArgs {
    pairs
        .into_iter()
        .map(|(k, v)| (Cow::Borrowed(k), v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rama::http::HeaderMap;

    fn headers_with_cookie(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    }

    fn headers_with_accept_language(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn from_headers_defaults_to_english_with_no_signal() {
        assert_eq!(Lang::from_headers(&HeaderMap::new()), Lang::En);
    }

    #[test]
    fn from_headers_prefers_cookie_over_accept_language() {
        let mut h = headers_with_cookie("lang=de");
        h.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("fr-FR,fr;q=0.9"),
        );
        assert_eq!(Lang::from_headers(&h), Lang::De);
    }

    #[test]
    fn from_headers_falls_back_to_accept_language_first_visit() {
        assert_eq!(
            Lang::from_headers(&headers_with_accept_language("fr-FR,fr;q=0.9,en;q=0.5")),
            Lang::Fr
        );
    }

    #[test]
    fn from_headers_ignores_unsupported_cookie_value() {
        assert_eq!(
            Lang::from_headers(&headers_with_cookie("lang=it")),
            Lang::En
        );
    }

    #[test]
    fn from_request_prefers_accept_language_over_cookie() {
        let mut h = headers_with_cookie("lang=de");
        h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("ru"));
        assert_eq!(Lang::from_request(&h), Lang::Ru);
    }

    #[test]
    fn from_request_falls_back_to_cookie_without_accept_language() {
        assert_eq!(
            Lang::from_request(&headers_with_cookie("lang=zh")),
            Lang::Zh
        );
    }

    #[test]
    fn from_request_defaults_to_english_with_no_signal() {
        assert_eq!(Lang::from_request(&HeaderMap::new()), Lang::En);
    }

    #[test]
    fn accept_language_picks_highest_q_among_supported() {
        // `es` has the higher q-value despite `it` (unsupported) appearing
        // with no explicit q (defaults to 1.0 but isn't one of the 6).
        assert_eq!(
            Lang::from_headers(&headers_with_accept_language("it,es;q=0.8")),
            Lang::Es
        );
    }

    #[test]
    fn t_falls_back_to_key_on_missing_translation() {
        assert_eq!(
            t(Lang::En, "this-key-does-not-exist"),
            "this-key-does-not-exist"
        );
    }

    #[test]
    fn t_resolves_seeded_chrome_key() {
        assert_eq!(t(Lang::En, "chrome-theme-toggle-title"), "Toggle theme");
    }
}
