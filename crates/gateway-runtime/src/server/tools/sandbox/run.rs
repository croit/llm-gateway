// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

pub struct RunInSandbox(pub Arc<SandboxClient>);

impl Tool for RunInSandbox {
    fn id(&self) -> &str {
        "run_in_sandbox"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        // Whether this deployment's runner can grant egress decides both the
        // wording and whether `network` exists as an option at all. Advertising
        // a `network` flag a runner will reject buys nothing: the model spends a
        // round discovering it, and cannot tell "misconfigured" from "wrong
        // tool for the job".
        let egress = self.0.egress_available();
        let mut properties = json!({
            "language": {
                "type": "string", "enum": ["python", "bash"],
                "description": "Interpreter for `code`."
            },
            "code": {
                "type": "string",
                "description": "The program to run. Any file you write under the \
                                working directory is returned to the user \
                                automatically, subdirectories included — a file \
                                written to `docs/backend.md` comes back as an \
                                attachment named `docs-backend.md` (the delivered \
                                name is flattened; its `sandbox_path` says where it \
                                sat). Check the `artifacts` list in the result: if a \
                                file you wrote is not in it, it was NOT delivered, \
                                and you must not tell the user it was."
            },
            "files": {
                "type": "array",
                "description": "Optional UTF-8 text files to place in the working \
                                directory before running.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "content"],
                    "properties": {
                        "name": {"type": "string"},
                        "content": {"type": "string"}
                    }
                }
            },
            "attachments": {
                "type": "array",
                "description": "Optional chat attachments to fetch into the working \
                                directory (binary-safe — use this for uploaded \
                                .pptx/.xlsx/.pdf/images/zip you want to process). \
                                The current turn's uploads are staged automatically; \
                                list ids here only to pull in files from EARLIER in \
                                the conversation (see `available_attachments` in a \
                                prior result).",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": {
                        "id": {"type": "string", "description": "Any file of this \
                               conversation: an attachment id `<turn>/<file>`, just \
                               the filename (newest match wins), or a canvas \
                               `document_id` / document title — a document named here \
                               is materialised into the working directory exactly as \
                               if it were listed under `documents`."},
                        "name": {"type": "string", "description": "Optional filename \
                                 to give the file in the working directory."}
                    }
                }
            },
            "documents": {
                "type": "array",
                "description": "Canvas documents (from `create_document`) to \
                                materialize into the working directory — resolved \
                                server-side, so use this instead of pasting large \
                                content into `files`.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["document_id"],
                    "properties": {
                        "document_id": {"type": "string", "description": "Id from \
                                        `create_document`."},
                        "version": {"type": "integer", "description": "Specific \
                                    version (default: latest)."},
                        "name": {"type": "string", "description": "Filename in the \
                                 working directory (default: `<title>.<ext>`)."}
                    }
                }
            },
            "fresh": {
                "type": "boolean",
                "description": "Discard the current sandbox and start from a clean \
                                one (drops anything earlier calls this turn wrote to \
                                the working directory). Default false. Use it to \
                                reset state."
            }
        });
        if egress && let Some(props) = properties.as_object_mut() {
            props.insert(
                "network".into(),
                json!({
                    "type": "boolean",
                    "description": "Request network egress for this run (web access, \
                                    NOT for installing packages — nothing can be \
                                    installed). Default false. Fixed when the sandbox \
                                    starts, so to change it mid-turn also set \
                                    `fresh: true`."
                }),
            );
        }
        ToolDef::function(
            self.id(),
            run_in_sandbox_desc(egress),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["language", "code"],
                "properties": properties,
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: RunArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{language, code, files?, attachments?, documents?, network?}}: {e}"
                ))
            })?;
            if args.code.trim().is_empty() {
                return Err(ToolError::InvalidArgs("code must be non-empty".into()));
            }
            // `network` isn't in the schema when the runner has no egress, but a
            // model working from a stale tool list can still send it. Refuse
            // here rather than spending a container on the runner's 400.
            if args.network && !self.0.egress_available() {
                return Err(ToolError::InvalidArgs(
                    "this gateway's sandbox has no network egress configured, so \
                     `network` cannot be granted. Solve the task offline with the \
                     preinstalled libraries and the files in the working directory."
                        .into(),
                ));
            }
            // Stage the round's uploads + any named attachments first, then
            // canvas documents, then the model's inline text files (so an
            // explicit text file wins over a same-named staged file).
            let Staged {
                files: staged_files,
                staged,
                available,
                mut notes,
                documents: attachment_documents,
            } = stage_attachments(&ctx, &args.attachments).await?;
            let mut files = staged_files;
            // A canvas document is materialised the same way whether it was
            // named in `documents` or in `attachments` (the resolver sorted
            // that out), so both lists go through one call.
            let mut wanted_documents = args.documents;
            wanted_documents.extend(attachment_documents);
            let staged_documents =
                stage_documents(&ctx, &wanted_documents, &mut files, &mut notes).await;
            files.extend(args.files.into_iter().map(TextFile::into_input));
            let req = RunRequest {
                language: args.language,
                code: args.code,
                files,
                timeout_secs: None,
                network: args.network,
                container_id: None,
                keep_alive: false,
            };
            // When the turn carries a lease (chat + proxy paths with the
            // sandbox configured), route through it so `/work` and scratch
            // state persist across this turn's `run_in_sandbox` calls; the
            // lease sets `container_id` / `keep_alive`. Without a lease (tests,
            // sandbox-less paths) fall back to a single-use call. Either way
            // the result is shaped the same.
            let mut out = match &ctx.sandbox_lease {
                Some(lease) => {
                    let run = lease.run_tracked(req, args.fresh).await?;
                    let reset = run.workdir_reset;
                    let mut out = self.0.shape_response(&ctx, run.resp).await?;
                    if reset && let Some(obj) = out.as_object_mut() {
                        obj.insert(
                            "workdir_reset".into(),
                            json!(
                                "The working directory did NOT carry over from your previous \
                                 run_in_sandbox call in this turn — this job started in a fresh, \
                                 empty /work (the runner could not keep the earlier container). \
                                 Files earlier calls wrote are gone. Recreate what you need in \
                                 ONE call rather than assuming it is still there, and note that \
                                 files already returned in an earlier call's `artifacts` are \
                                 saved to the conversation and can be re-staged by `id`."
                            ),
                        );
                    }
                    out
                }
                None => self.0.execute(&ctx, req).await?,
            };
            augment_with_staging(&mut out, staged, available, notes);
            if !staged_documents.is_empty()
                && let Some(obj) = out.as_object_mut()
            {
                obj.insert("staged_documents".into(), json!(staged_documents));
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: generate_document (markdown -> pdf/docx/pptx via pandoc)
