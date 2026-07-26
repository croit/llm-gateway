// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Integration test: exercises the full Runner pipeline against a real
//! ComfyUI worker. Marked `#[ignore]` so it doesn't run in normal CI
//! (needs a live ComfyUI instance + the right models installed).
//!
//! Run manually:
//!   cargo test --package gateway --test comfyui_integration -- --nocapture --ignored

use gateway_features::server::comfyui::{Client, Runner};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn text_to_image_against_real_comfyui() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("gateway=debug")
        .try_init();

    // Point at the real ComfyUI worker. Set COMFYUI_URL to your worker.
    let base_url = std::env::var("COMFYUI_URL")
        .unwrap_or_else(|_| "http://comfyui.example.com:8008".to_string());

    let client = Client::new(base_url).expect("client build");
    let runner = Runner::new(client, Duration::from_millis(500), Duration::from_secs(120));

    // Load the manifest from the example catalog.
    let content_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/comfyui-workflows/llmgw-text2image");

    // Manually load the manifest (the store's `load` does this, but
    // for a single-workflow test we can call it directly).
    let manifest =
        gateway_features::server::comfyui::manifest::load(&content_dir).expect("manifest loads");

    // Run the workflow with a simple prompt.
    let args = json!({
        "prompt": "a cute orange cat sitting on a windowsill, warm afternoon light, photorealistic",
        "negative_prompt": "",
        "width": 768,
        "height": 768,
        "steps": 15,
        "cfg": 5.0,
        "seed": -1,
    });

    let outcome = runner
        .run(&manifest, &args)
        .await
        .expect("workflow completes");

    // The produced asset should be an image.
    assert!(
        !outcome.downloaded.bytes.is_empty(),
        "got empty image bytes"
    );
    assert!(
        outcome.downloaded.mime.starts_with("image/"),
        "mime is {}",
        outcome.downloaded.mime
    );
    assert!(
        outcome.downloaded.bytes.len() > 10_000,
        "image suspiciously small: {} bytes",
        outcome.downloaded.bytes.len()
    );

    println!(
        "✅ text_to_image OK: {} bytes, mime={}, prompt_id={}",
        outcome.downloaded.bytes.len(),
        outcome.downloaded.mime,
        outcome.prompt_id
    );
}
