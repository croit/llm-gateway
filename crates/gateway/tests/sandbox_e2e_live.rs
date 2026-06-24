// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH
//
// LIVE, no-mock E2E for the uploaded-file sandbox tools. Gated behind
// RUN_SANDBOX_E2E so it never runs in normal CI — it needs a real
// sandbox-runner (RUNNER_URL) and real S3 (GATEWAY_S3_* env + a bucket).
//
// Run (from repo root, with mise injecting the S3 secrets):
//   RUN_SANDBOX_E2E=1 RUNNER_URL=http://127.0.0.1:9009 \
//   DECK_PATH=/abs/deck.pptx \
//   mise exec -- cargo test -p gateway --test sandbox_e2e_live -- --nocapture
//
// It seeds a chat session, uploads the deck to S3 under a user turn (so
// it's the "round" upload), then drives the real tool code: resolve →
// real S3 fetch → real runner exec (python-pptx) → artifact delivery
// back to S3. Verifies the produced deck is a real, edited .pptx.

use std::sync::Arc;

use gateway::server::chat_attachments::{self, UploadOutcome};
use gateway::server::config::{S3Config, SandboxConfig};
use gateway::server::db;
use gateway::server::tools::sandbox::{EditPresentation, RunInSandbox, SandboxClient};
use gateway::server::tools::{Tool, ToolContext};
use serde_json::json;

fn enabled() -> bool {
    std::env::var("RUN_SANDBOX_E2E").is_ok()
}

fn s3_cfg() -> Arc<S3Config> {
    Arc::new(S3Config {
        endpoint: std::env::var("E2E_S3_ENDPOINT")
            .unwrap_or_else(|_| "https://fra1.s3.croit.io".into()),
        region: std::env::var("E2E_S3_REGION").unwrap_or_else(|_| "fra1".into()),
        bucket: std::env::var("E2E_S3_BUCKET").unwrap_or_else(|_| "llm-gateway".into()),
        access_key_env: "GATEWAY_S3_ACCESS_KEY".into(),
        secret_key_env: "GATEWAY_S3_SECRET_KEY".into(),
        // Isolate this test's objects under their own prefix.
        key_prefix: "e2e-sandbox-test".into(),
    })
}

fn client() -> Arc<SandboxClient> {
    let runner_url = std::env::var("RUNNER_URL").unwrap_or_else(|_| "http://127.0.0.1:9009".into());
    SandboxClient::new(
        Arc::new(SandboxConfig {
            enabled: true,
            runner_url,
            timeout_secs: 120,
            max_artifact_bytes: 50 * 1024 * 1024,
        }),
        "http://localhost:8080".into(),
    )
}

async fn seed(pool: &db::Pool, user_turn: &str, asst_turn: &str, deck: &[u8]) {
    // user + session + a user turn (carries the upload marker) + an
    // in-progress assistant turn (where produced files get delivered).
    sqlx::query(
        "INSERT INTO users (id, email, created_at, updated_at) VALUES \
         ('u1','u1@e','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z') ON CONFLICT(id) DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, created_at, updated_at) VALUES \
         ('s1','u1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z') ON CONFLICT(id) DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();

    let cfg = s3_cfg();
    chat_attachments::upload(
        &cfg,
        user_turn,
        "deck.pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        deck.to_vec(),
    )
    .await
    .expect("upload deck to S3");
    let marker = chat_attachments::marker_line(
        user_turn,
        &UploadOutcome {
            filename: "deck.pptx".into(),
            mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                .into(),
            bytes: deck.len() as u64,
        },
    );
    sqlx::query(
        "INSERT INTO chat_turns (id, session_id, seq, role, user_content, status, created_at) \
         VALUES (?,'s1',0,'user',?,'completed','2026-01-01T00:00:00Z')",
    )
    .bind(user_turn)
    .bind(&marker)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_turns (id, session_id, seq, role, content, status, created_at) \
         VALUES (?,'s1',1,'assistant','','in_progress','2026-01-01T00:00:00Z')",
    )
    .bind(asst_turn)
    .execute(pool)
    .await
    .unwrap();
}

fn ctx(pool: db::Pool, asst_turn: &str) -> ToolContext {
    ToolContext {
        user_id: "u1".into(),
        roles: vec![],
        db: pool,
        s3: Some(s3_cfg()),
        assistant_turn_id: Some(asst_turn.into()),
        session_id: Some("s1".into()),
        client_ip: None,
        geoip: None,
        chat_feedback: None,
        attachment_reservations: Some(chat_attachments::new_reservation_set()),
        indexer: None,
    }
}

#[tokio::test]
async fn edit_presentation_live_round_trip() {
    if !enabled() {
        eprintln!("skipping: set RUN_SANDBOX_E2E=1 to run the live sandbox E2E");
        return;
    }
    let deck = std::fs::read(std::env::var("DECK_PATH").expect("DECK_PATH")).unwrap();
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let (user_turn, asst_turn) = ("t-user-edit", "t-asst-edit");
    seed(&pool, user_turn, asst_turn, &deck).await;

    // No attachment_id → defaults to the round's deck. The code reads the
    // staged input.pptx, edits it, saves output.pptx, then reopens it and
    // prints the title back so stdout proves the edit round-tripped.
    let code = "from pptx import Presentation\n\
                p = Presentation('input.pptx')\n\
                p.slides[0].shapes.title.text = 'HELLO LIVE E2E'\n\
                p.save('output.pptx')\n\
                print('TITLE=' + Presentation('output.pptx').slides[0].shapes.title.text)\n";

    let out = EditPresentation(client())
        .run(ctx(pool, asst_turn), json!({ "code": code }))
        .await
        .expect("edit_presentation run");
    eprintln!(
        "edit_presentation result: {}",
        serde_json::to_string_pretty(&out).unwrap()
    );

    assert_eq!(out["exit_code"], 0, "sandbox exit non-zero: {out}");
    assert!(
        out["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("TITLE=HELLO LIVE E2E"),
        "edit did not round-trip in-sandbox: {out}"
    );
    assert_eq!(out["edited"]["id"], format!("{user_turn}/deck.pptx"));
    let arts = out["artifacts"].as_array().expect("artifacts");
    let outp = arts
        .iter()
        .find(|a| a["name"] == "output.pptx")
        .expect("output.pptx artifact");
    assert_eq!(
        outp["status"], "attached",
        "produced file not attached: {outp}"
    );

    // Re-fetch the delivered file from REAL S3 and confirm it's a valid,
    // non-trivial .pptx (zip magic "PK").
    let fetched = chat_attachments::fetch(&s3_cfg(), asst_turn, "output.pptx")
        .await
        .expect("fetch produced output.pptx from S3");
    assert!(fetched.bytes.starts_with(b"PK"), "output is not a zip/pptx");
    assert!(fetched.bytes.len() > 10_000, "output suspiciously small");
    eprintln!(
        "PASS edit_presentation: produced + delivered a {}-byte edited .pptx",
        fetched.bytes.len()
    );
}

#[tokio::test]
async fn run_in_sandbox_auto_stages_round_upload() {
    if !enabled() {
        eprintln!("skipping: set RUN_SANDBOX_E2E=1 to run the live sandbox E2E");
        return;
    }
    let deck = std::fs::read(std::env::var("DECK_PATH").expect("DECK_PATH")).unwrap();
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let (user_turn, asst_turn) = ("t-user-run", "t-asst-run");
    seed(&pool, user_turn, asst_turn, &deck).await;

    // No `attachments` arg: the round's deck.pptx must be auto-staged into
    // /work, so this code can just open it.
    let code = "from pptx import Presentation\n\
                import os\n\
                print('HAS_DECK=' + str(os.path.exists('deck.pptx')))\n\
                print('SLIDES=' + str(len(Presentation('deck.pptx').slides._sldIdLst)))\n";
    let out = RunInSandbox(client())
        .run(
            ctx(pool, asst_turn),
            json!({ "language": "python", "code": code }),
        )
        .await
        .expect("run_in_sandbox run");
    eprintln!(
        "run_in_sandbox result: {}",
        serde_json::to_string_pretty(&out).unwrap()
    );

    assert_eq!(out["exit_code"], 0, "sandbox exit non-zero: {out}");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("HAS_DECK=True"),
        "round upload was not auto-staged: {out}"
    );
    assert!(
        stdout.contains("SLIDES=2"),
        "staged deck not readable: {out}"
    );
    // The staging metadata should advertise what landed in /work.
    let staged = out["staged_files"].as_array().expect("staged_files");
    assert!(
        staged
            .iter()
            .any(|s| s["name"] == "deck.pptx" && s["id"] == format!("{user_turn}/deck.pptx")),
        "staged_files missing the round deck: {out}"
    );
    eprintln!("PASS run_in_sandbox: round upload auto-staged and read inside the sandbox");
}
