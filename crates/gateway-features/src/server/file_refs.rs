// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! One way to name a file in a conversation, for every tool that takes one.
//!
//! A conversation holds files in two stores — immutable attachments keyed
//! `<turn_id>/<filename>`, and mutable canvas documents keyed `doc_…` — and
//! the tools grew four dialects for pointing at them: the raw id, a bare
//! filename (newest match wins), an `att:` ref for typst image fields, and a
//! `document_id` in a separate argument. Every tool then re-implemented its
//! own subset, so which spellings worked depended on which tool you called:
//! `run_in_sandbox` took ids and filenames in `attachments` but documents only
//! in `documents`, `fetch_attachment` couldn't read a document at all, and a
//! model that had just been handed a `document_id` had to know which argument
//! it belonged in.
//!
//! [`resolve`] accepts all of them and says what it found. Tools call it
//! instead of parsing, so a reference the model got from *any* result works in
//! *any* argument, and the accepted syntax is documented (and tested) once.
//!
//! Session scoping is part of resolution, never an afterthought:
//!
//! - a marker-backed attachment is proven in-session by the enumeration
//!   itself (only this conversation's turns are read),
//! - an *unlisted* `<turn>/<file>` (a typst render's hidden `.json`, an
//!   intermediate artifact) is checked against `chat_turns.session_id`, and
//! - a document id goes through the session-scoped `documents::get_version`.
//!
//! So a guessed id from another conversation resolves to `NotFound`, with the
//! same message as a typo — no existence leak.

use gateway_core::server::db::Pool;
use gateway_core::server::db::documents::{self, Document, DocumentFormat, DocumentVersion};

use crate::server::chat_attachments::{self, AttachmentRef};

/// Prefixes a model may put in front of a reference. `att:` is the typst
/// image-field syntax and shows up in copied deck data; `doc:` and `file:`
/// are spellings models reach for unprompted. Stripping them costs nothing
/// and turns a class of "invalid reference" errors into working calls.
const PREFIXES: &[&str] = &["att:", "attachment:", "doc:", "document:", "file:"];

/// Appended to a `NotFound` whose reference contains a `/`.
///
/// An attachment id (`<turn_id>/<filename>`) and a sandbox working-directory
/// path (`docs/backend.md`) are the same shape, so a model that has just
/// written the latter in `/work` passes exactly that — and the bare "no file
/// named that in this conversation" reads as *your file is gone* rather than
/// *you named the wrong store*. A model that believes its output vanished stops
/// trying to deliver it properly and starts inventing ways to hand it over.
///
/// Appended rather than substituted on a guessed classification. The two shapes
/// cannot be told apart reliably — turn ids are not required to look any
/// particular way, so a "first segment isn't a UUID → it's a path" rule
/// mislabels a real cross-session id — and a lookup can't disambiguate either
/// without leaking whether another conversation holds that id. Saying both
/// possibilities costs a sentence and is never wrong.
const SANDBOX_PATH_HINT: &str = "If that is a path inside the sandbox's working \
     directory, note that `/work` is a different store: nothing outside a \
     `run_in_sandbox` call can read it. A file the sandbox produced becomes \
     referenceable only once a run returned it in `artifacts`, under the flattened \
     `name`/`id` shown there (a file written to `docs/backend.md` is delivered as \
     `docs-backend.md`). If it never appeared in an `artifacts` list, it was never \
     delivered — write it again and use the `id` the result gives you.";

/// A resolved reference: which store holds the bytes, and everything the
/// caller needs to name, size, or read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRef {
    /// An attachment carrying a chat marker — mime and size are known from
    /// the marker, so a chip can be written without touching storage.
    Attachment(AttachmentRef),
    /// A `<turn_id>/<filename>` under a turn of this session that carries no
    /// marker: a typst render's hidden `.json` data base, or an intermediate
    /// artifact. Reachable by id, so metadata needs a storage HEAD.
    UnlistedAttachment { turn_id: String, filename: String },
    /// A canvas document, resolved to a concrete version (the latest unless
    /// the caller asked for one).
    Document {
        doc: Box<Document>,
        version: Box<DocumentVersion>,
    },
}

/// Why a reference didn't resolve. Callers map these to their own tool
/// errors — the strings here name the way out (`list_attachments` /
/// `list_documents`) rather than describing the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    /// Nothing in this conversation goes by that name — including anything
    /// that exists but belongs to another conversation.
    NotFound(String),
    /// A canvas document that is in the bin. Distinct from `NotFound`: the
    /// fix is `undelete_document`, not a different id.
    Deleted(String),
    /// The reference names a document, but this code path has no
    /// conversation to scope the lookup to (`/v1` proxy).
    NoSession,
    /// A read failed (storage / DB), as opposed to the reference being wrong.
    Failed(String),
}

impl std::fmt::Display for RefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefError::NotFound(given) => {
                write!(
                    f,
                    "no file or document named `{given}` in this conversation — call \
                     `list_attachments` / `list_documents` to see what exists, or pass a \
                     full `<turn_id>/<filename>` id"
                )?;
                if given.contains('/') {
                    write!(f, ". {SANDBOX_PATH_HINT}")?;
                }
                Ok(())
            }
            RefError::Deleted(given) => write!(
                f,
                "canvas document `{given}` is deleted — call `undelete_document` first \
                 if you meant to use it"
            ),
            RefError::NoSession => write!(
                f,
                "canvas documents and conversation files are only available inside a \
                 chat session"
            ),
            RefError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl FileRef {
    /// User-facing filename. A document has a title, not a filename, so it
    /// gets the format's conventional extension appended by the caller that
    /// materialises it — here the raw title is enough to name it in a note.
    pub fn name(&self) -> &str {
        match self {
            FileRef::Attachment(a) => &a.filename,
            FileRef::UnlistedAttachment { filename, .. } => filename,
            FileRef::Document { doc, .. } => &doc.title,
        }
    }

    /// The canonical id to echo back: `<turn>/<file>` or the document id.
    /// Round-trips through [`resolve`].
    pub fn id(&self) -> String {
        match self {
            FileRef::Attachment(a) => a.id.clone(),
            FileRef::UnlistedAttachment { turn_id, filename } => format!("{turn_id}/{filename}"),
            FileRef::Document { doc, .. } => doc.id.clone(),
        }
    }

    /// Whether this is the mutable, user-editable side of the conversation's
    /// files. Callers that must not write through a snapshot check this.
    pub fn is_document(&self) -> bool {
        matches!(self, FileRef::Document { .. })
    }

    /// Text content when the reference names something textual and already
    /// in hand — a canvas document. `None` for attachments, whose bytes live
    /// in storage (fetch them with the storage config).
    pub fn text(&self) -> Option<&str> {
        match self {
            FileRef::Document { version, .. } => Some(&version.content),
            _ => None,
        }
    }

    /// Content type: the marker's mime for an attachment, the format's for a
    /// document. `None` when only a HEAD can tell (an unlisted attachment).
    pub fn mime(&self) -> Option<String> {
        match self {
            FileRef::Attachment(a) => Some(a.mime.clone()),
            FileRef::UnlistedAttachment { .. } => None,
            FileRef::Document { doc, .. } => Some(document_mime(doc.format).to_string()),
        }
    }
}

/// Content type for a canvas document's format — what a browser should do
/// with it once it is written out as a file. Single source of truth so the
/// download chip, the sandbox staging note and any future export agree.
pub fn document_mime(format: DocumentFormat) -> &'static str {
    match format {
        DocumentFormat::Markdown => "text/markdown",
        DocumentFormat::Text => "text/plain",
        DocumentFormat::Html => "text/html",
        DocumentFormat::Json => "application/json",
        DocumentFormat::Toml => "application/toml",
        DocumentFormat::Yaml => "application/yaml",
        // No registered type; `text/plain` keeps it readable everywhere and
        // the `.typ` extension carries the real meaning.
        DocumentFormat::Typst => "text/plain",
    }
}

/// Strip a `att:` / `doc:` / `file:` style prefix and surrounding whitespace.
/// Pure, so callers that only need the bare form (typst's image staging walks
/// raw JSON strings) can use it without a DB.
pub fn strip_prefix(given: &str) -> &str {
    let given = given.trim();
    for p in PREFIXES {
        if let Some(rest) = given.strip_prefix(p) {
            return rest.trim();
        }
    }
    given
}

/// Whether `given` looks like a canvas document id rather than an attachment
/// reference. Document ids are `doc_<uuid-simple>`; an attachment id always
/// carries the `<turn>/<file>` slash, and a bare filename with no slash could
/// be either, so the `doc_` shape is what disambiguates.
pub fn looks_like_document(given: &str) -> bool {
    let g = strip_prefix(given);
    g.starts_with("doc_") && !g.contains('/')
}

/// Resolve a model-supplied reference against one conversation.
///
/// Order matters and encodes the precedence a model expects: an exact
/// attachment id or filename wins (that is what `list_attachments` hands
/// out), then a document id, then an unlisted `<turn>/<file>`. `version` is
/// only meaningful for a document; it is ignored for attachments, which have
/// no versions.
pub async fn resolve(
    db: &Pool,
    session_id: Option<&str>,
    given: &str,
    version: Option<i64>,
) -> Result<FileRef, RefError> {
    let given_raw = given.trim();
    let bare = strip_prefix(given_raw);
    if bare.is_empty() {
        return Err(RefError::NotFound(given_raw.to_string()));
    }
    let Some(session_id) = session_id else {
        return Err(RefError::NoSession);
    };

    // Marker-backed attachments first: exact `<turn>/<file>` id, else the
    // newest attachment with that filename. Both are session-scoped by
    // construction — the enumeration reads only this conversation's turns.
    let atts = chat_attachments::list_session_attachments(db, session_id)
        .await
        .map_err(|e| RefError::Failed(format!("listing attachments: {e}")))?;
    if let Some(found) = chat_attachments::resolve_attachment(&atts, bare) {
        return Ok(FileRef::Attachment(found.clone()));
    }

    // A canvas document, by id.
    if looks_like_document(bare) {
        return match documents::get_version(db, session_id, bare, version)
            .await
            .map_err(|e| RefError::Failed(format!("reading canvas document: {e}")))?
        {
            Some((doc, _)) if doc.is_deleted() => Err(RefError::Deleted(bare.to_string())),
            Some((doc, version)) => Ok(FileRef::Document {
                doc: Box::new(doc),
                version: Box::new(version),
            }),
            None => Err(RefError::NotFound(given_raw.to_string())),
        };
    }

    // A document by *title*, when it is unambiguous. Models refer to "the
    // migration plan" long after the id scrolled out of their context, and a
    // wrong guess here would be a silent edit of the wrong document — so an
    // ambiguous title is a miss, not a coin flip.
    if !bare.contains('/') {
        let docs = documents::list_for_session(db, session_id, false)
            .await
            .map_err(|e| RefError::Failed(format!("listing canvas documents: {e}")))?;
        let mut matches = docs
            .iter()
            .filter(|d| d.title.eq_ignore_ascii_case(bare) || file_name_of(d) == bare);
        if let (Some(doc), None) = (matches.next(), matches.next())
            && let Some((doc, version)) = documents::get_version(db, session_id, &doc.id, version)
                .await
                .map_err(|e| RefError::Failed(format!("reading canvas document: {e}")))?
        {
            return Ok(FileRef::Document {
                doc: Box::new(doc),
                version: Box::new(version),
            });
        }
    }

    // Finally an unlisted `<turn>/<file>`: no marker anywhere (so the
    // enumeration above missed it), but the turn is ours.
    if let Some((turn_id, filename)) = bare.split_once('/')
        && !filename.is_empty()
        && !filename.contains('/')
    {
        let ours = session_core::db::turn_in_session(db, turn_id, session_id)
            .await
            .map_err(|e| RefError::Failed(format!("checking turn ownership: {e}")))?;
        if ours {
            return Ok(FileRef::UnlistedAttachment {
                turn_id: turn_id.to_string(),
                filename: filename.to_string(),
            });
        }
    }
    Err(RefError::NotFound(given_raw.to_string()))
}

/// The filename a document gets when materialised: its title slugged, plus
/// the format's extension. Shared so a document staged into a sandbox, handed
/// over as a download, or matched by name in [`resolve`] all agree on it.
pub fn file_name_of(doc: &Document) -> String {
    format!("{}.{}", slug(&doc.title), doc.format.file_ext())
}

/// Lowercase ASCII slug for a document title: alphanumerics kept, every other
/// run collapsed to a single `-`, capped so a long title stays a usable
/// filename. Empty titles fall back to `document`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(48).collect();
    let trimmed = trimmed.trim_matches('-');
    if trimmed.is_empty() {
        "document".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db::documents::VersionAuthor;

    async fn pool() -> Pool {
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

    async fn turn_with_marker(pool: &Pool, session: &str, turn: &str, marker: Option<&str>) {
        let content = marker.map(|m| format!("here\n\n{m}\n"));
        sqlx::query(
            r#"INSERT INTO chat_turns (id, session_id, seq, role, content, status, created_at)
               VALUES (?, ?, 0, 'assistant', ?, 'completed', '2026-01-01T00:00:00Z')"#,
        )
        .bind(turn)
        .bind(session)
        .bind(content)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn doc(pool: &Pool, session: &str, title: &str, format: DocumentFormat) -> String {
        let id = documents::new_id();
        documents::create(pool, &id, session, "u1", title, format, "content\n", None)
            .await
            .unwrap();
        id
    }

    #[test]
    fn prefixes_are_stripped_and_document_ids_recognised() {
        assert_eq!(strip_prefix(" att:t1/logo.png "), "t1/logo.png");
        assert_eq!(strip_prefix("doc:doc_abc"), "doc_abc");
        assert_eq!(strip_prefix("file:notes.md"), "notes.md");
        assert_eq!(strip_prefix("t1/plain.txt"), "t1/plain.txt");
        assert!(looks_like_document("doc_abc"));
        assert!(looks_like_document("document:doc_abc"));
        // A file that merely starts with `doc_` but sits under a turn is an
        // attachment, not a document id.
        assert!(!looks_like_document("t1/doc_abc.pdf"));
        assert!(!looks_like_document("notes.md"));
    }

    #[test]
    fn document_filenames_slug_the_title() {
        let now: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let d = Document {
            id: "doc_x".into(),
            session_id: "s1".into(),
            title: "croit — LLM Gateway".into(),
            format: DocumentFormat::Json,
            current_ver: 1,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        assert_eq!(file_name_of(&d), "croit-llm-gateway.json");
        assert_eq!(slug("——"), "document");
    }

    #[tokio::test]
    async fn one_resolver_accepts_every_dialect() {
        let pool = pool().await;
        let marker = session_core::attachments::marker_line(
            "logo.png",
            "image/png",
            "/chat/attachment/t1/logo.png",
            9,
        );
        turn_with_marker(&pool, "s1", "t1", Some(&marker)).await;
        let doc_id = doc(&pool, "s1", "Migration plan", DocumentFormat::Markdown).await;

        // Exact attachment id, bare filename, and the typst `att:` spelling
        // all land on the same attachment.
        for given in ["t1/logo.png", "logo.png", "att:t1/logo.png", "att:logo.png"] {
            let r = resolve(&pool, Some("s1"), given, None).await.unwrap();
            assert_eq!(r.id(), "t1/logo.png", "{given}");
            assert_eq!(r.mime().as_deref(), Some("image/png"));
            assert!(!r.is_document());
        }
        // Document by id, by prefixed id, by title, and by materialised
        // filename — the four ways a model will have seen it referred to.
        for given in [
            doc_id.as_str(),
            &format!("doc:{doc_id}"),
            "Migration plan",
            "migration-plan.md",
        ] {
            let r = resolve(&pool, Some("s1"), given, None).await.unwrap();
            assert_eq!(r.id(), doc_id, "{given}");
            assert!(r.is_document());
            assert_eq!(r.text(), Some("content\n"));
            assert_eq!(r.mime().as_deref(), Some("text/markdown"));
        }
    }

    #[tokio::test]
    async fn an_unlisted_object_under_our_turn_resolves() {
        // A typst render's hidden `.json`: the turn is ours, the marker was
        // never written, so only the id can find it.
        let pool = pool().await;
        turn_with_marker(&pool, "s1", "t1", None).await;
        let r = resolve(&pool, Some("s1"), "t1/presentation.json", None)
            .await
            .unwrap();
        assert_eq!(
            r,
            FileRef::UnlistedAttachment {
                turn_id: "t1".into(),
                filename: "presentation.json".into()
            }
        );
        // Nothing is known about its bytes without a HEAD.
        assert!(r.mime().is_none());
    }

    #[tokio::test]
    async fn another_conversations_ids_are_simply_not_found() {
        let pool = pool().await;
        turn_with_marker(&pool, "s2", "t2", None).await;
        let other_doc = doc(&pool, "s2", "Secret", DocumentFormat::Markdown).await;

        // A turn and a document that exist — in someone else's conversation.
        // Both report the same "not found" as a typo would, so nothing leaks.
        for given in ["t2/secret.pdf", other_doc.as_str(), "Secret"] {
            match resolve(&pool, Some("s1"), given, None).await {
                Err(RefError::NotFound(g)) => assert_eq!(g, given),
                other => panic!("expected NotFound for {given}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_deleted_document_is_distinguishable_from_a_missing_one() {
        let pool = pool().await;
        let id = doc(&pool, "s1", "Draft", DocumentFormat::Markdown).await;
        documents::soft_delete(&pool, "s1", &id).await.unwrap();
        match resolve(&pool, Some("s1"), &id, None).await {
            // The fix is `undelete_document`, not another id — so the caller
            // must be able to say so.
            Err(RefError::Deleted(g)) => assert_eq!(g, id),
            other => panic!("expected Deleted, got {other:?}"),
        }
        // And a deleted document no longer answers to its title.
        assert!(matches!(
            resolve(&pool, Some("s1"), "Draft", None).await,
            Err(RefError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn an_ambiguous_title_is_a_miss_not_a_guess() {
        // Two documents called the same thing: picking one would silently
        // edit the wrong file.
        let pool = pool().await;
        doc(&pool, "s1", "Notes", DocumentFormat::Markdown).await;
        doc(&pool, "s1", "Notes", DocumentFormat::Markdown).await;
        assert!(matches!(
            resolve(&pool, Some("s1"), "Notes", None).await,
            Err(RefError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_document_resolves_at_the_asked_for_version() {
        let pool = pool().await;
        let id = doc(&pool, "s1", "Plan", DocumentFormat::Markdown).await;
        documents::append_version(&pool, "s1", &id, "v2\n", None, None, VersionAuthor::User)
            .await
            .unwrap();
        // Latest by default…
        let r = resolve(&pool, Some("s1"), &id, None).await.unwrap();
        assert_eq!(r.text(), Some("v2\n"));
        // …and an older one on request, which is what an export of "the
        // version the user is looking at" needs.
        let r = resolve(&pool, Some("s1"), &id, Some(1)).await.unwrap();
        assert_eq!(r.text(), Some("content\n"));
    }

    #[tokio::test]
    async fn off_chat_paths_say_so_instead_of_reporting_a_missing_file() {
        let pool = pool().await;
        assert!(matches!(
            resolve(&pool, None, "doc_abc", None).await,
            Err(RefError::NoSession)
        ));
    }

    /// An attachment id and a sandbox working-directory path are the same
    /// shape — `<something>/<filename>` — so a model that has just written
    /// `docs/backend.md` in `/work` passes exactly that here. The bare "no file
    /// named that in this conversation" reads as *your file is gone* rather
    /// than *you named the wrong store*, and a model that believes its output
    /// vanished starts inventing ways to hand it over.
    #[tokio::test]
    async fn a_slashed_reference_also_explains_the_sandbox_store() {
        let pool = pool().await;
        let msg = resolve(&pool, Some("s1"), "croit-app-reference/CLAUDE.md", None)
            .await
            .unwrap_err()
            .to_string();
        // The generic wording stays, so this reads the same everywhere it is
        // surfaced…
        assert!(msg.contains("no file or document named"), "{msg}");
        assert!(msg.contains("list_attachments"), "{msg}");
        // …and the other possibility is spelled out rather than guessed at.
        assert!(msg.contains("working directory"), "{msg}");
        assert!(msg.contains("artifacts"), "{msg}");
    }

    #[tokio::test]
    async fn a_bare_filename_gets_no_sandbox_hint() {
        // Nothing about `report.pdf` suggests a path, so the extra sentence
        // would only be noise — and noise is how the useful half gets skipped.
        let pool = pool().await;
        let msg = resolve(&pool, Some("s1"), "report.pdf", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no file or document named"), "{msg}");
        assert!(!msg.contains("working directory"), "{msg}");
    }
}
