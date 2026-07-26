// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `ask_user` — ask the user a question mid-turn and wait for the answer.
//!
//! Without this the model has two bad options when a request is
//! underspecified: guess (and produce work that gets thrown away), or answer
//! with a question and end the turn — which tears down everything the turn had
//! built up. A sandbox lease with a populated `/work`, a canvas document in
//! progress, files staged for a render: all cold by the next message. Asking
//! *inside* the turn keeps the work alive.
//!
//! Mechanically it is the same rendezvous `get_user_location` uses: inject a
//! card onto the live SSE stream, park on the feedback hub keyed by the
//! assistant turn id, and wait for `POST /api/v0/me/ask/feedback/{turn_id}`.
//! See `gateway_runtime::server::tools::feedback`.
//!
//! Chat-path only. Off it there is no browser to ask and no turn to attach a
//! card to, so the tool refuses rather than hanging — and `requires_chat_session`
//! keeps it out of the `/v1` tool list entirely.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_runtime::server::tools::feedback::AskReply;
use gateway_runtime::server::tools::{ChatFeedback, Tool, ToolContext, ToolError, ToolFuture};

/// How long to wait for a human to answer.
///
/// Far longer than the location prompt's wait: that one is answered by the
/// browser in a second or two once the user clicks, whereas this one waits for
/// someone to read a question, think, and possibly type. Short enough that a
/// user who wandered off doesn't pin a worker indefinitely.
const WAIT_SECS: u64 = 180;

/// Ceiling on the per-tool timeout the runner enforces. Must exceed
/// [`WAIT_SECS`] or the runner would cancel the tool while it is still
/// legitimately waiting — the default 30s would.
const MAX_DURATION_SECS: u64 = WAIT_SECS + 15;

/// Bounds on the question itself. A tool call is not the place to render an
/// essay, and an unbounded option list would be a broken UI.
const MAX_QUESTION_LEN: usize = 500;
const MAX_HEADER_LEN: usize = 40;
const MAX_OPTIONS: usize = 4;
const MAX_LABEL_LEN: usize = 80;
const MAX_DESCRIPTION_LEN: usize = 200;

pub struct AskUser;

#[derive(Deserialize)]
struct AskArgs {
    question: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    options: Option<Vec<AskOption>>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Deserialize, Clone, Debug)]
struct AskOption {
    label: String,
    #[serde(default)]
    description: Option<String>,
}

impl Tool for AskUser {
    fn id(&self) -> &str {
        "ask_user"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(MAX_DURATION_SECS))
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Ask the user a question and wait for their answer, without ending your turn. \
             Use it when a genuine choice would change what you build and guessing would \
             waste the work — which database to target, which of several files they meant, \
             whether to overwrite something. Offer `options` when you can enumerate the \
             sensible answers; the user can always type something else instead. \
             \
             Do NOT use it to confirm things you can just do, to ask permission for a \
             normal step, or to check in on progress — a question the user has to answer \
             costs them far more than a wrong guess you can correct. Ask once, with \
             everything you need, rather than several times in a row. If nobody answers, \
             the result says so and you should proceed on a stated assumption.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["question"],
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question, in the user's language. One question, \
                                        phrased so the options (if any) are obvious answers \
                                        to it."
                    },
                    "header": {
                        "type": "string",
                        "description": "Optional short label for the card (a few words, e.g. \
                                        \"Target database\") so the user sees at a glance \
                                        what is being decided."
                    },
                    "options": {
                        "type": "array",
                        "maxItems": MAX_OPTIONS,
                        "description": "Optional list of 2-4 answers to offer as buttons. \
                                        Omit for an open question. A free-text field is \
                                        always shown as well, so never add an \"other\" \
                                        option yourself.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["label"],
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "Short answer text (1-5 words). This is \
                                                    what comes back to you."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Optional one-line explanation of what \
                                                    choosing this means."
                                }
                            }
                        }
                    },
                    "multi_select": {
                        "type": "boolean",
                        "description": "Allow picking several options at once. Default false. \
                                        Only set it when the options genuinely combine."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: AskArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{question: string, header?, options?, multi_select?}}: {e}"
                ))
            })?;
            let prompt = Prompt::validate(args)?;

            // Chat path only: without a live turn there is no card to inject
            // and nobody to answer.
            let (Some(fb), Some(turn_id)) =
                (ctx.chat_feedback.as_ref(), ctx.assistant_turn_id.as_deref())
            else {
                return Err(ToolError::Failed(
                    "ask_user only works inside a chat session — there is no user watching \
                     this request to answer. Proceed on your best assumption and say which \
                     assumption you made."
                        .into(),
                ));
            };

            match request_answer(fb, turn_id, &prompt).await {
                Some(AskReply::Answered { choices, text }) => Ok(json!({
                    "answered": true,
                    "choices": choices,
                    "text": text,
                })),
                Some(AskReply::Dismissed) => Ok(unanswered(
                    "dismissed",
                    "The user dismissed the question without answering.",
                )),
                None => Ok(unanswered(
                    "no_response",
                    "Nobody answered in time (or nobody was watching this conversation).",
                )),
            }
        })
    }
}

/// How a [`confirm`] prompt came back.
#[derive(Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// The user picked the affirmative option.
    Approved,
    /// The user picked the negative option, dismissed the card, or typed
    /// something instead of choosing.
    Declined {
        /// What they said, when they typed rather than clicked — worth
        /// relaying so the model can act on "not Mondays, Tuesdays".
        text: Option<String>,
    },
    /// Nobody answered in time, or nobody was watching.
    NoAnswer,
}

/// Ask the user to confirm an action, reusing `ask_user`'s card + rendezvous.
///
/// Exists so a tool with a persistent side effect can require a human "yes"
/// without reimplementing the SSE card and the hub wait. `schedule_action` is
/// the first caller: an action created from injected text would otherwise run
/// **as the user**, on a schedule, indefinitely — persistence is what makes
/// prompt injection there worth a confirmation step, where an ordinary tool
/// call isn't.
///
/// Returns [`Confirmation::NoAnswer`] off the chat path rather than erroring,
/// so the caller decides what "no human here" means for its own operation.
pub async fn confirm(
    ctx: &ToolContext,
    question: &str,
    header: &str,
    approve_label: &str,
    decline_label: &str,
) -> Confirmation {
    let (Some(fb), Some(turn_id)) = (ctx.chat_feedback.as_ref(), ctx.assistant_turn_id.as_deref())
    else {
        return Confirmation::NoAnswer;
    };
    let prompt = Prompt {
        question: question.to_string(),
        header: Some(header.to_string()),
        options: vec![
            AskOption {
                label: approve_label.to_string(),
                description: None,
            },
            AskOption {
                label: decline_label.to_string(),
                description: None,
            },
        ],
        multi_select: false,
    };
    match request_answer(fb, turn_id, &prompt).await {
        Some(AskReply::Answered { choices, text }) => {
            // A click on the affirmative button is the only "yes". Free text
            // is never read as approval: "yes but move it to 07:00" is a
            // change request, and treating it as consent would write the
            // wrong thing.
            if choices.iter().any(|c| c == approve_label) {
                Confirmation::Approved
            } else {
                Confirmation::Declined {
                    text: text.filter(|t| !t.trim().is_empty()),
                }
            }
        }
        Some(AskReply::Dismissed) => Confirmation::Declined { text: None },
        None => Confirmation::NoAnswer,
    }
}

/// A validated question, ready to render.
#[derive(Debug)]
struct Prompt {
    question: String,
    header: Option<String>,
    options: Vec<AskOption>,
    multi_select: bool,
}

impl Prompt {
    fn validate(args: AskArgs) -> Result<Self, ToolError> {
        let question = args.question.trim().to_string();
        if question.is_empty() {
            return Err(ToolError::InvalidArgs("question must not be empty".into()));
        }
        if question.chars().count() > MAX_QUESTION_LEN {
            return Err(ToolError::InvalidArgs(format!(
                "question too long; keep it under {MAX_QUESTION_LEN} characters"
            )));
        }
        let header = args
            .header
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());
        if let Some(h) = &header
            && h.chars().count() > MAX_HEADER_LEN
        {
            return Err(ToolError::InvalidArgs(format!(
                "header too long; keep it under {MAX_HEADER_LEN} characters"
            )));
        }

        let mut options = Vec::new();
        for opt in args.options.unwrap_or_default() {
            let label = opt.label.trim().to_string();
            // A blank label would render an unclickable button and come back
            // as an empty answer.
            if label.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "every option needs a non-empty label".into(),
                ));
            }
            if label.chars().count() > MAX_LABEL_LEN {
                return Err(ToolError::InvalidArgs(format!(
                    "option label `{label}` is too long; keep labels under {MAX_LABEL_LEN} \
                     characters"
                )));
            }
            let description = opt
                .description
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty());
            if let Some(d) = &description
                && d.chars().count() > MAX_DESCRIPTION_LEN
            {
                return Err(ToolError::InvalidArgs(format!(
                    "option description for `{label}` is too long; keep it under \
                     {MAX_DESCRIPTION_LEN} characters"
                )));
            }
            options.push(AskOption { label, description });
        }
        if options.len() > MAX_OPTIONS {
            return Err(ToolError::InvalidArgs(format!(
                "at most {MAX_OPTIONS} options (got {})",
                options.len()
            )));
        }
        // One option is a yes/no dressed up as a choice — the free-text field
        // is already the "something else" path, so a single button adds nothing
        // but a misleading UI.
        if options.len() == 1 {
            return Err(ToolError::InvalidArgs(
                "offer either no options (open question) or at least two".into(),
            ));
        }

        Ok(Self {
            question,
            header,
            options,
            multi_select: args.multi_select,
        })
    }
}

/// The shared shape for "we got no usable answer".
///
/// Deliberately a successful result with `answered: false` rather than an
/// error: the model's next step is the same either way (assume and say so),
/// and returning an error tends to make models retry the question.
fn unanswered(reason: &str, note: &str) -> Value {
    json!({
        "answered": false,
        "reason": reason,
        "note": format!("{note} Continue with your best assumption and state it explicitly \
                         in your reply so the user can correct you."),
    })
}

/// Inject the question, park on the hub, tear the card down again.
///
/// Mirrors `location::request_browser_location`; see the comments there for
/// why the card is appended to `#conversation` (a `#turn-<id>` patch would
/// clobber it on the next tick) and why the append + scroll ride in one frame.
async fn request_answer(fb: &ChatFeedback, turn_id: &str, prompt: &Prompt) -> Option<AskReply> {
    use session_core::workers::TurnUpdate;

    // Nobody subscribed → nobody can answer. The timeout below is the real
    // backstop if the stream drops right after this check.
    if fb.broadcast.receiver_count() == 0 {
        return None;
    }

    let rx = fb.ask_hub.register(turn_id);

    let card = prompt_card_html(turn_id, prompt);
    let mut frame =
        session_core::chrome::sse_patch(Some("#conversation"), Some("append"), &card).to_vec();
    let scroll = session_core::chrome::sse_script(&format!(
        "document.getElementById('ask-prompt-{turn_id}')\
         ?.scrollIntoView({{block:'center',behavior:'smooth'}});"
    ));
    frame.extend_from_slice(&scroll);
    let _ = fb
        .broadcast
        .send(TurnUpdate::Inject(std::sync::Arc::new(frame.into())));

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), rx).await;

    // Tear down regardless of how the wait ended. The client removes the card
    // itself on submit; this covers timeout and dismissal-by-navigation.
    let cleanup = session_core::chrome::sse_script(&format!(
        "document.getElementById('ask-prompt-{turn_id}')?.remove();"
    ));
    let _ = fb
        .broadcast
        .send(TurnUpdate::Inject(std::sync::Arc::new(cleanup)));

    match outcome {
        Ok(Ok(reply)) => Some(reply),
        // Timed out, or the sender was dropped.
        _ => {
            fb.ask_hub.cancel(turn_id);
            None
        }
    }
}

/// Escape text for interpolation into the card's HTML.
///
/// The question and the option labels are **model-generated**, and a model can
/// be steered by the page it just read — so this is not merely defensive
/// hygiene. Hand-rolled because the card is assembled as a string (the same way
/// `location`'s is) rather than through plait's auto-escaping templates.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape for a single-quoted JavaScript string literal inside an attribute.
/// Runs *after* [`esc`], so the value is safe in both contexts.
fn esc_js(s: &str) -> String {
    esc(&s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The injected card: the question, optional choice buttons, and a free-text
/// field with a submit button. Appended to `#conversation` so it reads as the
/// assistant asking, just under the in-progress reply.
///
/// `turn_id` is a UUID, so it is safe to interpolate into element ids and the
/// `window.ask.*` calls.
fn prompt_card_html(turn_id: &str, prompt: &Prompt) -> String {
    let mut html = String::with_capacity(512);
    html.push_str(&format!(
        "<div id=\"ask-prompt-{tid}\" \
           class=\"alert bg-base-100 border border-base-300 shadow-sm \
                  flex flex-col items-start gap-2 self-start max-w-md\">",
        tid = turn_id
    ));
    if let Some(header) = &prompt.header {
        html.push_str(&format!(
            "<span class=\"badge badge-sm badge-ghost\">{}</span>",
            esc(header)
        ));
    }
    html.push_str(&format!(
        "<span class=\"text-sm\">\u{2753} {}</span>",
        esc(&prompt.question)
    ));

    if !prompt.options.is_empty() {
        let multi = if prompt.multi_select { "true" } else { "false" };
        html.push_str("<div class=\"flex flex-col gap-1 w-full\">");
        for opt in &prompt.options {
            html.push_str(&format!(
                "<button type=\"button\" class=\"btn btn-sm btn-outline justify-start \
                   h-auto py-1 flex-col items-start gap-0\" \
                   data-ask-option=\"{label_attr}\" \
                   data-on:click=\"window.ask.pick('{tid}', this, {multi})\">\
                   <span class=\"text-sm font-medium\">{label}</span>",
                tid = turn_id,
                multi = multi,
                label_attr = esc(&opt.label),
                label = esc(&opt.label),
            ));
            if let Some(d) = &opt.description {
                html.push_str(&format!(
                    "<span class=\"text-xs opacity-70 font-normal\">{}</span>",
                    esc(d)
                ));
            }
            html.push_str("</button>");
        }
        html.push_str("</div>");
    }

    // Always offered, even with options: forcing a user into a preset answer
    // when none fits produces a confidently wrong turn.
    let placeholder = if prompt.options.is_empty() {
        "Your answer…"
    } else {
        "Something else…"
    };
    html.push_str(&format!(
        "<div class=\"flex gap-2 w-full\">\
           <input type=\"text\" id=\"ask-text-{tid}\" placeholder=\"{ph}\" \
             class=\"input input-bordered input-sm flex-1\" \
             data-on:keydown=\"evt.key === 'Enter' && window.ask.submit('{tid_js}')\">\
           <button type=\"button\" class=\"btn btn-sm btn-primary\" \
             data-on:click=\"window.ask.submit('{tid_js}')\">Send</button>\
         </div>\
         <button type=\"button\" class=\"btn btn-xs btn-ghost self-end\" \
           data-on:click=\"window.ask.dismiss('{tid_js}')\">Skip</button>\
         </div>",
        tid = turn_id,
        tid_js = esc_js(turn_id),
        ph = placeholder,
    ));
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db;

    fn args(v: Value) -> AskArgs {
        serde_json::from_value(v).expect("valid args")
    }

    async fn ctx_off_chat() -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext::for_test(pool)
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(AskUser.id(), AskUser.schema().function.name);
    }

    /// The runner's default per-tool timeout is 30s — far below the human wait,
    /// so without the override the tool would be cancelled mid-question.
    #[test]
    fn max_duration_outlasts_the_human_wait() {
        let d = AskUser.max_duration().expect("must override the default");
        assert!(
            d.as_secs() > WAIT_SECS,
            "{}s must exceed the {WAIT_SECS}s wait",
            d.as_secs()
        );
    }

    #[test]
    fn schema_advertises_the_option_bounds() {
        let schema = AskUser.schema();
        let props = &schema.function.parameters["properties"];
        assert_eq!(props["options"]["maxItems"], MAX_OPTIONS);
        assert_eq!(props["multi_select"]["type"], "boolean");
        assert_eq!(schema.function.parameters["required"], json!(["question"]));
    }

    #[test]
    fn rejects_an_empty_question() {
        assert!(matches!(
            Prompt::validate(args(json!({"question": "   "}))).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn rejects_a_single_option() {
        // One button plus a free-text field is a misleading UI, not a choice.
        let err = Prompt::validate(args(json!({
            "question": "Which?",
            "options": [{"label": "only one"}]
        })))
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[test]
    fn rejects_a_blank_option_label() {
        assert!(matches!(
            Prompt::validate(args(json!({
                "question": "Which?",
                "options": [{"label": " "}, {"label": "b"}]
            })))
            .unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn rejects_too_many_options() {
        let many: Vec<Value> = (0..MAX_OPTIONS + 1)
            .map(|i| json!({"label": format!("opt{i}")}))
            .collect();
        assert!(matches!(
            Prompt::validate(args(json!({"question": "Which?", "options": many}))).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn accepts_an_open_question_and_a_normal_choice() {
        let open = Prompt::validate(args(json!({"question": "Which region?"}))).unwrap();
        assert!(open.options.is_empty());
        assert!(!open.multi_select);

        let choice = Prompt::validate(args(json!({
            "question": "Which database?",
            "header": "Target",
            "multi_select": true,
            "options": [
                {"label": "Postgres", "description": "the primary"},
                {"label": "SQLite"}
            ]
        })))
        .unwrap();
        assert_eq!(choice.options.len(), 2);
        assert_eq!(choice.header.as_deref(), Some("Target"));
        assert!(choice.multi_select);
    }

    #[tokio::test]
    async fn off_the_chat_path_it_refuses_with_actionable_advice() {
        // /v1 callers have no browser. requires_chat_session should keep the
        // tool out of their list entirely, but the runtime gate has to hold
        // regardless — and tell the model what to do instead of retrying.
        let err = AskUser
            .run(ctx_off_chat().await, json!({"question": "Which one?"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("assumption"), "{msg}");
    }

    #[test]
    fn card_escapes_model_supplied_text() {
        // The question and labels come from the model, which may itself have
        // been steered by a page it just read. They must not be able to inject
        // markup into the chat DOM.
        let prompt = Prompt::validate(args(json!({
            "question": "<img src=x onerror=alert(1)> & \"quoted\"",
            "options": [
                {"label": "</button><script>evil()</script>"},
                {"label": "it's fine", "description": "<b>bold</b>"}
            ]
        })))
        .unwrap();
        let html = prompt_card_html("turn-1", &prompt);
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>bold"), "{html}");
        assert!(html.contains("&lt;img"), "{html}");
        assert!(html.contains("&#39;"), "apostrophe must be escaped: {html}");
    }

    #[test]
    fn card_wires_every_control_to_the_client_helpers() {
        let prompt = Prompt::validate(args(json!({
            "question": "Which?",
            "options": [{"label": "a"}, {"label": "b"}]
        })))
        .unwrap();
        let html = prompt_card_html("t-9", &prompt);
        // The element id the server's own teardown script targets.
        assert!(html.contains("id=\"ask-prompt-t-9\""), "{html}");
        assert!(html.contains("id=\"ask-text-t-9\""), "{html}");
        // Every handler the client must expose.
        assert!(html.contains("window.ask.pick('t-9'"), "{html}");
        assert!(html.contains("window.ask.submit('t-9')"), "{html}");
        assert!(html.contains("window.ask.dismiss('t-9')"), "{html}");
        // Option labels ride on the element so the client can read them back.
        assert!(html.contains("data-ask-option=\"a\""), "{html}");
    }

    #[test]
    fn open_question_still_offers_free_text_and_no_option_buttons() {
        let prompt = Prompt::validate(args(json!({"question": "Which region?"}))).unwrap();
        let html = prompt_card_html("t-1", &prompt);
        assert!(html.contains("ask-text-t-1"), "{html}");
        assert!(!html.contains("data-ask-option"), "{html}");
        assert!(html.contains("Your answer"), "{html}");
    }

    #[test]
    fn multi_select_is_passed_to_the_client() {
        for (multi, expected) in [(true, "true"), (false, "false")] {
            let prompt = Prompt::validate(args(json!({
                "question": "Which?",
                "multi_select": multi,
                "options": [{"label": "a"}, {"label": "b"}]
            })))
            .unwrap();
            let html = prompt_card_html("t", &prompt);
            assert!(
                html.contains(&format!("this, {expected})")),
                "multi_select={multi} should pass {expected}: {html}"
            );
        }
    }

    #[test]
    fn unanswered_results_tell_the_model_to_assume_and_say_so() {
        for v in [
            unanswered("dismissed", "The user dismissed it."),
            unanswered("no_response", "Nobody answered."),
        ] {
            assert_eq!(v["answered"], false);
            let note = v["note"].as_str().unwrap();
            assert!(note.contains("assumption"), "{note}");
        }
    }
}
