// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `offer_download` — hand an object that already exists in this
//! conversation back to the user as a download chip.
//!
//! The gap this closes: asked for "the deck's data file", a model would
//! call `typst_presentation_read`, get the JSON, and *paste it into its
//! reply* as a 400-line code block. Every byte went through the model
//! twice (once out of the tool, once into the answer), the user got
//! something to select-and-copy rather than a file, and long payloads got
//! truncated or subtly re-typed on the way through.
//!
//! `upload_attachment` could not fix that: it takes *content*, so the
//! model still has to re-emit the bytes. This tool takes a *reference* —
//! any `<turn_id>/<filename>` id in the conversation, including the ones
//! that never got a chat chip (a typst render's hidden `.json` edit base,
//! a sandbox artifact) — and the copy happens inside S3. Nothing transits
//! the model.
//!
//! Two invariants shape the implementation:
//!
//! - **Markers are only honoured under the turn that owns them**
//!   (`marker_url_owned_by`, added when models started forging chips out
//!   of replayed history). So re-offering an *older* turn's file can't be
//!   done by pointing a marker at it — the object is copied into the
//!   current turn's key space first.
//! - **Session scoping.** An id that carries a marker is proven in-session
//!   by the enumeration itself; an unlisted id is checked against
//!   `chat_turns.session_id` before anything is read, so a guessed
//!   `<uuid>/secret.pdf` from another conversation is refused.

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use gateway_core::server::db::documents::{self, DocumentFormat};
use gateway_features::server::chat_attachments::{self, AttachmentRef};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture, ToolResult};

pub struct OfferDownload;

#[derive(Deserialize)]
struct OfferArgs {
    id: String,
    /// Optional user-facing rename. The stored object keeps its own name;
    /// the chip (and the downloaded file) uses this one.
    #[serde(default)]
    filename: Option<String>,
}

/// Where the bytes to offer live, and what we know about them.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// The id resolved against the session's attachment markers, so mime
    /// and size are already known — no storage round-trip needed.
    Listed(AttachmentRef),
    /// A `<turn_id>/<filename>` id with no marker anywhere in the
    /// conversation (a typst `data_id`, an intermediate artifact). The
    /// turn was verified to belong to this session; mime + size come from
    /// a HEAD before the copy.
    Unlisted { turn_id: String, filename: String },
    /// A canvas document (`doc_…`): the mutable, versioned side of the
    /// conversation's files — a draft, or the data behind a rendered
    /// document. Handing one over materialises its *current* version as a
    /// file, so what the user downloads is what the panel shows (including
    /// their own hand-edits).
    Document {
        title: String,
        format: DocumentFormat,
        version: i64,
        content: String,
    },
}

impl Tool for OfferDownload {
    fn id(&self) -> &str {
        "offer_download"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Give the user a file to download: attaches any file or data \
             object that already exists in this conversation to your current \
             reply, as a download chip. Pass its `id` — a `<turn_id>/<filename>` \
             id from `list_attachments`, a canvas `document_id` (from \
             `list_documents`, `create_document`, `import_file`, or a typst \
             render — the current version is written out as a file), a sandbox \
             artifact ref, or just a filename from this conversation (newest \
             match wins). Reach for this whenever the user \
             asks to *get*, *have*, *export* or *download* something that \
             exists as a file — including files from earlier turns and internal \
             data objects that were never shown as a chip. It is also the right \
             answer instead of pasting a large payload (JSON, CSV, a config, a \
             whole source file) into your prose: attach it and describe it in a \
             sentence. The bytes are copied inside the gateway's storage — you \
             do NOT need to read or repeat the file's contents to hand it over. \
             Use `upload_attachment` instead when the file's content is \
             something you are composing yourself.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "What to hand over: a `<turn_id>/<filename>` \
                                        attachment id (from `list_attachments`, a \
                                        replay stub, …), a canvas `document_id`, or \
                                        a bare filename from this conversation."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional friendlier name for the download, \
                                        e.g. `deck-data.json` for an internal \
                                        `presentation.json`. No slashes. Keep the \
                                        original extension so the file still opens."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: OfferArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, filename?}}: {e}")))?;
            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "offer_download only works inside a chat session — there is no \
                     conversation to take a file from, and no reply to attach it to"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "offer_download is only available inside a chat session — \
                     there's no assistant turn to attach to on this code path"
                        .into(),
                )
            })?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3])"
                        .into(),
                )
            })?;
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "offer_download requires a per-turn attachment-reservation set, \
                     which is only initialised on the chat-page path"
                        .into(),
                )
            })?;

            let source = locate(&ctx.db, session_id, &args.id).await?;
            // A canvas document has no stored object to copy — its content
            // lives in the DB — so it takes the short path: write the current
            // version out as a file under this turn and chip it.
            if let Source::Document {
                title,
                format,
                version,
                content,
            } = &source
            {
                return offer_document(
                    &ctx,
                    turn_id,
                    s3,
                    reservations,
                    &args,
                    title,
                    *format,
                    *version,
                    content,
                )
                .await;
            }
            let (src_turn, src_file) = match &source {
                Source::Listed(a) => (a.turn_id.clone(), a.filename.clone()),
                Source::Unlisted { turn_id, filename } => (turn_id.clone(), filename.clone()),
                // Handled above; keeping the arm exhaustive rather than
                // unreachable!() so a future variant is a compile error.
                Source::Document { .. } => unreachable!("document handled above"),
            };
            // Size + mime for the marker line. Listed ids carry both in the
            // marker they came from; an unlisted one needs a HEAD (never a
            // GET — the object can be tens of megabytes and we only want its
            // metadata).
            let (mime, size) = match &source {
                Source::Listed(a) => (a.mime.clone(), a.size),
                Source::Document { .. } => unreachable!("document handled above"),
                Source::Unlisted { .. } => {
                    let meta = chat_attachments::head(s3, &src_turn, &src_file)
                        .await
                        .map_err(|e| {
                            ToolError::InvalidArgs(format!(
                                "`{}` names a turn of this conversation but no such \
                                 stored file ({e}) — call `list_attachments` to see \
                                 what exists",
                                args.id
                            ))
                        })?;
                    (meta.mime, meta.bytes)
                }
            };

            let desired = match args.filename.as_deref().map(str::trim) {
                Some("") => {
                    return Err(ToolError::InvalidArgs(
                        "`filename` must not be empty (omit it to keep the original name)".into(),
                    ));
                }
                Some(name) if name.contains('/') => {
                    return Err(ToolError::InvalidArgs(format!(
                        "`filename` must not contain `/` (got `{name}`)"
                    )));
                }
                Some(name) => name.to_string(),
                None => src_file.clone(),
            };

            // Already a chip on this reply under that name and backed by the
            // same object? Then the user can already download it; a second
            // marker would just draw a duplicate.
            if src_turn == turn_id
                && desired == src_file
                && has_marker(&ctx.db, turn_id).await?.contains(&desired)
            {
                return Ok(json!({
                    "filename": desired,
                    "id": format!("{turn_id}/{desired}"),
                    "mime": mime,
                    "size": size,
                    "already_attached": true,
                    "rendered": "This file is already a download chip on your \
                                 current reply — nothing more to do, and do NOT \
                                 repeat the marker text in your prose.",
                }));
            }

            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &desired)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;

            // Same key on both sides means the object is already sitting under
            // this turn with the right name (a hidden data file the render left
            // behind) — it only lacked a marker. S3 rejects a self-copy, so
            // skip it and just publish the chip.
            if !(src_turn == turn_id && src_file == filename) {
                chat_attachments::copy_object_as(s3, &src_turn, &src_file, turn_id, &filename)
                    .await
                    .map_err(|e| {
                        ToolError::Failed(format!(
                            "could not copy `{src_turn}/{src_file}` into this reply: {e}"
                        ))
                    })?;
            }

            let marker = session_core::attachments::marker_line(
                &filename,
                &mime,
                &chat_attachments::proxy_url(turn_id, &filename),
                size,
            );
            chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;

            Ok(json!({
                "filename": filename,
                "id": format!("{turn_id}/{filename}"),
                "mime": mime,
                "size": size,
                "source_id": format!("{src_turn}/{src_file}"),
                "rendered": "Attached to your reply as a download (images show \
                             inline) — do NOT repeat the marker text or the file's \
                             contents in your prose. One short sentence saying what \
                             the file is is enough.",
            }))
        })
    }
}

/// Hand a canvas document to the user as a file: write its current version
/// under this turn and splice the chip.
///
/// The bytes are a *snapshot*. The document keeps living in the panel and can
/// move on (model edit, user hand-edit), so the result says which version was
/// handed over — a later "you said X" is answerable, and the model knows to
/// re-offer after further edits rather than assume the file is current.
#[allow(clippy::too_many_arguments)]
async fn offer_document(
    ctx: &ToolContext,
    turn_id: &str,
    s3: &gateway_core::server::config::S3Config,
    reservations: &tokio::sync::Mutex<std::collections::HashSet<String>>,
    args: &OfferArgs,
    title: &str,
    format: DocumentFormat,
    version: i64,
    content: &str,
) -> ToolResult {
    let desired = match args.filename.as_deref().map(str::trim) {
        Some("") => {
            return Err(ToolError::InvalidArgs(
                "`filename` must not be empty (omit it for a name derived from the \
                 document title)"
                    .into(),
            ));
        }
        Some(name) if name.contains('/') => {
            return Err(ToolError::InvalidArgs(format!(
                "`filename` must not contain `/` (got `{name}`)"
            )));
        }
        Some(name) => name.to_string(),
        None => document_filename(title, format),
    };
    let filename = chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &desired)
        .await
        .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;
    let bytes = content.as_bytes().to_vec();
    let size = bytes.len() as u64;
    let mime = document_mime(format);
    chat_attachments::upload(s3, turn_id, &filename, mime, bytes)
        .await
        .map_err(|e| ToolError::Failed(format!("storing the document as a file: {e}")))?;
    let marker = session_core::attachments::marker_line(
        &filename,
        mime,
        &chat_attachments::proxy_url(turn_id, &filename),
        size,
    );
    chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
        .await
        .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;
    Ok(json!({
        "filename": filename,
        "id": format!("{turn_id}/{filename}"),
        "mime": mime,
        "size": size,
        "source_document": args.id,
        "source_version": version,
        "rendered": format!(
            "Attached to your reply as a download — v{version} of the canvas \
             document, exactly what the panel shows. Do NOT repeat the marker \
             text or the file's contents in your prose. The file is a snapshot: \
             after another edit, offer it again if the user wants the new state."
        ),
    }))
}

/// Filename for a handed-over canvas document: a slug of its title plus the
/// format's conventional extension, so the download lands in the user's
/// Downloads folder with a name they can recognise a week later.
fn document_filename(title: &str, format: DocumentFormat) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let stem = slug.trim_matches('-').chars().take(48).collect::<String>();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "document" } else { stem };
    format!("{stem}.{}", format.file_ext())
}

/// Content type for a canvas document's format. Drives both the chip's icon
/// and what the browser does with the download.
fn document_mime(format: DocumentFormat) -> &'static str {
    match format {
        DocumentFormat::Markdown => "text/markdown",
        DocumentFormat::Text => "text/plain",
        DocumentFormat::Html => "text/html",
        DocumentFormat::Json => "application/json",
        DocumentFormat::Toml => "application/toml",
        DocumentFormat::Yaml => "application/yaml",
        // No registered type; `text/plain` at least makes it readable
        // everywhere, and the `.typ` extension carries the real meaning.
        DocumentFormat::Typst => "text/plain",
    }
}

/// Resolve a model-supplied id to a source, session-scoped.
///
/// Marker-backed ids go through `resolve_attachment`, which also accepts a
/// bare filename (newest match wins) — the same leniency every other
/// attachment-taking tool has, because models lose track of turn ids across
/// rounds. An id with no marker is only accepted in `<turn_id>/<filename>`
/// form and only when that turn belongs to *this* session.
async fn locate(
    db: &gateway_core::server::db::Pool,
    session_id: &str,
    given: &str,
) -> Result<Source, ToolError> {
    let atts = chat_attachments::list_session_attachments(db, session_id)
        .await
        .map_err(|e| ToolError::Failed(format!("listing attachments: {e}")))?;
    if let Some(found) = chat_attachments::resolve_attachment(&atts, given) {
        return Ok(Source::Listed(found.clone()));
    }
    // A canvas document id carries no slash, so it can't collide with an
    // attachment id. `get_version` is session-scoped, which is the same
    // guarantee the attachment paths get.
    if !given.contains('/')
        && let Some((doc, ver)) = documents::get_version(db, session_id, given, None)
            .await
            .map_err(|e| ToolError::Failed(format!("reading canvas document: {e}")))?
    {
        return Ok(Source::Document {
            title: doc.title,
            format: doc.format,
            version: ver.version,
            content: ver.content,
        });
    }
    let (turn_id, filename) = given.split_once('/').ok_or_else(|| {
        ToolError::InvalidArgs(format!(
            "no file or document named `{given}` in this conversation — call \
             `list_attachments` / `list_documents` to see what exists, or pass a \
             full `<turn_id>/<filename>` id"
        ))
    })?;
    if filename.is_empty() || filename.contains('/') {
        return Err(ToolError::InvalidArgs(format!(
            "`{given}` is not a `<turn_id>/<filename>` id"
        )));
    }
    if !session_core::db::turn_in_session(db, turn_id, session_id)
        .await
        .map_err(|e| ToolError::Failed(format!("checking turn ownership: {e}")))?
    {
        return Err(ToolError::InvalidArgs(format!(
            "`{given}` does not belong to this conversation — you can only hand \
             over files from the current chat (see `list_attachments`)"
        )));
    }
    Ok(Source::Unlisted {
        turn_id: turn_id.to_string(),
        filename: filename.to_string(),
    })
}

/// Filenames already carrying a marker on `turn_id`'s content — the chips
/// the user can see on the reply being written.
async fn has_marker(
    db: &gateway_core::server::db::Pool,
    turn_id: &str,
) -> Result<Vec<String>, ToolError> {
    let content = chat::get_content(db, turn_id)
        .await
        .map_err(|e| ToolError::Failed(format!("read turn content: {e}")))?
        .unwrap_or_default();
    Ok(
        session_core::attachments::parse_markers_for_turn(&content, turn_id)
            .into_iter()
            .map(|a| a.filename)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(OfferDownload.id(), OfferDownload.schema().function.name);
    }

    async fn pool_with_session() -> gateway_core::server::db::Pool {
        let pool = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for s in ["s1", "s2"] {
            sqlx::query(
                r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
                   VALUES (?, 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
            )
            .bind(s)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    async fn insert_turn(
        pool: &gateway_core::server::db::Pool,
        session: &str,
        id: &str,
        seq: i64,
        role: &str,
        content: &str,
    ) {
        let (user_content, content_col) = if role == "user" {
            (Some(content), None)
        } else {
            (None, Some(content))
        };
        sqlx::query(
            r#"INSERT INTO chat_turns (id, session_id, seq, role, user_content, content,
                                       status, created_at)
               VALUES (?, ?, ?, ?, ?, ?, 'completed', '2026-01-01T00:00:00Z')"#,
        )
        .bind(id)
        .bind(session)
        .bind(seq)
        .bind(role)
        .bind(user_content)
        .bind(content_col)
        .execute(pool)
        .await
        .unwrap();
    }

    fn ctx(pool: gateway_core::server::db::Pool, session: Option<&str>) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: Some("t9".into()),
            session_id: session.map(str::to_string),
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
            ocr: None,
            sandbox_lease: None,
            browser_lease: None,
            crypto: None,
            push: None,
            model: None,
        }
    }

    #[tokio::test]
    async fn errors_cleanly_without_a_session() {
        let pool = pool_with_session().await;
        let err = OfferDownload
            .run(ctx(pool, None), json!({"id": "t1/data.csv"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("chat session"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_s3_not_configured() {
        let pool = pool_with_session().await;
        let err = OfferDownload
            .run(ctx(pool, Some("s1")), json!({"id": "t1/data.csv"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("not configured"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn locates_a_marker_backed_file_by_bare_filename() {
        let pool = pool_with_session().await;
        let marker = session_core::attachments::marker_line(
            "data.csv",
            "text/csv",
            "/chat/attachment/t1/data.csv",
            10,
        );
        insert_turn(&pool, "s1", "t1", 0, "user", &format!("here\n\n{marker}\n")).await;

        // Bare filename resolves, and the marker's own mime/size come along
        // so the copy needs no storage round-trip to write its chip.
        let src = locate(&pool, "s1", "data.csv").await.unwrap();
        assert_eq!(
            src,
            Source::Listed(AttachmentRef {
                id: "t1/data.csv".into(),
                turn_id: "t1".into(),
                filename: "data.csv".into(),
                mime: "text/csv".into(),
                size: 10,
            })
        );
    }

    #[tokio::test]
    async fn locates_an_unlisted_object_in_a_turn_of_this_session() {
        let pool = pool_with_session().await;
        // A typst render's hidden `.json` edit base: the turn exists, the
        // object exists, but no marker was ever spliced for it.
        insert_turn(&pool, "s1", "t1", 0, "assistant", "rendered").await;
        let src = locate(&pool, "s1", "t1/presentation.json").await.unwrap();
        assert_eq!(
            src,
            Source::Unlisted {
                turn_id: "t1".into(),
                filename: "presentation.json".into(),
            }
        );
    }

    #[tokio::test]
    async fn refuses_a_turn_from_another_conversation() {
        let pool = pool_with_session().await;
        insert_turn(&pool, "s2", "t2", 0, "assistant", "someone else's turn").await;
        let err = locate(&pool, "s1", "t2/secret.pdf").await.unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => {
                assert!(
                    msg.contains("does not belong to this conversation"),
                    "{msg}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_an_unknown_bare_filename() {
        let pool = pool_with_session().await;
        insert_turn(&pool, "s1", "t1", 0, "assistant", "nothing here").await;
        let err = locate(&pool, "s1", "nope.json").await.unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains("list_attachments"), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn locates_a_canvas_document_by_id() {
        let pool = pool_with_session().await;
        let id = documents::new_id();
        documents::create(
            &pool,
            &id,
            "s1",
            "u1",
            "Deck data",
            DocumentFormat::Json,
            r#"{"deck":{"slides":[]}}"#,
            None,
        )
        .await
        .unwrap();
        // v2 so the test would catch handing over the wrong (initial) version.
        documents::append_version(
            &pool,
            "s1",
            &id,
            r#"{"deck":{"slides":[1]}}"#,
            None,
            None,
            documents::VersionAuthor::Assistant,
        )
        .await
        .unwrap();

        match locate(&pool, "s1", &id).await.unwrap() {
            Source::Document {
                title,
                format,
                version,
                content,
            } => {
                assert_eq!(title, "Deck data");
                assert_eq!(format, DocumentFormat::Json);
                assert_eq!(version, 2, "the latest version is what the panel shows");
                assert!(content.contains("slides"));
            }
            other => panic!("expected a document, got {other:?}"),
        }

        // Another conversation's document is not reachable, and the error names
        // both inventories rather than leaking which id exists.
        let err = locate(&pool, "s2", &id).await.unwrap_err();
        assert!(
            matches!(&err, ToolError::InvalidArgs(m) if m.contains("list_documents")),
            "{err:?}"
        );
    }

    #[test]
    fn document_filenames_are_slugged_titles_with_the_format_extension() {
        assert_eq!(
            document_filename("croit — LLM Gateway", DocumentFormat::Json),
            "croit-llm-gateway.json"
        );
        assert_eq!(
            document_filename("Migration plan", DocumentFormat::Markdown),
            "migration-plan.md"
        );
        assert_eq!(
            document_filename("deck.typ", DocumentFormat::Typst),
            "deck-typ.typ"
        );
        // A title with nothing sluggable still yields a usable filename.
        assert_eq!(
            document_filename("——", DocumentFormat::Text),
            "document.txt"
        );
    }

    #[test]
    fn document_mimes_make_text_downloads_readable() {
        assert_eq!(document_mime(DocumentFormat::Json), "application/json");
        assert_eq!(document_mime(DocumentFormat::Markdown), "text/markdown");
        // No registered type for typst; plain text keeps it openable.
        assert_eq!(document_mime(DocumentFormat::Typst), "text/plain");
    }

    #[tokio::test]
    async fn marker_filenames_are_read_back_from_the_turn() {
        let pool = pool_with_session().await;
        let marker = session_core::attachments::marker_line(
            "deck.json",
            "application/json",
            "/chat/attachment/t9/deck.json",
            42,
        );
        insert_turn(
            &pool,
            "s1",
            "t9",
            0,
            "assistant",
            &format!("done\n\n{marker}\n"),
        )
        .await;
        assert_eq!(has_marker(&pool, "t9").await.unwrap(), vec!["deck.json"]);
    }
}
