// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// Knobs the composer renderer needs that aren't universal across
/// drivers.
pub struct ComposerOpts<'a> {
    /// Where the form submits. `/chat/{id}/messages` for the gateway.
    pub post_url: &'a str,
    /// Where the stop button posts. `/chat/{id}/cancel` for the
    /// gateway.
    pub cancel_url: &'a str,
    /// Textarea placeholder.
    pub placeholder: &'a str,
    /// Voice-input mic button. The gateway shows this when the user
    /// has a transcription model available.
    pub has_voice: bool,
    /// Voice-conversation mode: show the speak-back toggle + push-to-talk
    /// control. The gateway sets this only when a `speech` (TTS) pool is
    /// configured *and* transcription is available (the full loop needs both).
    pub voice_out: bool,
    /// Initial value of the `$chatStreaming` signal. `true` when a turn is
    /// already in flight at render time, so the Stop control shows on a
    /// fresh load / reload — not just after a submit set the signal in JS.
    /// Without this, reloading mid-turn leaves no way to stop a runaway.
    pub streaming: bool,
    /// Optional toolbar row rendered inside the composer, above the input —
    /// the host app's per-message controls (the gateway puts its "+" tools /
    /// integrations / skills menu here). `None` renders no row. Must contain
    /// no `<form>` (the composer itself is a form; nested forms are invalid) —
    /// use button-driven actions instead.
    pub toolbar: Option<Html>,
    /// UI language for the composer's own labels (attach/record/send/stop).
    pub lang: Lang,
}

pub fn render_composer(opts: ComposerOpts<'_>) -> Html {
    let ComposerOpts {
        post_url,
        cancel_url,
        placeholder,
        has_voice,
        voice_out,
        streaming,
        toolbar,
        lang,
    } = opts;
    let attach_aria = t(lang, "render-composer-attach-aria");
    let attach_title = t(lang, "render-composer-attach-title");
    let record_aria = t(lang, "render-composer-record-aria");
    let record_title = t(lang, "render-composer-record-title");
    let send_label = t(lang, "render-composer-send");
    let stop_label = t(lang, "render-composer-stop");
    let voice_toggle_title = t(lang, "voice-toggle-title");
    let submit_directive = format!(
        "window.chatComposer.onSubmit(evt) && ($chatStreaming = true, \
         @post('{post_url}', {{contentType: 'form'}}))"
    );
    let cancel_directive = format!("@post('{cancel_url}'); $chatStreaming = false");
    let placeholder = placeholder.to_string();
    // Seed `$chatStreaming` from the server's knowledge of whether a turn is
    // live, so Stop is present on load/reload (not only after a JS submit).
    let initial_signals = format!("{{chatStreaming: {streaming}}}");
    // Pre-render the optional toolbar (empty fragment when absent) so it can be
    // interpolated by value inside the macro's `Fn` closure.
    let toolbar_html = toolbar.unwrap_or_else(|| html! { "" }.to_html());
    // The voice-conversation modal (the whole call surface). Emitted as a
    // sibling after the form; hidden until `window.chatVoice.open()`. Empty
    // fragment when the voice loop isn't available.
    let voice_modal_html = if voice_out {
        render_voice_modal(lang)
    } else {
        html! { "" }.to_html()
    };
    html! {
        form(
            id: "chat-form",
            "data-signals": (initial_signals),
            "data-class": "{'chat-composer--streaming': $chatStreaming}",
            "data-on:submit__prevent": (submit_directive),
            "data-on:dragover__prevent": "window.chatComposer.onDragOver(evt)",
            "data-on:dragleave__prevent": "window.chatComposer.onDragLeave(evt)",
            "data-on:drop__prevent": "window.chatComposer.onDrop(evt)",
            "data-on:paste": "window.chatComposer.onPaste(evt)",
            method: "post",
            enctype: "multipart/form-data",
            class: "chat-composer"
        ) {
            // Hidden file input — `name="attachment"` so the
            // backend's multipart parser picks it up; `multiple`
            // accepts batch picks. The composer.ts paste/drop
            // handlers replace `.files` via DataTransfer so all
            // attachment sources flow through this one element.
            input(
                id: "chat-attachments-input",
                name: "attachment",
                type: "file",
                multiple: "multiple",
                hidden: "hidden",
                "data-on:change": "window.chatComposer.onFilesPicked(evt)"
            );
            // Voice-conversation flag: `voice.ts` flips it to "true" before
            // auto-submitting a spoken turn so the worker injects the brevity
            // directive. Serialised with the form's `@post`; harmless "false"
            // otherwise. Only present when the full voice loop is available.
            if voice_out {
                input(id: "chat-voice-flag", name: "voice", type: "hidden", value: "false");
            }
            // Host-app toolbar row (gateway: the "+" tools/integrations/skills
            // menu + active chips). Rendered above the field; contains no form.
            // Empty fragment when none was supplied.
            (toolbar_html.clone())
            // Chip strip — populated by composer.ts as files land.
            // Empty container; CSS hides it while no children.
            div(
                id: "chat-attachments-chips",
                class: "chat-composer__chips"
            ) {}
            div(class: "chat-composer__field") {
                textarea(
                    id: "message",
                    name: "message",
                    rows: "1",
                    placeholder: (placeholder),
                    // Focus the composer on a full page load so the user
                    // can start typing immediately. The Datastar nav path
                    // (+ New chat / switching chats) re-focuses via the
                    // nav script, since `autofocus` only fires on initial
                    // parse.
                    autofocus: "autofocus",
                    "data-on:keydown": "window.chatComposer.onKeydown(evt)",
                    class: "chat-composer__textarea"
                ) {}
                div(class: "chat-composer__action") {
                    // Attach button — opens the hidden file input.
                    button(
                        type: "button",
                        "data-on:click": "window.chatComposer.openFilePicker()",
                        "aria-label": (attach_aria),
                        title: (attach_title),
                        class: "btn btn-sm btn-circle btn-ghost chat-composer__attach"
                    ) {
                        (icons::paperclip(16))
                    }
                    if has_voice {
                        div(class: "voice-control") {
                            div(class: "voice-level", "data-mic-meter": "1") {
                                span {}
                                span {}
                                span {}
                                span {}
                            }
                            button(
                                type: "button",
                                "data-on:click": "window.chatMic.toggle(el)",
                                "aria-label": (record_aria),
                                title: (record_title),
                                class: "btn btn-sm btn-circle btn-ghost data-[recording=1]:btn-error"
                            ) {
                                span(class: "mic-idle") { (icons::mic(16)) }
                                span(class: "mic-recording") { (icons::stop(16)) }
                                span(class: "mic-transcribing") { (icons::spinner(16)) }
                            }
                        }
                    }
                    // Voice-conversation entry: one button that opens the voice
                    // modal (the whole call — PTT, status, captions — lives
                    // there). The dictation mic above stays for text dictation.
                    if voice_out {
                        button(
                            type: "button",
                            "data-on:click": "window.chatVoice.open()",
                            "aria-label": (voice_toggle_title.clone()),
                            title: (voice_toggle_title),
                            class: "btn btn-sm btn-circle btn-ghost chat-voice-toggle"
                        ) {
                            (icons::waveform(16))
                        }
                    }
                    button(
                        type: "submit",
                        class: "btn btn-sm btn-circle btn-primary chat-composer__send",
                        "aria-label": (send_label.clone()),
                        title: (send_label)
                    ) {
                        (icons::send(16))
                    }
                    button(
                        type: "button",
                        "data-on:click": (cancel_directive),
                        class: "btn btn-sm btn-circle btn-error chat-composer__stop",
                        "aria-label": (stop_label.clone()),
                        title: (stop_label)
                    ) {
                        (icons::stop(16))
                    }
                }
            }
        }
        (voice_modal_html)
    }
    .to_html()
}

/// The voice-conversation modal — a native `<dialog>` (opened via
/// `showModal()`, same pattern as the feedback widget) that hosts the whole
/// call: a state-reflecting talk control, a status line, live You/AI captions,
/// and a "recording to chat" reassurance. Backend/persistence are unchanged —
/// turns still flow through the normal chat path and land in the transcript
/// behind. Driven by `voice.ts`; `data-voice-state` (idle|listening|working|
/// speaking) drives the control + status via CSS.
pub(crate) fn render_voice_modal(lang: Lang) -> Html {
    let title = t(lang, "voice-modal-title");
    let close_label = t(lang, "voice-exit-title");
    let talk_label = t(lang, "voice-ptt-title");
    html! {
        dialog(
            id: "voice-modal",
            class: "voice-modal",
            "data-voice-state": "idle",
            "data-voice-greeting": (t(lang, "voice-greeting")),
            // Status strings for each state, so `voice.ts` sets them client-side
            // while the i18n stays server-owned.
            "data-txt-idle": (t(lang, "voice-hint-tap-to-talk")),
            "data-txt-listening": (t(lang, "voice-status-listening")),
            "data-txt-send": (t(lang, "voice-hint-tap-to-send")),
            "data-txt-working": (t(lang, "voice-status-working")),
            "data-txt-speaking": (t(lang, "voice-status-speaking")),
            "data-txt-interrupt": (t(lang, "voice-hint-tap-to-interrupt")),
            "data-txt-notcaught": (t(lang, "voice-not-caught"))
        ) {
            div(class: "voice-modal__box") {
                header(class: "voice-modal__header") {
                    span(class: "voice-modal__title") { (title) }
                    button(
                        type: "button",
                        "data-on:click": "window.chatVoice.close()",
                        "aria-label": (close_label),
                        class: "btn btn-sm btn-circle btn-ghost"
                    ) { "✕" }
                }
                button(
                    type: "button",
                    id: "voice-control",
                    "data-on:click": "window.chatVoice.talk(el)",
                    "aria-label": (talk_label),
                    class: "voice-modal__control"
                ) {
                    // Live frequency-bars visualizer — driven by `voice.ts` off
                    // the mic (listening) and the TTS audio (speaking) via Web
                    // Audio analysers; a calm idle animation otherwise.
                    canvas(id: "voice-viz", class: "voice-modal__viz", width: "180", height: "180") {}
                    span(class: "voice-modal__spin") { (icons::spinner(44)) }
                }
                p(id: "voice-status", class: "voice-modal__status") {
                    (t(lang, "voice-hint-tap-to-talk"))
                }
                div(class: "voice-modal__captions") {
                    p(class: "voice-cap") {
                        span(class: "voice-cap__who") { (t(lang, "voice-caption-you")) }
                        span(id: "voice-cap-user", class: "voice-cap__text") {}
                    }
                    p(class: "voice-cap") {
                        span(class: "voice-cap__who") { (t(lang, "voice-caption-ai")) }
                        span(id: "voice-cap-ai", class: "voice-cap__text") {}
                    }
                }
                p(class: "voice-modal__note") {
                    span(class: "voice-rec-dot") {}
                    (t(lang, "voice-recording-to-chat"))
                }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Busy-button helpers.
//
// Datastar's `data-indicator="<signal>"` directive flips `$<signal>`
// to `true` while a request issued from that element is in flight,
// back to `false` when it settles. Pair that with two spans whose
// visibility is driven by `data-show` and you get a "label →
// spinner → label" swap with no JS on the page side.
//
// The two functions below are the DRY-est expression of that
// pattern across the codebase:
//
//   * `render_busy_post_form` — a single-button form. Wraps the
//     button in a `<form>` with the `@post` directive + indicator
//     wiring. Use this for any one-shot action button (delete row,
//     stop, probe, …). Each call site MUST pass a `busy_signal`
//     unique to that button on the page so concurrent clicks on
//     siblings don't share state. Hashing the action URL is a good
//     default but we leave that to the caller so signal names stay
//     stable across re-renders.
//   * `render_busy_submit` — just the submit button. Use inside
//     bigger forms (multi-field create dialogs) whose `<form>` you
//     already authored with `data-indicator` on it.
//
// Both render `<span data-show="$<signal>" style="display:none">…
// spinner …</span>` for the busy state. Datastar's `data-show`
// flips `style.display` between `none` and an empty string at
// runtime, so the initial render shows the label and never the
// spinner. No FOUC.
//
// Why two spans + `data-show` rather than `data-class` on the
// button: keeps the helper a pure renderer with no CSS-side
// dependency to add or remember elsewhere.

/// Strip everything that isn't `[A-Za-z0-9_]` from `raw` so the
/// result is safe to use as a datastar signal-name suffix (those
/// live in a JS-identifier namespace). Hyphens etc. become
/// underscores; empty input becomes a single underscore so the
/// suffix is never empty.
pub fn sanitize_signal_name(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Action-form spec for `render_busy_post_form`.
pub struct BusyPostForm<'a> {
    /// URL the form POSTs to. Used both as the form's `action`
    /// attribute (for no-JS fallbacks) and inside the `@post(...)`
    /// datastar directive.
    pub action: &'a str,
    /// Label rendered when idle. Plain text — escaped by plait.
    pub label: &'a str,
    /// Label rendered while the request is in flight. "Stopping…",
    /// "Deleting…", "Probing…" — verb in present-continuous.
    pub busy_label: &'a str,
    /// Button-side classes — `btn btn-sm btn-error`, etc. Don't
    /// include `m-0`; the form gets that automatically so it
    /// stacks cleanly next to siblings.
    pub button_class: &'a str,
    /// Signal name backing the data-indicator. Must be unique per
    /// in-flight button on the page (e.g. `"busy_probe_sbx_abc"`).
    /// Allowed chars are JS identifier ones — alnum + underscore.
    pub busy_signal: &'a str,
    /// Optional native `confirm("…")` guard prepended to the
    /// directive. Useful for destructive actions ("Delete this
    /// item?"). Prevents the @post from firing on cancel.
    pub confirm: Option<&'a str>,
    /// Optional `title=` for hover tooltip.
    pub title: Option<&'a str>,
}

/// Render an action button wrapped in a single-button form with
/// loading-state visuals + a data-indicator. See module docs.
pub fn render_busy_post_form(opts: BusyPostForm<'_>) -> Html {
    let post_call = format!("@post('{}', {{contentType: 'form'}})", opts.action);
    let directive = match opts.confirm {
        Some(prompt) => {
            // JSON-encode rather than hand-escape — see `action_submit`'s
            // doc comment for why this is the safe pattern once `prompt`
            // can carry translated text.
            let prompt_js = serde_json::to_string(prompt).expect("String always serialises");
            format!("confirm({prompt_js}) && {post_call}")
        }
        None => post_call,
    };
    let signal_ref = format!("${}", opts.busy_signal);
    let show_idle = format!("!{signal_ref}");
    let show_busy = signal_ref.clone();
    let disabled_attr = signal_ref.clone();
    let action_owned = opts.action.to_string();
    let label_owned = opts.label.to_string();
    let busy_label_owned = opts.busy_label.to_string();
    let class_owned = opts.button_class.to_string();
    let busy_signal_owned = opts.busy_signal.to_string();
    let title_owned = opts.title.map(str::to_string);
    html! {
        form(
            action: (action_owned),
            method: "post",
            class: "m-0",
            "data-indicator": (busy_signal_owned),
            "data-on:submit__prevent": (directive)
        ) {
            button(
                type: "submit",
                class: (class_owned),
                "data-attr-disabled": (disabled_attr),
                title: (title_owned.clone().unwrap_or_default())
            ) {
                span(
                    "data-show": (show_idle),
                    class: "contents"
                ) { (label_owned) }
                span(
                    "data-show": (show_busy),
                    class: "inline-flex items-center gap-2",
                    style: "display:none"
                ) {
                    (icons::spinner(14))
                    span { (busy_label_owned) }
                }
            }
        }
    }
    .to_html()
}

/// Idle/busy submit-button spec for `render_busy_submit`.
pub struct BusySubmit<'a> {
    pub label: &'a str,
    pub busy_label: &'a str,
    pub button_class: &'a str,
    pub busy_signal: &'a str,
}

/// Render a submit button with the same idle/busy swap as
/// `render_busy_post_form`, intended for inclusion inside a larger
/// form whose `data-indicator` already references `busy_signal`.
pub fn render_busy_submit(opts: BusySubmit<'_>) -> Html {
    let signal_ref = format!("${}", opts.busy_signal);
    let show_idle = format!("!{signal_ref}");
    let show_busy = signal_ref.clone();
    let disabled_attr = signal_ref.clone();
    let label_owned = opts.label.to_string();
    let busy_label_owned = opts.busy_label.to_string();
    let class_owned = opts.button_class.to_string();
    html! {
        button(
            type: "submit",
            class: (class_owned),
            "data-attr-disabled": (disabled_attr)
        ) {
            span(
                "data-show": (show_idle),
                class: "contents"
            ) { (label_owned) }
            span(
                "data-show": (show_busy),
                class: "inline-flex items-center gap-2",
                style: "display:none"
            ) {
                (icons::spinner(14))
                span { (busy_label_owned) }
            }
        }
    }
    .to_html()
}
