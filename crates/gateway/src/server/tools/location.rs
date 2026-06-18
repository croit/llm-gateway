// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `get_user_location` — tells the assistant where the caller is so it
//! can answer "what's the weather here?", "restaurants near me", etc.
//! without making the user spell out a city. It does NOT fetch weather;
//! it returns a location the model then uses (e.g. via `search_web`).
//!
//! Resolution precedence, best → coarsest:
//!   1. **Fresh browser position** the user already shared (precise GPS)
//!      — `db::users::find_location`, written by `POST /api/v0/me/location`.
//!      Gated on [`FRESH_SECS`] since a fix goes stale as the user moves.
//!   2. **Interactive prompt** (chat path only): when there's no fresh
//!      fix and the user is watching a live turn, push a "share your
//!      location?" prompt to the browser and *wait* (bounded by
//!      [`WAIT_SECS`]) for them to grant it. This is the feedback loop —
//!      the model's tool call pauses until the browser replies. Declines
//!      / timeouts fall through.
//!   3. **GeoIP** on the caller's source IP (coarse city/country) — works
//!      for *every* caller including bearer-token API use with no browser
//!      (`ctx.client_ip` + `ctx.geoip`). The always-available fallback.
//!   4. **Unknown** — a structured `{known:false}` that tells the model
//!      to just ask the user for a city/region.

use serde_json::{Value, json};
use shared::api::ToolDef;

use super::feedback::BrowserFix;
use super::{ChatFeedback, Tool, ToolContext, ToolFuture};
use crate::server::db::users;

/// How recent a shared browser position must be to be trusted as the
/// user's *current* location. A precise GPS fix goes stale as the user
/// moves; an hour keeps "here" meaningful without re-prompting every
/// message. Older fixes fall through to the interactive prompt / GeoIP.
const FRESH_SECS: i64 = 60 * 60;

/// How long the interactive prompt waits for the browser to reply before
/// giving up and falling back to GeoIP. Kept well under the runner's 30 s
/// per-tool timeout so the tool always returns a result rather than being
/// force-cancelled mid-wait.
const WAIT_SECS: u64 = 22;

pub struct GetUserLocation;

impl Tool for GetUserLocation {
    fn id(&self) -> &str {
        "get_user_location"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Get the user's PRECISE device location (GPS) by asking their browser to \
             share it. The user's IP address and an APPROXIMATE, city-level location \
             are ALREADY provided automatically in your context — answer \"what's my IP\", \
             \"where am I\", \"weather here\", and similar from that directly, WITHOUT \
             calling this tool. Only call this when approximate accuracy isn't enough \
             (e.g. navigation, finding something within a few hundred metres). It may \
             prompt the user to share their device location and falls back to the \
             approximate IP location if they decline; `source`/`precision` say which.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            // 1. Fresh browser-shared position wins.
            match users::find_location(&ctx.db, &ctx.user_id).await {
                Ok(Some(loc)) if loc.is_fresh(FRESH_SECS) => {
                    return Ok(precise_payload(loc.lat, loc.lon, loc.accuracy));
                }
                Ok(_) => {}
                Err(err) => {
                    // A DB hiccup on the precise position shouldn't fail
                    // the call — fall through to GeoIP.
                    tracing::warn!(error = %err, "get_user_location: find_location failed");
                }
            }

            // 2. Interactive: live chat turn, no fresh fix.
            if let (Some(fb), Some(turn_id)) =
                (ctx.chat_feedback.as_ref(), ctx.assistant_turn_id.as_deref())
            {
                if fb.secure {
                    // Ask the browser and wait; decline / timeout falls through.
                    if let Some(BrowserFix::Position { lat, lon, accuracy }) =
                        request_browser_location(fb, turn_id).await
                    {
                        return Ok(precise_payload(lat, lon, accuracy));
                    }
                } else {
                    // Insecure transport (plain-HTTP, non-localhost origin):
                    // browsers block geolocation outright, so don't prompt for
                    // something the user can't grant. Warn inline and fall
                    // through to GeoIP.
                    warn_insecure(fb);
                }
            }

            // 3. Coarse GeoIP on the source IP (works without a browser).
            if let (Some(geoip), Some(ip)) = (ctx.geoip.as_ref(), ctx.client_ip.as_deref())
                && let Some(geo) = geoip.lookup(ip)
            {
                return Ok(ip_payload(&geo));
            }

            // 4. Nothing to go on.
            Ok(json!({
                "known": false,
                "note": "No location available (the user hasn't shared a precise \
                         location and their IP couldn't be resolved). Ask the user \
                         which city or region they mean.",
            }))
        })
    }
}

/// Shape a precise browser position (shared via `/tools` earlier or the
/// in-chat prompt just now — same shape either way). No place name: the
/// model can reverse-geocode via `search_web` if it needs one; the
/// coordinates are enough for weather/nearby queries.
fn precise_payload(lat: f64, lon: f64, accuracy: Option<f64>) -> Value {
    let mut out = json!({
        "known": true,
        "source": "browser",
        "precision": "precise",
        "latitude": lat,
        "longitude": lon,
        "note": "Precise location the user shared from their device. \
                 Use it directly (e.g. search the web for weather at these coordinates).",
    });
    if let Some(acc) = accuracy {
        out["accuracy_meters"] = json!(acc);
    }
    out
}

/// Run the chat feedback loop: push a "share your location?" prompt onto
/// the live SSE turn, then wait (bounded) for the browser to reply via
/// `POST /api/v0/me/location/feedback/{turn_id}`. Returns the fix, or
/// `None` on decline / timeout / no-one-watching — caller falls back to
/// GeoIP. Always tears the prompt back down before returning.
async fn request_browser_location(fb: &ChatFeedback, turn_id: &str) -> Option<BrowserFix> {
    use session_core::workers::TurnUpdate;

    // Fast path: with no live SSE subscriber nobody can answer, so don't
    // prompt. Best-effort only — the stream could still drop right after
    // this check, in which case the `WAIT_SECS` timeout below is the real
    // backstop (we fall through to GeoIP rather than hang).
    if fb.broadcast.receiver_count() == 0 {
        return None;
    }

    let rx = fb.hub.register(turn_id);

    // Append the prompt as an inline card at the end of the live
    // conversation — right under the in-progress assistant bubble, where
    // the user is already looking — rather than a floating corner toast.
    // The stream's Tick re-render only ever patches `#turn-<id>` (mode
    // outer), so a sibling appended to `#conversation` survives every
    // tick; the explicit teardown below (and the client's own removal on
    // click) is what clears it.
    //
    // A trailing one-shot scroll brings the card into view (distinct from
    // the suppressed token-by-token autoscroll; `center` keeps it clear of
    // the floating composer). Both SSE events ride in a single `Inject`
    // frame so the append and scroll arrive — and apply — atomically.
    let card = prompt_card_html(turn_id);
    let mut frame =
        session_core::chrome::sse_patch(Some("#conversation"), Some("append"), &card).to_vec();
    let scroll = session_core::chrome::sse_script(&format!(
        "document.getElementById('geo-prompt-{turn_id}')\
         ?.scrollIntoView({{block:'center',behavior:'smooth'}});"
    ));
    frame.extend_from_slice(&scroll);
    let _ = fb
        .broadcast
        .send(TurnUpdate::Inject(std::sync::Arc::new(frame.into())));

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), rx).await;

    // Tear the prompt down regardless of how the wait ended (the client
    // also removes it on click — this covers the timeout case).
    let cleanup = session_core::chrome::sse_script(&format!(
        "document.getElementById('geo-prompt-{turn_id}')?.remove();"
    ));
    let _ = fb
        .broadcast
        .send(TurnUpdate::Inject(std::sync::Arc::new(cleanup)));

    match outcome {
        Ok(Ok(fix)) => Some(fix),
        // Timeout or the sender was dropped — unregister and fall back.
        _ => {
            fb.hub.cancel(turn_id);
            None
        }
    }
}

/// Inline notice (an auto-dismissing toast in the page's `#toasts`
/// region) for when we *can't* ask the browser because the origin isn't a
/// secure context. Tells the user why precise location was skipped; the
/// tool then falls back to GeoIP. No-op if nobody's watching the turn.
fn warn_insecure(fb: &ChatFeedback) {
    use session_core::workers::TurnUpdate;
    if fb.broadcast.receiver_count() == 0 {
        return;
    }
    // Matches the `.toast-item` shape `app.ts` arms for auto-dismiss.
    let html = "<div role=\"status\" class=\"toast-item pointer-events-auto bg-base-100 \
                text-base-content border border-base-300 border-l-4 border-l-warning \
                rounded-lg shadow-md px-3 py-2 text-sm max-w-sm\">\u{1F4CD} Precise location \
                needs a secure (HTTPS) connection — using your approximate location instead.</div>";
    let patch = session_core::chrome::sse_patch(Some("#toasts"), Some("append"), html);
    let _ = fb
        .broadcast
        .send(TurnUpdate::Inject(std::sync::Arc::new(patch)));
}

/// The injected prompt, rendered as an inline card that drops into the
/// conversation flow (appended to `#conversation`) rather than a floating
/// overlay. Left-aligned (`self-start`) so it reads as the assistant
/// asking, sitting just under the in-progress reply. `turn_id` is a UUID,
/// so it's safe to interpolate into both the element id and the
/// `window.geo.*` calls.
fn prompt_card_html(turn_id: &str) -> String {
    format!(
        "<div id=\"geo-prompt-{tid}\" \
           class=\"alert bg-base-100 border border-base-300 shadow-sm \
                  flex flex-col items-start gap-2 self-start max-w-md\">\
           <span class=\"text-sm\">\u{1F4CD} The assistant wants to use your device's precise \
             location to answer that. Share it?</span>\
           <div class=\"flex gap-2 self-end\">\
             <button type=\"button\" class=\"btn btn-xs btn-ghost\" \
               data-on:click=\"window.geo.declineForTurn('{tid}')\">Not now</button>\
             <button type=\"button\" class=\"btn btn-xs btn-primary\" \
               data-on:click=\"window.geo.shareForTurn('{tid}')\">Share location</button>\
           </div>\
         </div>",
        tid = turn_id
    )
}

/// Shape a coarse GeoIP result. `GeoLocation` already serialises only
/// its populated fields (`skip_serializing_if = "Option::is_none"`), so
/// start from that object and layer the tool-result envelope on top.
fn ip_payload(geo: &crate::server::geoip::GeoLocation) -> Value {
    let mut out = serde_json::to_value(geo).unwrap_or_else(|_| json!({}));
    if let Some(map) = out.as_object_mut() {
        map.insert("known".into(), json!(true));
        map.insert("source".into(), json!("ip"));
        map.insert("precision".into(), json!("approximate"));
        map.insert(
            "note".into(),
            json!(
                "Approximate location derived from the user's IP address (city-level at best). \
                 If the user needs precision, ask them to confirm or share their location."
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use jiff::Timestamp;

    async fn ctx_for(pool: db::Pool, user_id: &str) -> ToolContext {
        // Ensure the user row exists so find_location has a row to read.
        let now = Timestamp::now();
        db::users::upsert(
            &pool,
            &db::users::User {
                id: user_id.into(),
                email: format!("{user_id}@example.com"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();
        ToolContext {
            user_id: user_id.into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    #[tokio::test]
    async fn returns_unknown_with_no_sources() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let ctx = ctx_for(pool, "u").await;
        let out = GetUserLocation.run(ctx, Value::Null).await.unwrap();
        assert_eq!(out["known"], false);
        assert!(out["note"].is_string());
    }

    #[tokio::test]
    async fn prefers_fresh_browser_position() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let ctx = ctx_for(pool.clone(), "u").await;
        db::users::set_location(&pool, "u", 52.52, 13.405, Some(20.0))
            .await
            .unwrap();
        let out = GetUserLocation.run(ctx, json!({})).await.unwrap();
        assert_eq!(out["known"], true);
        assert_eq!(out["source"], "browser");
        assert_eq!(out["precision"], "precise");
        assert_eq!(out["latitude"], 52.52);
        assert_eq!(out["accuracy_meters"], 20.0);
    }

    #[tokio::test]
    async fn schema_name_matches_id() {
        assert_eq!(GetUserLocation.id(), GetUserLocation.schema().function.name);
    }
}
