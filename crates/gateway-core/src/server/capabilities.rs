// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Vision-fallback: when a tool result contains image content but the primary
//! model has no vision support, transparently route the image to a configured
//! fallback model and inject the text description instead.
//!
//! Activated only when the admin has set `vision = false` + a `fallback_vision`
//! model on the primary model's capability row. The fallback model must be
//! routable via the normal upstream registry.

use serde_json::Value;

use crate::server::db::model_defaults;
use crate::server::upstreams::{PoolKind, UpstreamRegistry};

/// The prompt sent to the fallback vision model. Asks for a thorough
/// description so the primary model can reason about the image's content
/// (text, layout, colors, UI elements) without seeing it directly.
const DESCRIBE_PROMPT: &str = "Describe this image in detail. Include any visible text, the layout, colors, objects, people, UI elements, and anything else notable. Be thorough — your description will be used by another model that cannot see the image.";

/// If the tool-result content parts contain `image_url` entries and the
/// primary model lacks vision support, replace them with a text description
/// produced by the fallback vision model.
///
/// Returns the (possibly modified) content parts and an optional notification
/// string (shown to the user when a fallback was used).
pub async fn maybe_replace_image_content(
    parts: &[Value],
    primary_model: &str,
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    registry: &UpstreamRegistry,
) -> (Vec<Value>, Option<String>) {
    let has_image = parts
        .iter()
        .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("image_url"));
    if !has_image {
        return (parts.to_vec(), None);
    }

    let caps = match model_defaults::get(db, primary_model).await {
        Ok(Some(row)) => row.capabilities,
        _ => return (parts.to_vec(), None),
    };

    if caps.vision == Some(true) {
        return (parts.to_vec(), None);
    }

    let Some(fallback_model) = caps.fallback_vision.as_deref() else {
        return (parts.to_vec(), None);
    };

    let mut new_parts: Vec<Value> = Vec::new();
    let mut descriptions: Vec<String> = Vec::new();

    for part in parts {
        match part.get("type").and_then(|t| t.as_str()) {
            Some("image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|iu| iu.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                if url.is_empty() {
                    continue;
                }
                match describe_image(http, registry, fallback_model, url).await {
                    Ok(desc) => {
                        descriptions.push(format!("({fallback_model}): {desc}"));
                        new_parts.push(serde_json::json!({
                            "type": "text",
                            "text": format!("[Image described by {fallback_model}: {desc}]"),
                        }));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, fallback_model, "vision fallback: describe failed");
                        new_parts.push(part.clone());
                    }
                }
            }
            Some("text") => {
                new_parts.push(part.clone());
            }
            _ => {
                new_parts.push(part.clone());
            }
        }
    }

    let notification = if descriptions.is_empty() {
        None
    } else {
        Some(format!(
            "The selected model ({primary_model}) has no vision support. Using {fallback_model} to describe the image and attaching the text description instead."
        ))
    };

    (new_parts, notification)
}

async fn describe_image(
    http: &reqwest::Client,
    registry: &UpstreamRegistry,
    model: &str,
    image_url: &str,
) -> Result<String, anyhow::Error> {
    let acquired = registry
        .acquire_for(model, PoolKind::Chat)
        .map_err(|e| anyhow::anyhow!("routing fallback model: {e}"))?;
    let url = format!("{}/chat/completions", acquired.backend().base_url);
    let body = serde_json::json!({
        "model": acquired.resolved_model(),
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": DESCRIBE_PROMPT},
                {"type": "image_url", "image_url": {"url": image_url}}
            ]
        }],
        "max_tokens": 500,
        "stream": false,
    });

    let mut req = http.post(&url).json(&body);
    if let Some(key) = acquired.backend().api_key.as_deref() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?.error_for_status()?;
    let json: Value = resp.json().await?;
    let text = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("(no description)");
    Ok(text.to_string())
}
