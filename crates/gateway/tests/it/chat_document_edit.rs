// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The user's own hand edit of a canvas document
//! (`POST /chat/{id}/document/{doc_id}/edit`).
//!
//! What matters here is who may write and what the write records: the canvas
//! is shared with the model, so a version the *user* authored has to be
//! distinguishable (the request context warns the model off overwriting it)
//! and a shared conversation's read-only viewer must not be able to write at
//! all — the UI hides the affordance from them, but the route is the gate.

use crate::common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use gateway_core::server::db::documents::{self, DocumentFormat, VersionAuthor};
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;

async fn seed_document(
    state: &gateway::rama_server::RamaState,
    session_id: &str,
    user: &str,
    content: &str,
) -> String {
    let id = documents::new_id();
    documents::create(
        &state.db,
        &id,
        session_id,
        user,
        "Migration plan",
        DocumentFormat::Markdown,
        content,
        None,
    )
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn a_hand_edit_saves_a_user_authored_version() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let doc_id = seed_document(&state, &session.id, "alice", "# Plan\n\nold wording\n").await;
    let app = router(state.clone());

    let resp = app
        .serve(common::post_form(
            &format!("/chat/{}/document/{doc_id}/edit", session.id),
            &cookie,
            // Browsers submit CRLF per the HTML spec; the handler normalises
            // it, or every line would read as changed.
            "content=%23+Plan%0D%0A%0D%0Amy+wording%0D%0A",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (doc, ver) = documents::get_version(&state.db, &session.id, &doc_id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.current_ver, 2,
        "the edit is a new version, not a rewrite"
    );
    assert_eq!(ver.content, "# Plan\n\nmy wording\n");
    assert_eq!(
        ver.author,
        VersionAuthor::User,
        "authorship is what tells the model not to revert it"
    );
    // v1 is untouched — the canvas's promise is that nothing is ever lost.
    let (_, first) = documents::get_version(&state.db, &session.id, &doc_id, Some(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.content, "# Plan\n\nold wording\n");
    assert_eq!(first.author, VersionAuthor::Assistant);
}

#[tokio::test]
async fn saving_an_unchanged_document_mints_no_version() {
    // Opening the editor and pressing save without typing is not a change;
    // the history is meant to list real revisions.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let doc_id = seed_document(&state, &session.id, "alice", "same\n").await;
    let app = router(state.clone());

    let resp = app
        .serve(common::post_form(
            &format!("/chat/{}/document/{doc_id}/edit", session.id),
            &cookie,
            "content=same%0A",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = documents::get(&state.db, &session.id, &doc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.current_ver, 1);
}

#[tokio::test]
async fn a_shared_viewer_cannot_write_to_someone_elses_document() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _owner = common::seed_session(&state, "alice", "alice@example.com").await;
    let intruder = common::seed_session(&state, "bob", "bob@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    // Shared, so bob can *read* the conversation — reading is not writing.
    chat::set_shared(&state.db, "alice", &session.id, true)
        .await
        .unwrap();
    let doc_id = seed_document(&state, &session.id, "alice", "alice's plan\n").await;
    let app = router(state.clone());

    let resp = app
        .serve(common::post_form(
            &format!("/chat/{}/document/{doc_id}/edit", session.id),
            &intruder,
            "content=bob+was+here%0A",
        ))
        .await
        .unwrap();
    // The SSE surface answers 200 with an error frame rather than a status
    // code (same as every other chat action), so the assertion that matters
    // is that nothing was written.
    assert_eq!(resp.status(), StatusCode::OK);
    let (doc, ver) = documents::get_version(&state.db, &session.id, &doc_id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.current_ver, 1, "no version from a non-owner");
    assert_eq!(ver.content, "alice's plan\n");
}

#[tokio::test]
async fn a_deleted_document_refuses_the_edit() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let doc_id = seed_document(&state, &session.id, "alice", "draft\n").await;
    documents::soft_delete(&state.db, &session.id, &doc_id)
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(common::post_form(
            &format!("/chat/{}/document/{doc_id}/edit", session.id),
            &cookie,
            "content=resurrected%0A",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = documents::get(&state.db, &session.id, &doc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.current_ver, 1,
        "a document in the bin takes no writes — undelete first"
    );
}

#[tokio::test]
async fn the_panel_offers_the_editor_and_flags_the_users_own_version() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let doc_id = seed_document(&state, &session.id, "alice", "body\n").await;
    let app = router(state.clone());

    // Before any hand edit: the form is there, the badge is not.
    let resp = app
        .serve(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/chat/{}/document/{doc_id}", session.id))
                .header("cookie", format!("id={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains(&format!("/chat/{}/document/{doc_id}/edit", session.id)),
        "panel should post to the edit route: {body}"
    );
    assert!(
        body.contains("$canEditDocs"),
        "the affordance is gated on the shell signal, not on baked-in identity: {body}"
    );
    assert!(!body.contains("edited by you"), "no badge yet: {body}");

    // After one: the badge marks the version as the user's own.
    documents::append_version(
        &state.db,
        &session.id,
        &doc_id,
        "my body\n",
        Some("Edited by you"),
        None,
        VersionAuthor::User,
    )
    .await
    .unwrap();
    let resp = app
        .serve(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/chat/{}/document/{doc_id}", session.id))
                .header("cookie", format!("id={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(body.contains("edited by you"), "badge missing: {body}");
}
