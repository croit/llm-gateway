// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// One attachment chip / inline image. Role-agnostic — the same
/// renderer fires for user-uploaded files and assistant-uploaded
/// files (via the `upload_attachment` tool) so the UI stays DRY and
/// the model's attachments look identical to the user's.
///
/// Images are `<img>`-displayed at a thumbnail cap (max ~16 rem each
/// side) linked through to the full-res URL. Audio and video get native
/// browser controls; everything else gets a neutral chip with a
/// mime-aware icon + filename + byte size.
pub(crate) fn render_attachment(
    att: &crate::attachments::ParsedAttachment,
    remove_prefix: Option<&str>,
    lang: Lang,
) -> Html {
    let url = att.url.clone();
    let filename = att.filename.clone();
    let mime = att.mime.clone();
    let size = format_bytes(att.size);
    // The per-attachment remove (×) control — only when `remove_prefix` is
    // set (owner viewing their own conversation; None for shared/read-only
    // views). The POST target is `<prefix>/<filename>/remove`; the filename
    // is percent-encoded so spaces / unicode don't break the URL path.
    let remove_btn = remove_prefix.map(|prefix| {
        let remove_url = format!("{prefix}/{}/remove", urlencode_path_segment(&filename));
        render_attachment_remove(&remove_url, &filename, lang)
    });
    // Every attachment sits in a `position: relative` wrapper so the ×
    // can pin to its top-right corner regardless of media type.
    let wrap = |inner: Html| {
        html! {
            div(class: "chat-msg__attachment") {
                (inner)
                if let Some(btn) = remove_btn.clone() { (btn) }
            }
        }
        .to_html()
    };
    if att.is_image() {
        let alt = filename.clone();
        // A preview image (e.g. a typst render's PNG) clicks through to
        // its `link` (the PDF) when set; an ordinary image links to its
        // own full-res bytes. The `<img src>` is always the image url.
        let href = att.link.clone().unwrap_or_else(|| url.clone());
        let title = match &att.link {
            Some(_) => t_args(
                lang,
                "render-attachment-open-title",
                &i18n::args([
                    ("filename", filename.clone().into()),
                    ("mime", mime.clone().into()),
                    ("size", size.clone().into()),
                ]),
            ),
            None => t_args(
                lang,
                "render-attachment-title",
                &i18n::args([
                    ("filename", filename.clone().into()),
                    ("mime", mime.clone().into()),
                    ("size", size.clone().into()),
                ]),
            ),
        };
        return wrap(
            html! {
                a(href: (href), target: "_blank", rel: "noopener", class: "chat-msg__attachment-image") {
                    img(src: (url), alt: (alt), title: (title), loading: "lazy");
                }
            }
            .to_html(),
        );
    }
    if mime.starts_with("video/") {
        let title = t_args(
            lang,
            "render-attachment-title",
            &i18n::args([
                ("filename", filename.clone().into()),
                ("mime", mime.clone().into()),
                ("size", size.clone().into()),
            ]),
        );
        return wrap(
            html! {
                div(class: "chat-msg__attachment-player") {
                    video(
                        src: (url.clone()),
                        controls: "controls",
                        preload: "metadata",
                        title: (title)
                    ) {}
                    a(href: (url), target: "_blank", rel: "noopener", class: "chat-msg__attachment-player-meta") {
                        span(class: "chat-msg__attachment-name") { (filename) }
                        span(class: "chat-msg__attachment-meta") { (size) }
                    }
                }
            }
            .to_html(),
        );
    }
    if mime.starts_with("audio/") {
        let title = t_args(
            lang,
            "render-attachment-title",
            &i18n::args([
                ("filename", filename.clone().into()),
                ("mime", mime.clone().into()),
                ("size", size.clone().into()),
            ]),
        );
        return wrap(
            html! {
                div(class: "chat-msg__attachment-player chat-msg__attachment-player--audio") {
                    audio(
                        src: (url.clone()),
                        controls: "controls",
                        preload: "metadata",
                        title: (title)
                    ) {}
                    a(href: (url), target: "_blank", rel: "noopener", class: "chat-msg__attachment-player-meta") {
                        span(class: "chat-msg__attachment-name") { (filename) }
                        span(class: "chat-msg__attachment-meta") { (size) }
                    }
                }
            }
            .to_html(),
        );
    }
    let chip_title = t_args(
        lang,
        "render-attachment-chip-title",
        &i18n::args([("mime", mime.clone().into()), ("size", size.clone().into())]),
    );
    wrap(
        html! {
            a(
                href: (url.clone()),
                target: "_blank",
                rel: "noopener",
                class: "chat-msg__attachment-chip",
                title: (chip_title)
            ) {
                span(class: "chat-msg__attachment-icon") { (icons::paperclip(14)) }
                span(class: "chat-msg__attachment-name") { (filename) }
                span(class: "chat-msg__attachment-meta") { (size) }
            }
        }
        .to_html(),
    )
}

/// The hover "×" control that removes one attachment from a message.
/// Confirms, then `@post`s to the removal endpoint whose SSE response
/// re-renders this turn without the attachment (and reclaims the S3
/// object server-side). No hidden form / JS glue — datastar's `@post`
/// handles the request and the element patch.
pub(crate) fn render_attachment_remove(remove_url: &str, filename: &str, lang: Lang) -> Html {
    let confirm = t_args(
        lang,
        "render-attachment-remove-confirm",
        &i18n::args([("filename", filename.to_string().into())]),
    );
    let aria = t(lang, "render-attachment-remove-aria");
    let confirm_js = serde_json::to_string(&confirm).expect("String always serialises");
    let directive = format!("confirm({confirm_js}) && @post('{remove_url}')");
    html! {
        button(
            type: "button",
            class: "chat-msg__attachment-remove",
            "aria-label": (aria),
            title: (aria),
            "data-on:click": (directive)
        ) {
            "×"
        }
    }
    .to_html()
}

/// Percent-encode a filename as a single URL path segment for the
/// removal endpoint — mirrors the gateway's `proxy_url` encoding so a
/// name with spaces / unicode survives the round-trip. The upload path
/// rejects `/`, so the name is always one path component.
pub(crate) fn urlencode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The gallery "kind" of an attachment — `Some("image"|"video"|"audio")`
/// for media that participates in the numbered gallery, `None` for plain
/// file chips (pdf, csv, …) which stay inline and unnumbered.
pub(crate) fn media_kind(att: &crate::attachments::ParsedAttachment) -> Option<&'static str> {
    if att.is_image() {
        Some("image")
    } else if att.mime.starts_with("video/") {
        Some("video")
    } else if att.mime.starts_with("audio/") {
        Some("audio")
    } else {
        None
    }
}

/// One piece of a rendered message body: a pre-built block (prose/text),
/// a numbered media attachment, or a plain file attachment.
pub(crate) enum BodyPiece {
    /// Pre-rendered block HTML (markdown prose or an escaped user-text
    /// slot) — emitted verbatim.
    Block(Html),
    /// An image/video/audio attachment — grouped into the numbered gallery
    /// when a reply carries two or more.
    Media(crate::attachments::ParsedAttachment),
    /// A non-media attachment (pdf, csv, …) — rendered as an inline chip,
    /// never numbered.
    File(crate::attachments::ParsedAttachment),
}

/// One media tile inside a gallery: the attachment plus, when the reply
/// carries 2+ media, a "Image 2 / Video 1 …" caption so the reader can
/// reference it in the next message ("turn the 2nd image into a video").
pub(crate) fn render_media_tile(
    att: &crate::attachments::ParsedAttachment,
    kind: &str,
    n: usize,
    numbered: bool,
    remove_prefix: Option<&str>,
    lang: Lang,
) -> Html {
    let inner = render_attachment(att, remove_prefix, lang);
    if !numbered {
        return html! { div(class: "chat-media") { (inner) } }.to_html();
    }
    let label = t_args(
        lang,
        "render-media-label",
        &i18n::args([
            ("kind", kind.to_string().into()),
            ("n", n.to_string().into()),
        ]),
    );
    html! {
        div(class: "chat-media") {
            div(class: "chat-media__label") { (label) }
            (inner)
        }
    }
    .to_html()
}

/// Render a message body from its pieces, coalescing consecutive media
/// attachments into a side-by-side `chat-media-gallery`. Media are
/// numbered per kind within the reply (Image 1, Image 2, Video 1, …).
///
/// Grouping + numbering only engage when the body holds 2+ media — a lone
/// image/video renders inline exactly as before, so the single-media and
/// text-only cases (and their streaming-morph output) are unchanged.
pub(crate) fn render_body(
    pieces: &[BodyPiece],
    remove_prefix: Option<&str>,
    lang: Lang,
) -> Vec<Html> {
    let total_media = pieces
        .iter()
        .filter(|p| matches!(p, BodyPiece::Media(_)))
        .count();
    if total_media < 2 {
        return pieces
            .iter()
            .map(|p| match p {
                BodyPiece::Block(h) => h.clone(),
                BodyPiece::File(att) | BodyPiece::Media(att) => {
                    render_attachment(att, remove_prefix, lang)
                }
            })
            .collect();
    }
    let mut counters: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    let mut out: Vec<Html> = Vec::with_capacity(pieces.len());
    let mut i = 0;
    while i < pieces.len() {
        match &pieces[i] {
            BodyPiece::Block(h) => {
                out.push(h.clone());
                i += 1;
            }
            BodyPiece::File(att) => {
                out.push(render_attachment(att, remove_prefix, lang));
                i += 1;
            }
            BodyPiece::Media(_) => {
                // Consume the run of consecutive media into one gallery.
                let mut tiles: Vec<Html> = Vec::new();
                while let Some(BodyPiece::Media(att)) = pieces.get(i) {
                    let kind = media_kind(att).unwrap_or("other");
                    let n = {
                        let c = counters.entry(kind).or_insert(0);
                        *c += 1;
                        *c
                    };
                    tiles.push(render_media_tile(att, kind, n, true, remove_prefix, lang));
                    i += 1;
                }
                out.push(
                    html! {
                        div(class: "chat-media-gallery") {
                            for t in tiles.iter() { (t.clone()) }
                        }
                    }
                    .to_html(),
                );
            }
        }
    }
    out
}

pub fn format_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    format!("{mb:.1} MB")
}

/// Char-bounded truncation with a trailing `…` when the string was cut.
/// Char-based (not byte-based) so it never splits a UTF-8 sequence.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
