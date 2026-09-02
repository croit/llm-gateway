// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/settings` — the operator settings that moved out of `gateway.toml`.
//!
//! What this pins, in ascending order of how expensive it would be to get
//! wrong:
//!
//! 1. **The gate** — these settings reconfigure the whole gateway, so anonymous
//!    and non-admin callers must not reach the page or the save.
//! 2. **A save is live** — the point of the move is that an operator changes a
//!    value in the browser and the gateway uses it, with no restart and no
//!    file. A save that only writes a row nobody re-reads would look identical
//!    in the UI and do nothing.
//! 3. **Secrets stay secret** — a stored credential must never be rendered back
//!    into the page, and an empty box must not wipe it.

use crate::common;

use common::Service as _;
use gateway_core::server::settings;
use jiff::Timestamp;
use rama::http::{Body, Method, Request, Response, StatusCode};

use gateway_core::server::db::users;

fn form(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap()
}

async fn seed_admin(state: &gateway::rama_server::RamaState, id: &str) -> String {
    let cookie = common::seed_session(state, id, &format!("{id}@example.com")).await;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: id.into(),
            email: format!("{id}@example.com"),
            name: None,
            roles: vec!["admin".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
            speech_voice: None,
        },
    )
    .await
    .unwrap();
    cookie
}

async fn body_text(resp: Response) -> String {
    String::from_utf8_lossy(&common::read_body(resp).await).into_owned()
}

// ---------------------------------------------------------------------------
// 1. The gate

#[tokio::test]
async fn anonymous_and_non_admin_callers_are_kept_out() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let plain = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    let resp = app
        .serve(common::req(Method::GET, "/admin/settings"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "anon → /login");

    let resp = app.serve(get("/admin/settings", &plain)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The save in particular: a non-admin who can POST here can turn on the
    // code sandbox or repoint attachment storage.
    let resp = app
        .serve(form(
            "/admin/settings",
            &plain,
            "section=usage&usage.enabled=on&usage.currency=EUR",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// 2. The page, and saves that actually take effect

#[tokio::test]
async fn every_declared_field_is_reachable_on_some_tab() {
    // The page shows one category at a time, so "is it rendered" is now a
    // question about the whole set of tabs. A field on no tab is a field no
    // operator can edit, and the flat `SECTIONS` list cannot show that.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);

    let mut seen = String::new();
    for category in settings::Category::ALL {
        let uri = format!("/admin/settings?tab={}", category.slug());
        seen.push_str(&body_text(app.serve(get(&uri, &cookie)).await.unwrap()).await);
    }
    for field in settings::all_fields() {
        assert!(
            seen.contains(field.key),
            "{} is declared but appears on no tab",
            field.key
        );
    }
}

#[tokio::test]
async fn a_tab_shows_its_own_sections_and_not_the_others() {
    // The whole point of the split: opening one category must not render the
    // other thirteen cards underneath it.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);

    let html = body_text(
        app.serve(get("/admin/settings?tab=notifications", &cookie))
            .await
            .unwrap(),
    )
    .await;
    assert!(html.contains("push.contact"), "its own field is missing");
    assert!(
        !html.contains("chat.ocr.dpi"),
        "another tab's field leaked onto this one"
    );
}

#[tokio::test]
async fn an_unknown_tab_falls_back_to_the_first_one() {
    // A stale bookmark or a hand-typed slug must not render a page with no
    // cards on it.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);

    let resp = app
        .serve(get("/admin/settings?tab=not-a-tab", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    let first = settings::Category::ALL[0];
    let expected = first.sections().next().expect("the first tab has cards");
    assert!(
        html.contains(expected.fields[0].key),
        "expected a fallback to the {:?} tab",
        first
    );
}

#[tokio::test]
async fn switching_the_sandbox_on_builds_its_client_without_a_restart() {
    // The feature bundle, not just the config value. A save that flipped
    // `sandbox.enabled` used to leave `sandbox_client` at whatever boot had
    // built — so the config said "on" while the thing that talks to the runner
    // was still absent, and every sandbox call failed until a restart.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert!(
        state.sandbox_client().is_none(),
        "no client before the feature is switched on"
    );

    let app = common::app(state.clone());
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=sandbox&sandbox.enabled=on&sandbox.runner_url=http%3A%2F%2Frunner%3A9000\
         &sandbox.timeout_secs=45&sandbox.max_artifact_bytes=1024",
    ))
    .await
    .unwrap();

    assert!(
        state.sandbox_client().is_some(),
        "the client must exist the moment the config says the feature is on"
    );

    // ...and switching it back off must take the client away again, or a
    // disabled feature would keep a live connection to the runner.
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=sandbox&sandbox.runner_url=http%3A%2F%2Frunner%3A9000\
         &sandbox.timeout_secs=45&sandbox.max_artifact_bytes=1024",
    ))
    .await
    .unwrap();
    assert!(state.config().sandbox.is_none(), "config says off");
    assert!(
        state.sandbox_client().is_none(),
        "so the client is gone too"
    );
}

#[tokio::test]
async fn switching_skills_on_builds_its_stores_without_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert!(state.skills().is_none() && state.user_skills().is_none());

    let app = common::app(state.clone());
    let body = format!(
        "section=skills&skills.enabled=on&skills.dir={}",
        serde_urlencoded::to_string([("v", dir.path().display().to_string())])
            .unwrap()
            .trim_start_matches("v=")
    );
    app.serve(form("/admin/settings", &cookie, &body))
        .await
        .unwrap();

    assert!(state.skills().is_some(), "the global store is loaded");
    assert!(
        state.user_skills().is_some(),
        "and the private store moves in lockstep with it"
    );
}

#[tokio::test]
async fn pointing_typst_at_a_new_directory_registers_its_tools() {
    // The one family whose *membership* depends on a setting: typst registers
    // one concrete tool per discovered template, so a per-request filter could
    // never surface a template added after boot. This pins that the registry
    // itself is rebuilt — and it is the only reason  lost
    // its restart badge.
    let dir = tempfile::tempdir().expect("tempdir");
    let tpl = dir.path().join("memo");
    std::fs::create_dir_all(&tpl).expect("template dir");
    std::fs::write(
        tpl.join("template.toml"),
        "id = \"memo\"\ntitle = \"Memo\"\ndescription = \"A memo\"\n",
    )
    .expect("manifest");
    std::fs::write(tpl.join("template.typ"), "#set page()\n").expect("template");

    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert!(
        !state.tools().ids().any(|id| id.starts_with("typst_")),
        "no typst tools before the directory is configured"
    );

    let app = common::app(state.clone());
    let body = format!(
        "section=typst&typst.enabled=on&typst.templates_dir={}",
        serde_urlencoded::to_string([("v", dir.path().display().to_string())])
            .unwrap()
            .trim_start_matches("v=")
    );
    app.serve(form("/admin/settings", &cookie, &body))
        .await
        .unwrap();

    let ids: Vec<String> = state
        .tools()
        .ids()
        .filter(|id| id.starts_with("typst_"))
        .map(str::to_owned)
        .collect();
    assert!(
        ids.iter().any(|id| id == "typst_memo"),
        "the render tool for the discovered template must exist now; got {ids:?}"
    );

    // ...and switching typst off must take the whole family away again.
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=typst&typst.templates_dir=/nowhere",
    ))
    .await
    .unwrap();
    assert!(
        !state.tools().ids().any(|id| id.starts_with("typst_")),
        "a disabled feature must not leave its tools registered"
    );
}

#[tokio::test]
async fn a_restart_only_field_leaves_a_banner_that_outlives_the_toast() {
    // The toast is gone in three seconds and the operator who saves is often
    // not the operator who restarts the container, so "a restart is pending"
    // has to be state, not a notification.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());
    assert!(
        settings::restart_pending(&state.db)
            .await
            .unwrap()
            .is_empty(),
        "nothing pending before anything is saved"
    );

    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=rag&rag.enabled=on&rag.data_dir=/tmp/rag-elsewhere&rag.clone_concurrency=8",
    ))
    .await
    .unwrap();

    let pending = settings::restart_pending(&state.db).await.unwrap();
    assert!(
        pending.iter().any(|f| f == "rag.data_dir"),
        "the restart-only field must be recorded; got {pending:?}"
    );
    // And the page says so, not just the database.
    let html = body_text(
        app.serve(get("/admin/settings?tab=data", &cookie))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        html.contains("rag.data_dir"),
        "the banner must name the field waiting on a restart"
    );

    // Saving a fully hot section must not add anything to the list.
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=usage&usage.enabled=on&usage.currency=EUR&usage.retention_days=90",
    ))
    .await
    .unwrap();
    let after = settings::restart_pending(&state.db).await.unwrap();
    assert!(
        !after.iter().any(|f| f.starts_with("usage.")),
        "a hot section must not claim it needs a restart; got {after:?}"
    );
}

#[tokio::test]
async fn session_lifetimes_apply_without_a_restart() {
    // The policy lives on the SessionStore, which is `Clone` and gets cloned
    // into the shared state — so plain fields would have updated one copy and
    // left every other reader on the old timeout. This pins that a save
    // reaches the store every request actually uses.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let days = |d: u64| std::time::Duration::from_secs(d * 24 * 60 * 60);
    assert_eq!(state.sessions.ttl(), days(30), "the built-in default");

    let app = common::app(state.clone());
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=gateway&gateway.token_ttl_days=90&gateway.session_ttl_days=3\
         &gateway.session_absolute_max_days=14",
    ))
    .await
    .unwrap();

    assert_eq!(
        state.sessions.ttl(),
        days(3),
        "the idle timeout must be live on the store, not just in the config"
    );
    assert_eq!(state.sessions.absolute_max(), days(14));
}

#[tokio::test]
async fn a_saved_value_is_live_on_the_very_next_request() {
    // The whole point of the move: an operator changes a value in the browser
    // and the gateway uses it — no restart, no config file. Asserting only
    // that the row was written would pass even if nothing re-read it.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert_eq!(state.config().usage.currency, "USD", "the built-in default");

    let app = common::app(state.clone());
    let resp = app
        .serve(form(
            "/admin/settings",
            &cookie,
            "section=usage&usage.enabled=on&usage.retention_days=7&usage.currency=EUR",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(state.config().usage.currency, "EUR");
    assert_eq!(state.config().usage.retention_days, 7);
}

#[tokio::test]
async fn an_unchecked_box_turns_the_feature_off() {
    // A checkbox submits nothing when unchecked, so "absent" has to mean
    // `false` here — treating it as "unchanged", the rule secrets follow,
    // would make every toggle one-way.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert!(state.config().usage.enabled, "on by default");

    let app = common::app(state.clone());
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=usage&usage.currency=USD&usage.retention_days=90",
    ))
    .await
    .unwrap();

    assert!(!state.config().usage.enabled);
}

#[tokio::test]
async fn enabling_an_optional_block_brings_it_into_existence() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    assert!(state.config().sandbox.is_none(), "absent until configured");

    let app = common::app(state.clone());
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=sandbox&sandbox.enabled=on&sandbox.runner_url=http%3A%2F%2Frunner%3A9000\
         &sandbox.timeout_secs=45&sandbox.max_artifact_bytes=1024",
    ))
    .await
    .unwrap();

    let sandbox = state.config().sandbox.clone().expect("now configured");
    assert_eq!(sandbox.runner_url, "http://runner:9000");
    assert_eq!(sandbox.timeout_secs, 45);
}

#[tokio::test]
async fn saving_one_section_leaves_the_others_alone() {
    // Sections save independently. If a save rewrote every row from the posted
    // form, adjusting one number would silently reset eleven other blocks.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());

    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=chat.ocr&chat.ocr.enabled=on&chat.ocr.dpi=150",
    ))
    .await
    .unwrap();
    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=usage&usage.enabled=on&usage.currency=CHF&usage.retention_days=30",
    ))
    .await
    .unwrap();

    let config = state.config();
    assert!(config.chat.ocr.enabled, "the earlier section survived");
    assert_eq!(config.chat.ocr.dpi, 150);
    assert_eq!(config.usage.currency, "CHF");
}

// ---------------------------------------------------------------------------
// 3. Secrets

#[tokio::test]
async fn a_stored_secret_is_never_rendered_back_into_the_page() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());

    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=feedback&feedback.enabled=on&feedback.github_owner=croit\
         &feedback.github_repo=llm-gateway&feedback.github_token=ghp_supersecret\
         &feedback.github_api_base=https%3A%2F%2Fapi.github.com&feedback.labels=feedback\
         &feedback.assets_branch=feedback-assets",
    ))
    .await
    .unwrap();

    // It reached the config...
    assert_eq!(
        state
            .config()
            .feedback
            .as_ref()
            .and_then(|f| f.github_token())
            .as_deref(),
        Some("ghp_supersecret")
    );
    // ...and it is nowhere in the HTML.
    let html = body_text(app.serve(get("/admin/settings", &cookie)).await.unwrap()).await;
    assert!(
        !html.contains("ghp_supersecret"),
        "the page must never echo a stored secret"
    );

    // It is not sitting in the database in the clear either.
    let row: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_settings WHERE key = 'settings.feedback.github_token'",
    )
    .fetch_optional(&state.db)
    .await
    .unwrap();
    let row = row.expect("stored");
    assert!(!row.contains("ghp_supersecret"), "must be sealed at rest");
}

#[tokio::test]
async fn an_empty_secret_box_keeps_the_stored_value_and_clear_removes_it() {
    // The page cannot round-trip a secret it never renders, so an empty
    // submission has to mean "leave it alone" — otherwise every unrelated edit
    // to the section would wipe the credential.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());

    let with_token = "section=feedback&feedback.enabled=on&feedback.github_owner=croit\
         &feedback.github_repo=llm-gateway&feedback.github_token=ghp_supersecret\
         &feedback.github_api_base=https%3A%2F%2Fapi.github.com&feedback.labels=feedback\
         &feedback.assets_branch=feedback-assets";
    app.serve(form("/admin/settings", &cookie, with_token))
        .await
        .unwrap();

    // Re-save the section with the secret box left empty, changing something else.
    let without_token = "section=feedback&feedback.enabled=on&feedback.github_owner=croit\
         &feedback.github_repo=other-repo&feedback.github_token=\
         &feedback.github_api_base=https%3A%2F%2Fapi.github.com&feedback.labels=feedback\
         &feedback.assets_branch=feedback-assets";
    app.serve(form("/admin/settings", &cookie, without_token))
        .await
        .unwrap();

    let config = state.config();
    let feedback = config.feedback.as_ref().expect("enabled");
    assert_eq!(feedback.github_repo, "other-repo", "the edit landed");
    assert_eq!(
        feedback.github_token().as_deref(),
        Some("ghp_supersecret"),
        "an empty box must not wipe the credential"
    );

    // The explicit clear does remove it.
    app.serve(form(
        "/admin/settings/clear",
        &cookie,
        "key=feedback.github_token",
    ))
    .await
    .unwrap();
    assert_eq!(
        state
            .config()
            .feedback
            .as_ref()
            .and_then(|f| f.github_token()),
        None
    );
}

#[tokio::test]
async fn a_form_naming_an_undeclared_field_is_ignored() {
    // A stale tab or a hand-built request must not be able to plant a row
    // nothing reads — an invisible setting is worse than a rejected one.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());

    app.serve(form(
        "/admin/settings",
        &cookie,
        "section=usage&usage.enabled=on&usage.currency=USD&usage.retention_days=90\
         &usage.something_invented=42",
    ))
    .await
    .unwrap();

    let planted: Option<String> = sqlx::query_scalar(
        "SELECT value FROM app_settings WHERE key = 'settings.usage.something_invented'",
    )
    .fetch_optional(&state.db)
    .await
    .unwrap();
    assert!(planted.is_none(), "undeclared keys must not be stored");
}

#[tokio::test]
async fn a_list_field_survives_a_save_that_does_not_touch_it() {
    // The row holds JSON (`["feedback"]`) because that is what the TOML import
    // writes and what `settings::list()` parses, but the save handler reads the
    // box as a comma-separated line. Render the raw JSON and this round-trip
    // eats itself: submitting the page back unchanged stores a label literally
    // named `["feedback"]`, then `["[\"feedback\"]"]`, once per save.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state.clone());

    let save = |labels: &str| {
        format!(
            "section=feedback&feedback.enabled=on&feedback.github_owner=croit\
             &feedback.github_repo=llm-gateway&feedback.github_api_base=https%3A%2F%2Fapi.github.com\
             &feedback.labels={labels}&feedback.assets_branch=feedback-assets"
        )
    };

    app.serve(form("/admin/settings", &cookie, &save("feedback%2C+bug")))
        .await
        .unwrap();
    let stored = state.config().feedback.clone().expect("configured");
    assert_eq!(stored.labels, vec!["feedback", "bug"]);

    // What the page puts in the box has to be what the save handler accepts.
    let page = body_text(
        app.serve(get("/admin/settings?tab=notifications", &cookie))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        page.contains(r#"value="feedback, bug""#),
        "the box must show the comma-separated form, not raw JSON: {page}"
    );
    assert!(
        !page.contains(r#"value="[&quot;feedback&quot;"#),
        "raw JSON in the box would be re-split on the next save"
    );

    // And submitting that back unchanged must be a no-op.
    app.serve(form("/admin/settings", &cookie, &save("feedback%2C+bug")))
        .await
        .unwrap();
    let again = state.config().feedback.clone().expect("still configured");
    assert_eq!(
        again.labels,
        vec!["feedback", "bug"],
        "save must be idempotent"
    );
}
