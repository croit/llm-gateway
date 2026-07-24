// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

// ---------------------------------------------------------------------------
// Markdown

/// GFM with raw HTML / `javascript:` / `vbscript:` rejected — the LLM
/// can emit `<script>` inside a fenced code block and we still want
/// it to render as escaped text, not execute. Fenced code blocks
/// with a recognised language hint are then post-processed through
/// `lumis` for server-side syntax highlighting (inline-styled spans).
///
/// Markdown image parsing (`![alt](url)`) is disabled. The model never
/// has a legitimate way to produce an image through prose — every image
/// in the chat arrives via the `[gw-attachment …]` marker pipeline
/// (rendered by `render_attachment`). So a markdown image is always
/// either a hallucinated/echoed URL — e.g. `![](image_url)`,
/// `![](preview_url)`, `![](<turn-id>/letter.png)` — that the browser
/// resolves *relative* to the `/chat/<id>` page (`/chat/image_url`,
/// `/chat/<turn-id>/letter.png`) and 404s, and, being re-emitted on
/// every streaming morph, re-fetched until the edge rate-limiter answers
/// 429. Disabling the construct degrades `![alt](url)` to a harmless
/// inline link the browser doesn't auto-load, rather than a live
/// `<img src>`. (Raw `<img>` HTML the model types is already escaped to
/// text by `Options::gfm()`'s `allow_dangerous_html = false`.)
pub fn render_markdown(text: &str) -> String {
    // Images in assistant prose are never real gateway attachments. Actual
    // images use gw-attachment markers and are rendered separately.
    let text = SYNTHETIC_IMAGE_RE.replace_all(text, "$1");
    let mut options = markdown::Options::gfm();
    options.parse.constructs.label_start_image = false;
    let html = markdown::to_html_with_options(&text, &options)
        .unwrap_or_else(|_| markdown::to_html(&text));
    highlight_fenced_code_blocks(&html)
}

/// Lumis themes loaded once at process start. We hand them to the
/// multi-themes formatter so every styled token comes out as
/// `color: light-dark(<day>, <night>)`; the browser picks the right
/// half from the document's `color-scheme`, which daisyUI sets per
/// `data-theme`. No second render needed when the user toggles
/// themes.
static LIGHT_THEME: std::sync::LazyLock<lumis::themes::Theme> = std::sync::LazyLock::new(|| {
    lumis::themes::get("tokyonight_day").expect("lumis ships the tokyonight_day theme")
});
static DARK_THEME: std::sync::LazyLock<lumis::themes::Theme> = std::sync::LazyLock::new(|| {
    lumis::themes::get("tokyonight_night").expect("lumis ships the tokyonight_night theme")
});

/// Match `<pre><code class="language-FOO">…</code></pre>` from the
/// markdown crate's GFM output. Pattern is exact-shape; markdown
/// doesn't intersperse other attributes or whitespace inside the
/// open tags. Language captures `[\w+\-.]+` so identifiers like
/// `c++` (encoded as `c++`), `objective-c`, `f#` (encoded), or
/// `csharp` all match. The body capture is lazy so consecutive
/// code blocks don't merge into one giant match.
static FENCED_CODE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?s)<pre><code class="language-([\w+\-.]+)">(.*?)</code></pre>"#).unwrap()
});

static SYNTHETIC_IMAGE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"!\[([^\]]*)\]\((?:[^)]*/)?generated-image[^)]*\)").unwrap()
});

pub(crate) fn highlight_fenced_code_blocks(html: &str) -> String {
    FENCED_CODE_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let lang_hint = &caps[1];
            let escaped_source = &caps[2];
            let source = html_unescape(escaped_source);
            // Lumis returns its own `<pre class="lumis"…><code…>` —
            // we replace the markdown crate's wrapper entirely so
            // we don't end up with a nested `<pre><pre>`. If lumis
            // doesn't know the language we fall back to the
            // markdown wrapper (plain monospace, no colour).
            match highlight_one(lang_hint, &source) {
                Some(highlighted) => highlighted,
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// Strip the inline `style="…"` lumis writes onto its outer `<pre>`
/// (it carries the theme's foreground + background). We let
/// `.chat-prose pre` (which already paints `--color-base-200` +
/// matching border + radius) provide the surface so highlighted
/// and un-highlighted blocks share one visual treatment. The
/// per-token `<span style>` colours stay — they're what actually
/// makes the highlighting visible.
static PRE_STYLE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"^(<pre[^>]*?) style="[^"]*""#).unwrap());

/// Invoke lumis for one code block. Returns `None` if the language
/// hint doesn't resolve (lumis returns `LanguageParseError`) or the
/// formatter errors out — caller falls back to the un-highlighted
/// HTML so the block still renders, just without colour.
pub(crate) fn highlight_one(lang_hint: &str, source: &str) -> Option<String> {
    use lumis::formatters::Formatter;
    let language: lumis::languages::Language = normalise_lang_hint(lang_hint).parse().ok()?;
    let mut themes = std::collections::HashMap::new();
    themes.insert("light".to_string(), LIGHT_THEME.clone());
    themes.insert("dark".to_string(), DARK_THEME.clone());
    let formatter = lumis::HtmlMultiThemesBuilder::new()
        .language(language)
        .themes(themes)
        .default_theme("light-dark()")
        .build()
        .ok()?;
    let mut output = Vec::new();
    formatter.format(source, &mut output).ok()?;
    let raw = String::from_utf8(output).ok()?;
    Some(PRE_STYLE_RE.replace(&raw, "$1").into_owned())
}

/// Map common markdown-fence shorthands to the canonical names
/// lumis recognises. Markdown writers use `sh`, `py`, `js`, `ts`,
/// `yml` interchangeably with the long form. Unrecognised hints
/// fall through unchanged and either parse against lumis directly
/// or fail open (un-highlighted code block).
pub(crate) fn normalise_lang_hint(hint: &str) -> &str {
    match hint.to_ascii_lowercase().as_str() {
        "sh" | "shell" | "zsh" => "bash",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "yml" => "yaml",
        "rs" => "rust",
        "c++" | "cxx" => "cpp",
        _ => hint,
    }
}

/// Decode the entity set the `markdown` crate emits inside `<code>`
/// blocks. The set is bounded — `&amp;`, `&lt;`, `&gt;`, `&quot;`,
/// `&#39;` — so we don't need a full HTML parser. Order matters:
/// decode `&amp;` last so a nested entity like `&amp;lt;` (which
/// means a literal `&lt;` in the source) round-trips through to
/// `&lt;` instead of decoding into `<` by accident.
pub(crate) fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Conversation
