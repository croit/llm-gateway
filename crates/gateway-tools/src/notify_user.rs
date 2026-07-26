// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `notify_user` — send the user a Web Push notification mid-turn.
//!
//! The push stack already existed but had exactly one caller: the chat page
//! pings the user's browsers when a turn they started finishes. That covers
//! the case where the user is waiting for *this* answer, and nothing else.
//!
//! Two cases it doesn't cover, both of which this tool exists for:
//!
//!   * **Long work.** A sandbox job, a ComfyUI video, a large Typst render —
//!     the user goes and does something else. The turn-complete ping arrives
//!     eventually, but the model can't say *what* it found, and can't ping at
//!     the moment the interesting thing happened rather than at the end.
//!   * **Scheduled actions.** An action that fires at 06:00 writes its reply
//!     into a conversation nobody has open. Without a notification the result
//!     sits there until the user happens to look — which for "the backup
//!     failed" is the whole value gone.
//!
//! Deliberately **not** chat-only: a notification lands on the user's phone,
//! not in a conversation, so it works from the headless scheduler path (which
//! has a session but no browser) and from `/v1`. What it does need is a
//! configured `[push]` VAPID key and at least one subscribed browser; both
//! absences are reported as errors that tell the model to say so in its reply
//! rather than to retry.
//!
//! **One notification per turn**, latched in [`PushNotifier`]. A model that
//! can push in a loop is spam on someone's phone and a fast way to ruin a
//! VAPID key's standing with the push services.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_core::server::db::push_subscriptions;
use gateway_features::server::push::{PushMessage, SendOutcome};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Notification text bounds. The whole payload rides in one aes128gcm record
/// with a ~4 KB budget (and FCM caps the body at 4 KB too), and a phone
/// notification truncates long text anyway — so cap it here, where the model
/// gets told, rather than letting the push service silently cut it.
const MAX_TITLE_LEN: usize = 80;
const MAX_BODY_LEN: usize = 300;

pub struct NotifyUser;

#[derive(Deserialize)]
struct NotifyArgs {
    title: String,
    body: String,
    #[serde(default)]
    url: Option<String>,
}

impl Tool for NotifyUser {
    fn id(&self) -> &str {
        "notify_user"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Send the user a push notification on their phone or desktop, for \
             when they are NOT watching this conversation. Use it in exactly two \
             situations: long work has finished or produced something the user \
             needs to act on (a sandbox job, a long render, a large analysis), \
             or you are running as a scheduled action and found something worth \
             waking someone for. \
             \
             Do NOT use it to confirm an ordinary reply, to say you are starting \
             work, or to report progress — the user is reading your answer, and a \
             notification for that is an interruption with no content. Only ONE \
             notification is allowed per turn, so make it the one that matters, \
             and put the finding in the `body` (\"backup on node3 failed 3 \
             nights running\"), not a pointer to it (\"your report is ready\"). \
             If the user has no subscribed device this returns an error — then \
             just say so in your reply.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "body"],
                "properties": {
                    "title": {
                        "type": "string",
                        "description": format!(
                            "Heading, in the user's language. Max {MAX_TITLE_LEN} \
                             characters — a phone shows one line."
                        )
                    },
                    "body": {
                        "type": "string",
                        "description": format!(
                            "The substance: what happened and what it means, in one \
                             or two sentences. Max {MAX_BODY_LEN} characters."
                        )
                    },
                    "url": {
                        "type": "string",
                        "description": "Optional same-origin path to open when the user \
                                        taps the notification, e.g. `/chat/<session_id>` \
                                        for this conversation. Defaults to the current \
                                        conversation when there is one."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: NotifyArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{title: string, body: string, url?: string}}: {e}"
                ))
            })?;
            let (title, body) = validate(&args)?;
            let url = resolve_url(args.url.as_deref(), ctx.session_id.as_deref())?;

            let Some(push) = ctx.push.as_ref() else {
                return Err(ToolError::Failed(
                    "push notifications aren't configured on this gateway, so there is no \
                     way to reach the user outside this conversation. Put what you wanted \
                     to notify about in your reply instead."
                        .into(),
                ));
            };

            // Claim before sending, and before the DB read: a second call in the
            // same turn must be refused even if the first is still in flight.
            if !push.claim() {
                return Err(ToolError::Failed(
                    "you already sent a notification this turn — one is the limit. Put \
                     anything else you need to say in your reply."
                        .into(),
                ));
            }

            let subs = push_subscriptions::list_for_user(&ctx.db, &ctx.user_id)
                .await
                .map_err(|e| ToolError::Failed(format!("reading push subscriptions: {e}")))?;
            if subs.is_empty() {
                return Err(ToolError::Failed(
                    "the user has no device subscribed to notifications, so this could not \
                     be delivered. Say what you wanted to notify about in your reply, and \
                     that they can enable notifications in the app to be reached when they \
                     are away."
                        .into(),
                ));
            }

            // Fan out to every subscribed browser. `tag` is the notification's
            // coalescing key: the session id when we have one, so a second
            // notification about the same conversation replaces the first
            // instead of stacking. Gone subscriptions are pruned, exactly as
            // the turn-complete path does.
            let tag = ctx
                .session_id
                .clone()
                .unwrap_or_else(|| format!("notify:{}", ctx.user_id));
            let message = PushMessage {
                title: title.clone(),
                body: body.clone(),
                url: url.clone(),
                tag,
            };
            let mut delivered = 0usize;
            let mut pruned = 0usize;
            for sub in &subs {
                match push.sender().send(sub, &message).await {
                    SendOutcome::Delivered => delivered += 1,
                    SendOutcome::Gone => {
                        pruned += 1;
                        if let Err(err) = push_subscriptions::delete(&ctx.db, &sub.id).await {
                            tracing::warn!(error = %err, "notify_user: pruning gone subscription");
                        }
                    }
                    SendOutcome::Failed => {}
                }
            }

            if delivered == 0 {
                // Every endpoint rejected or vanished. The turn's budget stays
                // spent: retrying would hit the same endpoints.
                return Err(ToolError::Failed(format!(
                    "the notification could not be delivered to any of the user's \
                     {} registered device(s){}. Say what you wanted to notify about in \
                     your reply instead.",
                    subs.len(),
                    if pruned > 0 {
                        format!(" ({pruned} had expired and were removed)")
                    } else {
                        String::new()
                    }
                )));
            }

            Ok(json!({
                "delivered": true,
                "devices": delivered,
                "title": title,
                "body": body,
                "url": url,
                "status": "Notification sent. Do not send another this turn; continue with \
                           your reply, and don't repeat the notification text as if the \
                           user hasn't seen it — they have.",
            }))
        })
    }
}

/// Trim + bound the two text fields.
fn validate(args: &NotifyArgs) -> Result<(String, String), ToolError> {
    let title = args.title.trim().to_string();
    if title.is_empty() {
        return Err(ToolError::InvalidArgs("`title` must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_LEN {
        return Err(ToolError::InvalidArgs(format!(
            "`title` is too long; keep it under {MAX_TITLE_LEN} characters"
        )));
    }
    let body = args.body.trim().to_string();
    if body.is_empty() {
        return Err(ToolError::InvalidArgs(
            "`body` must not be empty — a notification with no substance is just noise".into(),
        ));
    }
    if body.chars().count() > MAX_BODY_LEN {
        return Err(ToolError::InvalidArgs(format!(
            "`body` is too long; keep it under {MAX_BODY_LEN} characters"
        )));
    }
    Ok((title, body))
}

/// Where tapping the notification goes.
///
/// Same-origin paths only. The service worker hands this to
/// `clients.openWindow`, so an absolute URL would let a model that just read
/// an attacker-controlled page turn a trusted-looking notification into a
/// link to anywhere — a phishing primitive with the gateway's own branding on
/// it. A leading `//` is rejected for the same reason: `//evil.example` is a
/// protocol-relative *absolute* URL, not a path.
fn resolve_url(requested: Option<&str>, session_id: Option<&str>) -> Result<String, ToolError> {
    let fallback = || match session_id {
        Some(s) => format!("/chat/{s}"),
        None => "/".to_string(),
    };
    let Some(raw) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(fallback());
    };
    if !raw.starts_with('/') || raw.starts_with("//") {
        return Err(ToolError::InvalidArgs(format!(
            "`url` must be a path on this gateway starting with a single `/` \
             (e.g. `/chat/<session_id>`); got {raw:?}"
        )));
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db;

    async fn pool() -> db::Pool {
        db::open(std::path::Path::new(":memory:")).await.unwrap()
    }

    fn args(title: &str, body: &str) -> Value {
        json!({"title": title, "body": body})
    }

    /// Without a configured VAPID key there is nothing to send with, and the
    /// model must be told to fall back to its reply rather than retry.
    #[tokio::test]
    async fn without_push_configured_it_says_so() {
        let ctx = ToolContext::for_test(pool().await);
        let err = NotifyUser
            .run(ctx, args("Done", "The backup finished."))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("aren't configured"), "{err}");
        assert!(err.contains("your reply"), "must redirect the model: {err}");
    }

    /// A real `PushNotifier` over an in-memory DB — enough to exercise the
    /// budget and the no-subscription path without a push service.
    async fn ctx_with_push(pool: &db::Pool) -> ToolContext {
        let crypto = gateway_core::server::crypto::Crypto::ephemeral();
        let sender = gateway_features::server::push::PushSender::new(
            pool,
            &crypto,
            "mailto:ops@example.com".into(),
        )
        .await
        .expect("VAPID keypair generates against an in-memory DB");
        ToolContext {
            session_id: Some("s1".into()),
            push: Some(gateway_runtime::server::tools::PushNotifier::new(
                std::sync::Arc::new(sender),
            )),
            ..ToolContext::for_test(pool.clone())
        }
    }

    /// The rate limit is the point of `PushNotifier`: the turn's budget is one
    /// notification, and the second call is refused before it can reach a push
    /// endpoint.
    #[tokio::test]
    async fn the_turn_budget_is_one_notification() {
        let pool = pool().await;
        let ctx = ctx_with_push(&pool).await;
        let notifier = ctx.push.clone().unwrap();
        assert!(notifier.claim(), "first claim must succeed");
        assert!(!notifier.claim(), "second claim must be refused");

        // And the same latch is what the tool consults: with the budget
        // already spent, a call is refused rather than reporting no devices.
        let err = NotifyUser
            .run(ctx, args("Done", "Backup finished."))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already sent a notification this turn"),
            "{err}"
        );
    }

    /// A fresh context (= a new turn) gets a fresh budget. Without this the
    /// latch would have to be reset somewhere, and nothing would notice if it
    /// weren't.
    #[tokio::test]
    async fn each_turn_gets_its_own_budget() {
        let pool = pool().await;
        assert!(ctx_with_push(&pool).await.push.unwrap().claim());
        assert!(ctx_with_push(&pool).await.push.unwrap().claim());
    }

    /// Clones share the latch: `ToolContext` is cloned per tool call inside a
    /// round, so a per-clone budget would be no budget at all.
    #[tokio::test]
    async fn cloned_contexts_share_the_turn_budget() {
        let pool = pool().await;
        let ctx = ctx_with_push(&pool).await;
        let second = ctx.clone();
        assert!(ctx.push.unwrap().claim());
        assert!(
            !second.push.unwrap().claim(),
            "a cloned context must not get a second notification"
        );
    }

    /// Push is configured but nothing is subscribed: the model has to learn
    /// that the message did not reach anyone, and say so in its reply.
    #[tokio::test]
    async fn with_no_subscribed_device_it_reports_undeliverable() {
        let pool = pool().await;
        let err = NotifyUser
            .run(ctx_with_push(&pool).await, args("Done", "All finished."))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no device subscribed"), "{err}");
        assert!(err.contains("your reply"), "must redirect the model: {err}");
    }

    #[test]
    fn title_and_body_are_required_and_bounded() {
        assert!(
            validate(&NotifyArgs {
                title: "  ".into(),
                body: "x".into(),
                url: None
            })
            .is_err()
        );
        assert!(
            validate(&NotifyArgs {
                title: "t".into(),
                body: " ".into(),
                url: None
            })
            .is_err()
        );
        assert!(
            validate(&NotifyArgs {
                title: "t".repeat(MAX_TITLE_LEN + 1),
                body: "b".into(),
                url: None
            })
            .is_err()
        );
        assert!(
            validate(&NotifyArgs {
                title: "t".into(),
                body: "b".repeat(MAX_BODY_LEN + 1),
                url: None
            })
            .is_err()
        );
        let (t, b) = validate(&NotifyArgs {
            title: "  Backup failed  ".into(),
            body: "  node3, three nights  ".into(),
            url: None,
        })
        .unwrap();
        assert_eq!(t, "Backup failed");
        assert_eq!(b, "node3, three nights");
    }

    /// The deeplink defaults to the conversation, and can never leave the
    /// gateway — the service worker opens whatever lands here.
    #[test]
    fn url_defaults_to_the_conversation_and_rejects_offsite_targets() {
        assert_eq!(resolve_url(None, Some("s1")).unwrap(), "/chat/s1");
        assert_eq!(resolve_url(None, None).unwrap(), "/");
        assert_eq!(resolve_url(Some("  "), Some("s1")).unwrap(), "/chat/s1");
        assert_eq!(resolve_url(Some("/scheduled"), None).unwrap(), "/scheduled");

        for bad in [
            "https://evil.example/phish",
            "//evil.example/phish",
            "javascript:alert(1)",
            "chat/s1",
        ] {
            assert!(
                resolve_url(Some(bad), Some("s1")).is_err(),
                "must reject {bad:?}"
            );
        }
    }
}
