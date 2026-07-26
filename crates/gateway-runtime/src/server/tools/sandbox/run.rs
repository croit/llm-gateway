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
        ToolDef::function(
            self.id(),
            RUN_IN_SANDBOX_DESC,
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["language", "code"],
                "properties": {
                    "language": {
                        "type": "string", "enum": ["python", "bash"],
                        "description": "Interpreter for `code`."
                    },
                    "code": {
                        "type": "string",
                        "description": "The program to run. Write output files to the \
                                        current working directory to return them."
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
                                "id": {"type": "string", "description": "Attachment id \
                                       `<turn>/<file>` from an attachment stub, or just the \
                                       filename of a file from earlier in this conversation \
                                       (newest match wins)."},
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
                    "network": {
                        "type": "boolean",
                        "description": "Request network egress for this run (web access, \
                                        NOT for installing packages). Default false; only \
                                        honored if the operator configured an egress \
                                        allowlist. Fixed when the sandbox starts — to \
                                        change it mid-turn, also set `fresh: true`."
                    },
                    "fresh": {
                        "type": "boolean",
                        "description": "Discard the current sandbox and start from a clean \
                                        one (drops anything earlier calls this turn wrote to \
                                        the working directory). Default false. Use it to \
                                        reset state, or to change `network`."
                    }
                }
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
            // Stage the round's uploads + any named attachments first, then
            // canvas documents, then the model's inline text files (so an
            // explicit text file wins over a same-named staged file).
            let Staged {
                files: staged_files,
                staged,
                available,
                mut notes,
            } = stage_attachments(&ctx, &args.attachments).await?;
            let mut files = staged_files;
            let staged_documents =
                stage_documents(&ctx, &args.documents, &mut files, &mut notes).await;
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
                    let resp = lease.run(req, args.fresh).await?;
                    self.0.shape_response(&ctx, resp).await?
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
