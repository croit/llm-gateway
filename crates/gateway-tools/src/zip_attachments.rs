// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `zip_attachments` — bundle several files this conversation already
//! holds into one `.zip` and hand it to the user as a single download.
//!
//! The gap this closes: a turn that produces a *set* of files (a docs tree,
//! a render's page images, six CSV exports) had only one way to deliver
//! them — one `offer_download` per file, so one chip per file. Past about
//! four that stops being a delivery and starts being a scavenger hunt, and
//! on a phone the chip strip is where the reply used to be. Users asked for
//! "just give me a zip".
//!
//! It is the multi-file sibling of [`crate::offer_download`] and inherits
//! its reference model wholesale: every entry is named by the same
//! `<turn_id>/<filename>` / `document_id` / bare-filename spellings, and
//! `file_refs::resolve` does the session scoping. The important difference
//! is that the bytes cannot stay inside S3 — a zip has to be *built* — so
//! this tool reads each member into memory. That is what the two caps below
//! are for; see [`MAX_ENTRIES`] and [`MAX_TOTAL_BYTES`].
//!
//! Nothing transits the model either way: it passes references, and gets
//! back a manifest. The archive's bytes never enter the conversation.

use std::collections::HashSet;
use std::io::Write as _;

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use gateway_features::server::chat_attachments;
use gateway_features::server::file_refs::{self, FileRef};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Most members one archive may have.
///
/// Not a storage limit — a guard on the *manifest*. Every entry costs a
/// resolve + an S3 GET, so a model that passed a hundred ids would stall the
/// turn for a minute before the user saw anything. Well above any real
/// "here are my files" set.
const MAX_ENTRIES: usize = 64;

/// Cap on total *uncompressed* member bytes.
///
/// The archive is assembled in RAM (the zip central directory is written
/// last, so streaming it straight to S3 would mean a second pass anyway), and
/// several concurrent turns share one gateway process. 256 MiB of members is
/// far more than a document bundle needs and still bounded per turn.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Default archive name when the model doesn't pass one.
const DEFAULT_FILENAME: &str = "attachments.zip";

pub struct ZipAttachments;

#[derive(Deserialize)]
struct ZipArgs {
    ids: Vec<String>,
    /// Optional name for the archive. `.zip` is appended when missing, so a
    /// model that passes `croit-docs` still produces a file the OS opens.
    #[serde(default)]
    filename: Option<String>,
}

/// One resolved member, ready to write into the archive.
struct Member {
    /// Name *inside* the archive (deduped across members).
    entry: String,
    /// The id this member came from, for the manifest.
    source_id: String,
    bytes: Vec<u8>,
}

impl Tool for ZipAttachments {
    fn id(&self) -> &str {
        "zip_attachments"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Bundle several files from this conversation into ONE .zip and \
             attach it to your current reply as a single download. Pass `ids` — \
             the same references `offer_download` takes: `<turn_id>/<filename>` \
             ids from `list_attachments`, canvas `document_id`s (the current \
             version is written into the archive), ids a `run_in_sandbox` result \
             reported in `artifacts`, or bare filenames from this conversation \
             (newest match wins). Use this instead of calling `offer_download` \
             once per file whenever you are handing over a *set* — a docs \
             bundle, an export batch, every page of a render: one chip the user \
             clicks once beats six they have to hunt for, and on a phone the \
             chip strip buries the reply. Prefer `offer_download` for a single \
             file (a one-entry zip just makes the user unpack it). The bytes are \
             read inside the gateway — you do NOT need to read, repeat, or \
             regenerate any file's contents to include it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["ids"],
                "properties": {
                    "ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ENTRIES,
                        "items": { "type": "string" },
                        "description": "The files to bundle, in the order they \
                                        should appear. Each is a \
                                        `<turn_id>/<filename>` attachment id, a \
                                        canvas `document_id`, or a bare filename \
                                        from this conversation. Not a sandbox \
                                        working-directory path — only what a run \
                                        returned in `artifacts`."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional name for the archive, e.g. \
                                        `croit-cowork-docs.zip`. No slashes. \
                                        `.zip` is added if you leave it off. \
                                        Defaults to `attachments.zip` — pass \
                                        something descriptive, the user sees this \
                                        name in their Downloads folder."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ZipArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{ids: [...], filename?}}: {e}"))
            })?;
            // Argument shape before the environment gates: when a model passes
            // an empty list on a gateway without storage configured, "you named
            // no files" is the actionable half of the truth and the only half
            // the model can act on.
            if args.ids.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "`ids` must name at least one file — call `list_attachments` to \
                     see what this conversation holds"
                        .into(),
                ));
            }
            if args.ids.len() > MAX_ENTRIES {
                return Err(ToolError::InvalidArgs(format!(
                    "`ids` has {} entries; at most {MAX_ENTRIES} can go into one \
                     archive. Send the most useful subset, or split it across two \
                     archives.",
                    args.ids.len()
                )));
            }
            // Rejected early for the same reason: a name with a `/` is a mistake
            // in the call, not a fact about the gateway.
            let desired = archive_filename(args.filename.as_deref())?;

            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "zip_attachments only works inside a chat session — there is no \
                     conversation to take files from, and no reply to attach the \
                     archive to"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "zip_attachments is only available inside a chat session — \
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
                    "zip_attachments requires a per-turn attachment-reservation set, \
                     which is only initialised on the chat-page path"
                        .into(),
                )
            })?;

            let members = collect_members(&ctx, session_id, s3, &args.ids).await?;

            let archive = build_zip(&members)
                .map_err(|e| ToolError::Failed(format!("building the archive: {e}")))?;
            let size = archive.len() as u64;

            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &desired)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;

            chat_attachments::upload(s3, turn_id, &filename, "application/zip", archive)
                .await
                .map_err(|e| ToolError::Failed(format!("storing the archive: {e}")))?;

            let marker = session_core::attachments::marker_line(
                &filename,
                "application/zip",
                &chat_attachments::proxy_url(turn_id, &filename),
                size,
            );
            chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;

            let uncompressed: u64 = members.iter().map(|m| m.bytes.len() as u64).sum();
            Ok(json!({
                "filename": filename,
                "id": format!("{turn_id}/{filename}"),
                "mime": "application/zip",
                "size": size,
                "entry_count": members.len(),
                "uncompressed_size": uncompressed,
                "entries": members.iter().map(|m| json!({
                    "name": m.entry,
                    "source_id": m.source_id,
                    "size": m.bytes.len(),
                })).collect::<Vec<_>>(),
                "rendered": "The archive is attached to your reply as a single \
                             download chip — do NOT repeat the marker text, and do \
                             NOT list every entry back in your prose. One sentence \
                             saying what the bundle contains is enough; the user can \
                             see the file names once they unpack it.",
            }))
        })
    }
}

/// Resolve every id to bytes, in the order the model asked for.
///
/// Sequential on purpose. Concurrency here would buy a little latency and cost
/// the two things that make failures debuggable: the *first* bad id is the one
/// reported (not whichever request lost the race), and the running byte total
/// stops the reads as soon as the cap is passed rather than after every GET has
/// already landed in memory.
async fn collect_members(
    ctx: &ToolContext,
    session_id: &str,
    s3: &gateway_core::server::config::S3Config,
    ids: &[String],
) -> Result<Vec<Member>, ToolError> {
    let mut members: Vec<Member> = Vec::with_capacity(ids.len());
    let mut used: HashSet<String> = HashSet::new();
    let mut total: u64 = 0;

    for id in ids {
        let source = file_refs::resolve(&ctx.db, Some(session_id), id, None).await?;
        let (name, bytes) = match &source {
            // A canvas document has no stored object — its content lives in the
            // DB. Same snapshot semantics as `offer_download`: the panel can
            // move on afterwards.
            FileRef::Document { doc, version } => (
                format!("{}.{}", file_refs::slug(&doc.title), doc.format.file_ext()),
                version.content.as_bytes().to_vec(),
            ),
            FileRef::Attachment(a) => {
                let fetched = chat_attachments::fetch(s3, &a.turn_id, &a.filename)
                    .await
                    .map_err(|e| {
                        ToolError::Failed(format!("reading `{}` for the archive: {e}", a.id))
                    })?;
                (a.filename.clone(), fetched.bytes)
            }
            FileRef::UnlistedAttachment { turn_id, filename } => {
                let fetched = chat_attachments::fetch(s3, turn_id, filename)
                    .await
                    .map_err(|e| {
                        ToolError::InvalidArgs(format!(
                            "`{id}` names a turn of this conversation but no such \
                             stored file ({e}) — call `list_attachments` to see what \
                             exists"
                        ))
                    })?;
                (filename.clone(), fetched.bytes)
            }
        };

        total += bytes.len() as u64;
        if total > MAX_TOTAL_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "the files named so far exceed the {} MiB the gateway will bundle in \
                 one archive (reached `{id}`). Bundle fewer files, or hand the \
                 largest ones over individually with `offer_download`.",
                MAX_TOTAL_BYTES / (1024 * 1024)
            )));
        }

        members.push(Member {
            entry: dedupe_entry(&mut used, &name),
            source_id: id.clone(),
            bytes,
        });
    }
    Ok(members)
}

/// Pick a collision-free name for one archive entry.
///
/// Two files from different turns legitimately share a name (`report.md` from
/// Monday and from Tuesday), and a zip with duplicate entries unpacks to
/// whichever the tool happens to write last — silently losing a file the user
/// asked for. Suffix the stem, keeping the extension so the file still opens:
/// `report.md`, `report-2.md`, `report-3.md`.
fn dedupe_entry(used: &mut HashSet<String>, desired: &str) -> String {
    let (stem, ext) = match desired.rsplit_once('.') {
        // A leading-dot name (`.gitignore`) is all stem, no extension.
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (desired, None),
    };
    let mut candidate = desired.to_string();
    let mut n = 1;
    while !used.insert(candidate.clone()) {
        n += 1;
        candidate = match ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{desired}-{n}"),
        };
    }
    candidate
}

/// Validate the requested archive name and make sure it ends in `.zip`.
fn archive_filename(requested: Option<&str>) -> Result<String, ToolError> {
    let name = match requested.map(str::trim) {
        None | Some("") => return Ok(DEFAULT_FILENAME.to_string()),
        Some(name) if name.contains('/') => {
            return Err(ToolError::InvalidArgs(format!(
                "`filename` must not contain `/` (got `{name}`)"
            )));
        }
        Some(name) => name,
    };
    if name.to_ascii_lowercase().ends_with(".zip") {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}.zip"))
    }
}

/// Write the members into an in-memory zip.
///
/// Deflated: these are overwhelmingly text bundles (markdown, JSON, CSV), where
/// compression is the whole point of asking for a zip. The `deflate` feature is
/// the one the workspace already builds `zip` with.
fn build_zip(members: &[Member]) -> Result<Vec<u8>, zip::result::ZipError> {
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for m in members {
            zw.start_file(&m.entry, opts)?;
            zw.write_all(&m.bytes)?;
        }
        zw.finish()?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(ZipAttachments.id(), ZipAttachments.schema().function.name);
    }

    #[test]
    fn archive_name_defaults_and_gains_a_zip_extension() {
        assert_eq!(archive_filename(None).unwrap(), "attachments.zip");
        assert_eq!(archive_filename(Some("  ")).unwrap(), "attachments.zip");
        // A model that names the bundle without an extension still produces a
        // file the OS opens by double-click.
        assert_eq!(
            archive_filename(Some("croit-docs")).unwrap(),
            "croit-docs.zip"
        );
        assert_eq!(
            archive_filename(Some("croit-docs.zip")).unwrap(),
            "croit-docs.zip"
        );
        // Case-insensitive, so `.ZIP` doesn't become `.ZIP.zip`.
        assert_eq!(archive_filename(Some("BUNDLE.ZIP")).unwrap(), "BUNDLE.ZIP");
    }

    #[test]
    fn archive_name_rejects_a_path() {
        let err = archive_filename(Some("../../etc/passwd")).unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains('/'), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn colliding_entry_names_are_suffixed_not_overwritten() {
        // Two turns can both hold a `report.md`. A duplicate zip entry unpacks
        // to whichever was written last — silently dropping a file the user
        // asked for — so the stem gets suffixed and the extension is kept.
        let mut used = HashSet::new();
        assert_eq!(dedupe_entry(&mut used, "report.md"), "report.md");
        assert_eq!(dedupe_entry(&mut used, "report.md"), "report-2.md");
        assert_eq!(dedupe_entry(&mut used, "report.md"), "report-3.md");
        // Extension-less and dotfile names keep their whole name as the stem.
        assert_eq!(dedupe_entry(&mut used, "LICENSE"), "LICENSE");
        assert_eq!(dedupe_entry(&mut used, "LICENSE"), "LICENSE-2");
        assert_eq!(dedupe_entry(&mut used, ".gitignore"), ".gitignore");
        assert_eq!(dedupe_entry(&mut used, ".gitignore"), ".gitignore-2");
    }

    #[test]
    fn the_archive_round_trips_every_member() {
        let members = vec![
            Member {
                entry: "a.txt".into(),
                source_id: "t1/a.txt".into(),
                bytes: b"alpha".to_vec(),
            },
            Member {
                entry: "nested-name.md".into(),
                source_id: "t2/nested-name.md".into(),
                bytes: b"# beta".to_vec(),
            },
            // Empty files are real (a placeholder, a truncated export) and must
            // not be silently dropped from the bundle.
            Member {
                entry: "empty.log".into(),
                source_id: "t3/empty.log".into(),
                bytes: Vec::new(),
            },
        ];
        let bytes = build_zip(&members).unwrap();
        let mut zr = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(zr.len(), 3);
        let mut found = Vec::new();
        for i in 0..zr.len() {
            let mut f = zr.by_index(i).unwrap();
            let mut s = String::new();
            std::io::Read::read_to_string(&mut f, &mut s).unwrap();
            found.push((f.name().to_string(), s));
        }
        assert_eq!(
            found,
            vec![
                ("a.txt".to_string(), "alpha".to_string()),
                ("nested-name.md".to_string(), "# beta".to_string()),
                ("empty.log".to_string(), String::new()),
            ]
        );
    }

    fn ctx(pool: gateway_core::server::db::Pool, session: Option<&str>) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec![],
            pool_access: gateway_core::server::upstreams::PoolAccess::all(),
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

    async fn pool() -> gateway_core::server::db::Pool {
        gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn errors_cleanly_without_a_session() {
        let err = ZipAttachments
            .run(ctx(pool().await, None), json!({"ids": ["t1/a.txt"]}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("chat session"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_s3_not_configured() {
        let err = ZipAttachments
            .run(ctx(pool().await, Some("s1")), json!({"ids": ["t1/a.txt"]}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("not configured"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_id_list_is_rejected_before_the_environment_gates() {
        // No session, no storage, and no ids — the model can only act on the
        // last of those, so that's the error it gets. (It also proves an empty
        // list never reaches the archive builder and yields a 0-entry zip.)
        let err = ZipAttachments
            .run(ctx(pool().await, None), json!({"ids": []}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains("at least one"), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn too_many_ids_is_refused_with_the_cap_in_the_message() {
        let ids: Vec<String> = (0..MAX_ENTRIES + 1)
            .map(|i| format!("t1/f{i}.txt"))
            .collect();
        let err = ZipAttachments
            .run(ctx(pool().await, Some("s1")), json!({"ids": ids}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => {
                assert!(msg.contains(&MAX_ENTRIES.to_string()), "{msg}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_bad_args_shape_is_an_invalid_args_error() {
        let err = ZipAttachments
            .run(ctx(pool().await, Some("s1")), json!({"id": "t1/a.txt"}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains("ids"), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }
}
