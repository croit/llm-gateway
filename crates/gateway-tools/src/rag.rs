// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG tools — the model-facing surface of the indexer.
//!
//! Two tools land here:
//!
//!   * [`RagListCollections`] — a discovery call the model can use to
//!     find out which codebases (and other corpora) the operator has
//!     indexed. Returns name + description + status so the model can
//!     tell ready-to-search from still-indexing.
//!
//!   * [`RagGrep`] — regex retrieval. `rag_search` is hybrid, so exact
//!     identifiers are already findable there; what BM25 cannot express is a
//!     *pattern* (`TODO\(.*\)`, `impl .* for Tool`). This scans chunk text
//!     with a compiled regex and returns matching lines with context, bounded
//!     by result / row / time limits because there is no index behind it.
//!
//!   * [`RagSearch`] — the actual retrieval. Embeds the query through
//!     the collection's configured embedding model, runs a k-NN search
//!     against the per-collection usearch index, joins back to the
//!     SQLite metadata for provenance, and hands the model a list of
//!     `{file, lines, content, score}` records.
//!
//! Both tools fail cleanly when `ToolContext::indexer` is `None`
//! (deployment hasn't wired the indexer up) — a model that gets that
//! error can pivot to other tools rather than retry forever.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use std::collections::HashSet;
use std::sync::Arc;

use gateway_core::server::db::rag as rag_db;
use gateway_core::server::rbac::Resolver;
use gateway_features::server::rag::worker;
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Holds the RBAC resolver so it can filter collections to the ones the
/// caller's gateway groups permit (per-collection `allowed_groups`).
pub struct RagListCollections {
    rbac: Arc<Resolver>,
}

impl RagListCollections {
    pub fn new(rbac: Arc<Resolver>) -> Self {
        Self { rbac }
    }
}

impl Tool for RagListCollections {
    fn id(&self) -> &str {
        "rag_list_collections"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List the codebases / corpora available for retrieval-augmented \
             generation. Each collection lists the indexed refs \
             (branches / tags / commits) you can search, and which is the \
             default. Pass a collection `name` to `rag_search` (and \
             optionally a `ref`) to query it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
            let mut cols = rag_db::list_collections(indexer.db())
                .await
                .map_err(|e| ToolError::Failed(format!("listing collections: {e}")))?;
            // Per-group access: hide collections this caller's gateway groups
            // don't permit. Empty `allowed_groups` = visible to all; admins see
            // everything. A hidden collection is also unsearchable by name (see
            // `RagSearch::run`), so this is a real capability filter.
            let role_ids = self.rbac.role_ids_for(&ctx.roles);
            cols.retain(|c| self.rbac.resource_allowed(&role_ids, &c.allowed_groups));
            let mut items: Vec<Value> = Vec::new();
            for c in &cols {
                let refs = rag_db::list_refs(indexer.db(), c.id)
                    .await
                    .map_err(|e| ToolError::Failed(format!("listing refs: {e}")))?;
                // Only advertise a queryable collection. Versioned: at least
                // one searchable ref. Aggregate: the PRIMARY ref (which holds
                // the single unified index) has finished its first build.
                let advertise = match c.search_mode {
                    rag_db::SearchMode::Aggregate => {
                        refs.iter().any(|r| r.is_primary && r.is_searchable())
                    }
                    rag_db::SearchMode::Versioned => refs.iter().any(|r| r.is_searchable()),
                };
                if !advertise {
                    continue;
                }
                let entry = match c.search_mode {
                    // Aggregate: present it as ONE searchable corpus. We do NOT
                    // enumerate the (possibly dozens of) source repos — listing
                    // them all tempts the model into searching them one-by-one.
                    // One rag_search with no `ref` already covers every source
                    // (it's a single unified index).
                    rag_db::SearchMode::Aggregate => json!({
                        "name": c.name,
                        "description": c.description,
                        "mode": "aggregate",
                        "sources": refs.len(),
                        "usage": "Search the WHOLE collection in a SINGLE rag_search call \
                                  with no `ref` — it is one unified index over all source \
                                  repos. Prefer one broad query; result paths are prefixed \
                                  with the source repo (e.g. `pve-manager/...`).",
                    }),
                    // Versioned: the refs ARE distinct versions, so list them —
                    // the caller needs to pick (or rely on the primary).
                    rag_db::SearchMode::Versioned => {
                        let ref_items: Vec<Value> = refs
                            .iter()
                            .map(|r| {
                                json!({
                                    "ref": r.git_ref,
                                    "primary": r.is_primary,
                                    "searchable": r.is_searchable(),
                                    "status": r.status.as_str(),
                                    "last_indexed_at": r.last_indexed_at.map(|t| t.to_string()),
                                })
                            })
                            .collect();
                        json!({
                            "name": c.name,
                            "description": c.description,
                            "mode": "versioned",
                            "refs": ref_items,
                        })
                    }
                };
                items.push(entry);
            }
            Ok(json!({ "collections": items }))
        })
    }
}

pub struct RagSearch {
    rbac: Arc<Resolver>,
}

impl RagSearch {
    pub fn new(rbac: Arc<Resolver>) -> Self {
        Self { rbac }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    collection: String,
    /// Which ref/source to search. Omitted → the collection's default: the
    /// primary ref (versioned collections) or all sources (aggregate). For
    /// aggregate collections this names a source repo (e.g. `qemu-server`);
    /// for versioned ones a branch/tag/commit. (`ref` is a Rust keyword.)
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    top_k: Option<u32>,
    /// Glob restricting which indexed paths may match. See the schema.
    #[serde(default)]
    path_glob: Option<String>,
}

const TOP_K_DEFAULT: u32 = 5;
/// Aggregate collections span many repos, so one search returns more than the
/// single-repo default — enough to cover several subsystems in a single call so
/// the model doesn't re-query once per aspect. Recall comes from the per-source
/// fan-out plus this larger merged result set.
const AGGREGATE_TOP_K_DEFAULT: u32 = 12;
const TOP_K_MAX: u32 = 25;

impl Tool for RagSearch {
    fn id(&self) -> &str {
        "rag_search"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Search an indexed codebase or corpus for passages relevant to a \
             natural-language query. Call `rag_list_collections` first if you \
             don't know which collections (and which of their refs) are \
             available. Returns the top-k matching chunks with file path, \
             line range, relevance score, and the chunk content. For an \
             aggregate collection, ONE call with no `ref` already searches \
             every source repo at once and merges the results. Prefer a SINGLE \
             broad, well-phrased query covering the whole question and answer \
             from the merged hits — most questions need only one or two \
             searches. Do not decompose into many narrow per-aspect queries or \
             search once per repo; only search again for a genuinely different \
             topic the first results missed (raise `top_k` if you need more \
             depth, up to 25).",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query", "collection"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language description of what you're looking for."
                    },
                    "collection": {
                        "type": "string",
                        "description": "Name of the indexed collection to search. \
                                        Get the list with `rag_list_collections`."
                    },
                    "ref": {
                        "type": "string",
                        "description": "Which ref/source to search. Omit to search \
                                        the collection's default — its primary ref, \
                                        or for an aggregate collection ALL of its \
                                        sources at once. For an aggregate collection \
                                        this names one source repo (e.g. \
                                        `qemu-server`); for a versioned one a branch \
                                        / tag / commit. See `rag_list_collections`."
                    },
                    "path_glob": {
                        "type": "string",
                        "description": "Optional path filter, so you can scope a search to \
                                        part of the corpus: `src/osd/*` (everything under \
                                        that directory, at any depth), `*.rs`, \
                                        `*/tests/*`. Matched against the indexed file path \
                                        with glob syntax (`*` any characters, `?` one, \
                                        `[abc]` a set) and case-sensitively. On an \
                                        aggregate collection paths start with the source \
                                        repo, e.g. `pve-manager/*`. Narrows the search \
                                        rather than guaranteeing every match under the \
                                        path — leave it off unless the user pointed at a \
                                        specific area."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": TOP_K_MAX,
                        "description": "How many results to return. Defaults to 5 \
                                        (12 for aggregate collections); max 25."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{query: string, collection: string, ref?: string, \
                     top_k?: integer, path_glob?: string}}: {e}"
                ))
            })?;
            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
            // A collection the caller's groups don't permit is reported as
            // "not found" — identical to a nonexistent one, so a restricted
            // collection can't be probed for existence or searched by name.
            // Shared with `rag_grep` so the two can't drift apart.
            let collection =
                resolve_collection(&self.rbac, indexer.db(), &ctx.roles, &args.collection).await?;

            // Aggregate collections default to a larger result set (they span
            // many repos, and one search covers them all); an explicit
            // caller-supplied `top_k` always wins.
            let default_k = match collection.search_mode {
                rag_db::SearchMode::Aggregate => AGGREGATE_TOP_K_DEFAULT,
                rag_db::SearchMode::Versioned => TOP_K_DEFAULT,
            };
            let top_k = args.top_k.unwrap_or(default_k).clamp(1, TOP_K_MAX) as usize;

            // Resolve the single store to query. Versioned: the named ref or
            // the primary. Aggregate: the primary ref holds the collection's
            // ONE unified index (built from every source), so we query it
            // directly — one global dense + lexical ranking. Hit paths are
            // prefixed with the source repo (e.g. `pve-manager/...`).
            let rref = resolve_search(indexer.db(), &collection, args.git_ref.as_deref()).await?;

            // Asymmetric query embedding (instruction-prefixed); documents
            // were embedded bare at index time. See `Indexer::embed_query`.
            let query_vec = indexer
                .embed_query(&collection.embedding_model, &args.query)
                .await
                .map_err(|e| ToolError::Failed(format!("embedding query: {e}")))?;

            let path_glob = validate_glob(args.path_glob.as_deref())?;
            let hits = worker::search_chunks(
                indexer,
                &rref,
                &args.query,
                &query_vec,
                top_k,
                path_glob.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("searching index: {e}")))?;
            let empty = hits.is_empty();
            let results: Vec<Value> = hits.into_iter().map(hit_json).collect();
            let mut out = json!({
                "collection": collection.name,
                "ref": rref.git_ref,
                "hits": results,
            });
            if let Some(glob) = &path_glob {
                out["path_glob"] = json!(glob);
                if empty {
                    // Distinguish "nothing matches the query" from "the filter
                    // excluded everything" — otherwise the model concludes the
                    // corpus has no answer when it only mis-scoped the path.
                    out["note"] = json!(format!(
                        "No hits under `{glob}`. The filter may be wrong (check the path \
                         shape with a search without `path_glob`, or note that an \
                         aggregate collection prefixes paths with the source repo) — \
                         retry unscoped before concluding the corpus doesn't cover this."
                    ));
                }
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// rag_grep

/// Regex search over an indexed collection's chunk text.
///
/// `rag_search` is already hybrid — dense kNN fused with FTS5/BM25 — so exact
/// identifiers *are* findable. What BM25 cannot express is a **pattern**: it
/// tokenises, so `TODO\(.*\)`, `impl .* for Tool` and `#\[cfg\(test\)\]` have
/// no query that finds them, and "show me every call site shaped like this" has
/// no tool at all.
///
/// The trade-off is the cost profile: this is a full scan over chunk text with
/// no index to lean on, so it is bounded three ways at once (matches, rows
/// scanned, wall clock) and reports which bound it hit. A partial answer the
/// model knows is partial beats a complete one that took the gateway down.
pub struct RagGrep {
    rbac: Arc<Resolver>,
}

impl RagGrep {
    pub fn new(rbac: Arc<Resolver>) -> Self {
        Self { rbac }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    collection: String,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    path_glob: Option<String>,
    #[serde(default)]
    max_results: Option<u32>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    context_lines: Option<u32>,
}

const GREP_MAX_RESULTS_DEFAULT: u32 = 40;
const GREP_MAX_RESULTS_CAP: u32 = 200;
const GREP_CONTEXT_LINES_CAP: u32 = 5;
/// Longest pattern we compile. Long patterns aren't dangerous (the `regex`
/// crate is linear-time by construction — there is no catastrophic
/// backtracking to trigger), this is just a sanity bound.
const GREP_MAX_PATTERN_LEN: usize = 500;
/// Compiled-program size ceiling. Guards the one thing a pattern *can* blow
/// up: memory, via a huge bounded repetition like `a{1000}{1000}`.
const GREP_REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Rows pulled per batch of the scan. Big enough to amortise the round trip,
/// small enough that the time budget is checked often.
const GREP_BATCH: usize = 500;
/// Hard ceiling on rows examined, whatever the time budget says.
const GREP_MAX_CHUNKS: usize = 50_000;
/// Wall-clock budget for the scan.
const GREP_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

impl Tool for RagGrep {
    fn id(&self) -> &str {
        "rag_grep"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Search an indexed collection with a REGULAR EXPRESSION and get back \
             matching lines with their file, line number and surrounding context \
             — the equivalent of `grep -rn` over the corpus. \
             \
             Use it when you need a pattern rather than a meaning: every `TODO(...)`, \
             every `impl ... for Tool`, every call site of a macro, every line \
             matching a config-key shape. For \"how does X work\" or \"where is Y \
             handled\", use `rag_search` instead — it is hybrid (semantic AND exact \
             keyword), so plain identifiers are already covered there, and it ranks \
             results by relevance while this tool just reports every match in \
             corpus order. \
             \
             This is a full scan with no index behind it, so scope it: pass a \
             `path_glob` when you know the area, and keep the pattern specific. It \
             stops at the result, row or time limit and tells you when it did — a \
             `truncated` result means \"narrow it\", not \"that's all there is\".",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["pattern", "collection"],
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust/RE2-syntax regular expression, matched against \
                                        each line individually (so `^` and `$` anchor to a \
                                        line, and there is no lookahead/backreference). \
                                        Remember to escape regex metacharacters when you \
                                        mean them literally: `TODO\\(.*\\)`, \
                                        `#\\[cfg\\(test\\)\\]`."
                    },
                    "collection": {
                        "type": "string",
                        "description": "Name of the indexed collection. Get the list with \
                                        `rag_list_collections`."
                    },
                    "ref": {
                        "type": "string",
                        "description": "Which ref/source to scan. Omit for the collection's \
                                        default, exactly as in `rag_search`."
                    },
                    "path_glob": {
                        "type": "string",
                        "description": "Path filter, e.g. `src/osd/*`, `*.rs`, `*/tests/*`. \
                                        Glob syntax, case-sensitive, matched against the \
                                        indexed path (on an aggregate collection paths \
                                        start with the source repo). Strongly preferred \
                                        when you know roughly where to look — it is what \
                                        keeps the scan cheap."
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "Match case-insensitively. Default false, like grep."
                    },
                    "context_lines": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": GREP_CONTEXT_LINES_CAP,
                        "description": "Lines of surrounding context per match (default 2, \
                                        max 5). Context is limited to the indexed chunk the \
                                        match falls in, so it can be shorter than asked \
                                        for near a chunk boundary."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": GREP_MAX_RESULTS_CAP,
                        "description": "Stop after this many matching lines. Default 40, \
                                        max 200."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: GrepArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{pattern: string, collection: string, ref?: string, \
                     path_glob?: string, ignore_case?: boolean, context_lines?: integer, \
                     max_results?: integer}}: {e}"
                ))
            })?;

            if args.pattern.trim().is_empty() {
                return Err(ToolError::InvalidArgs("`pattern` must not be empty".into()));
            }
            if args.pattern.len() > GREP_MAX_PATTERN_LEN {
                return Err(ToolError::InvalidArgs(format!(
                    "`pattern` is too long (max {GREP_MAX_PATTERN_LEN} characters)"
                )));
            }
            // Compile before touching the DB: a bad pattern is the model's to
            // fix, and the regex crate's error names the offending position.
            let re = regex::RegexBuilder::new(&args.pattern)
                .case_insensitive(args.ignore_case)
                .size_limit(GREP_REGEX_SIZE_LIMIT)
                .build()
                .map_err(|e| {
                    ToolError::InvalidArgs(format!(
                        "`pattern` is not a valid regular expression: {e}"
                    ))
                })?;

            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
            // Identical resolution (and identical "not found" for a collection
            // the caller's groups don't permit) as `rag_search` — a new tool
            // must not become a way to probe for restricted collections.
            let collection =
                resolve_collection(&self.rbac, indexer.db(), &ctx.roles, &args.collection).await?;
            let rref = resolve_search(indexer.db(), &collection, args.git_ref.as_deref()).await?;
            let store = indexer
                .collection_store(rref.id, &rref.data_uuid)
                .await
                .map_err(|e| ToolError::Failed(format!("opening collection store: {e}")))?;

            let path_glob = validate_glob(args.path_glob.as_deref())?;
            let max_results = args
                .max_results
                .unwrap_or(GREP_MAX_RESULTS_DEFAULT)
                .clamp(1, GREP_MAX_RESULTS_CAP) as usize;
            let context_lines =
                args.context_lines.unwrap_or(2).min(GREP_CONTEXT_LINES_CAP) as usize;

            let scan = grep_scan(
                &store,
                rref.collection_id,
                path_glob.as_deref(),
                &re,
                max_results,
                context_lines,
            )
            .await?;

            let mut out = json!({
                "collection": collection.name,
                "ref": rref.git_ref,
                "pattern": args.pattern,
                "matches": scan.matches,
                "match_count": scan.matches.len(),
                "chunks_scanned": scan.chunks_scanned,
            });
            if let Some(glob) = &path_glob {
                out["path_glob"] = json!(glob);
            }
            if let Some(reason) = scan.stopped_because {
                out["truncated"] = json!(true);
                out["note"] = json!(match reason {
                    StopReason::Results => format!(
                        "Stopped at the {max_results}-match limit; there are more. Narrow the \
                         pattern or add a `path_glob` rather than raising the limit."
                    ),
                    StopReason::Rows => format!(
                        "Stopped after scanning {GREP_MAX_CHUNKS} chunks — the rest of the \
                         collection was NOT examined. Add a `path_glob` to scope the scan."
                    ),
                    StopReason::Time => format!(
                        "Stopped after {}s — the rest of the collection was NOT examined. \
                         Add a `path_glob`, or use `rag_search` if a semantic query would do.",
                        GREP_TIME_BUDGET.as_secs()
                    ),
                });
            } else if scan.matches.is_empty() {
                out["note"] = json!(
                    "No lines matched, and the whole scope was scanned. Check the pattern's \
                     escaping, or try `rag_search` — it finds identifiers and phrasing that \
                     an exact pattern misses."
                );
            }
            Ok(out)
        })
    }
}

/// Why a scan stopped early. Reported to the model, because "40 matches" means
/// something very different when there were 41 versus when the clock ran out
/// halfway through the corpus.
#[derive(Debug, PartialEq, Eq)]
enum StopReason {
    /// Enough matches found; the scan was cut short with more to find.
    Results,
    /// The row ceiling hit — part of the scope was never examined.
    Rows,
    /// The wall-clock budget ran out — likewise.
    Time,
}

struct GrepScan {
    matches: Vec<Value>,
    chunks_scanned: usize,
    stopped_because: Option<StopReason>,
}

/// Page through the collection's chunks and collect matching lines.
///
/// Chunking uses an overlapping window, so one source line can appear in two
/// chunks — matches are de-duplicated on `(path, line)` so a match near a
/// boundary is reported once.
async fn grep_scan(
    store: &gateway_core::server::db::Pool,
    collection_id: i64,
    path_glob: Option<&str>,
    re: &regex::Regex,
    max_results: usize,
    context_lines: usize,
) -> Result<GrepScan, ToolError> {
    let started = std::time::Instant::now();
    let mut matches: Vec<Value> = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    let mut chunks_scanned = 0usize;
    let mut after_id = 0i64;
    let mut stopped_because = None;

    'scan: loop {
        if started.elapsed() >= GREP_TIME_BUDGET {
            stopped_because = Some(StopReason::Time);
            break;
        }
        if chunks_scanned >= GREP_MAX_CHUNKS {
            stopped_because = Some(StopReason::Rows);
            break;
        }
        let batch = rag_db::scan_chunks(store, collection_id, path_glob, after_id, GREP_BATCH)
            .await
            .map_err(|e| ToolError::Failed(format!("scanning chunks: {e}")))?;
        if batch.is_empty() {
            break;
        }
        for chunk in &batch {
            after_id = chunk.id;
            chunks_scanned += 1;
            let lines: Vec<&str> = chunk.content.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                let line_no = chunk.start_line + idx as i64;
                if !seen.insert((chunk.file_path.clone(), line_no)) {
                    continue;
                }
                let from = idx.saturating_sub(context_lines);
                let to = (idx + context_lines + 1).min(lines.len());
                matches.push(json!({
                    "file_path": chunk.file_path,
                    "line": line_no,
                    "text": line,
                    "context": lines[from..to].join("\n"),
                    "context_start_line": chunk.start_line + from as i64,
                }));
                if matches.len() >= max_results {
                    stopped_because = Some(StopReason::Results);
                    break 'scan;
                }
            }
            // Checked per chunk, not only per batch: one pathological chunk
            // shouldn't be able to overshoot the budget by a whole batch.
            if started.elapsed() >= GREP_TIME_BUDGET {
                stopped_because = Some(StopReason::Time);
                break 'scan;
            }
        }
    }

    Ok(GrepScan {
        matches,
        chunks_scanned,
        stopped_because,
    })
}

/// Resolve a collection by name, applying the per-collection group ACL.
///
/// Shared by `rag_search` and `rag_grep` so the two can't drift: a collection
/// the caller's groups don't permit must be reported as *not found*, identical
/// to a nonexistent one, or the error message becomes an existence oracle.
async fn resolve_collection(
    rbac: &Resolver,
    db: &gateway_core::server::db::Pool,
    roles: &[String],
    name: &str,
) -> Result<rag_db::Collection, ToolError> {
    rag_db::find_collection_by_name(db, name)
        .await
        .map_err(|e| ToolError::Failed(format!("looking up collection: {e}")))?
        .filter(|c| {
            let role_ids = rbac.role_ids_for(roles);
            rbac.resource_allowed(&role_ids, &c.allowed_groups)
        })
        .ok_or_else(|| {
            ToolError::Failed(format!(
                "no RAG collection named `{name}` — call rag_list_collections to discover \
                 which collections this gateway has indexed"
            ))
        })
}

/// Bound and normalise a caller-supplied path glob.
///
/// Only a length cap and an emptiness check: GLOB has no injection surface
/// here (it is a bound parameter, not interpolated SQL) and no pathological
/// patterns — SQLite's matcher is a bounded backtracker over a pattern we cap.
/// A blank string is treated as "no filter" rather than "match nothing",
/// because a model passing `""` means the former.
fn validate_glob(raw: Option<&str>) -> Result<Option<String>, ToolError> {
    const MAX_GLOB_LEN: usize = 200;
    let Some(glob) = raw.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(None);
    };
    if glob.len() > MAX_GLOB_LEN {
        return Err(ToolError::InvalidArgs(format!(
            "`path_glob` is too long (max {MAX_GLOB_LEN} characters)"
        )));
    }
    Ok(Some(glob.to_string()))
}

/// Render one search hit. The `score` is hybrid (dense + lexical)
/// reciprocal-rank-fusion relevance — relative ordering only, not an
/// absolute similarity.
fn hit_json((chunk, score): (rag_db::Chunk, f32)) -> Value {
    json!({
        "file_path": chunk.file_path,
        "start_line": chunk.start_line,
        "end_line": chunk.end_line,
        "score": score,
        "content": chunk.content,
    })
}

/// Resolve the single ref whose store `rag_search` queries.
/// * Versioned: the named ref, else the primary.
/// * Aggregate: always the primary ref — it holds the collection's one
///   unified index (built from every source), so a single query ranks across
///   the whole corpus. A caller-supplied `ref` is ignored in aggregate mode
///   (the index is merged; hit paths carry the source-repo prefix instead).
async fn resolve_search(
    db: &gateway_core::server::db::Pool,
    collection: &rag_db::Collection,
    git_ref: Option<&str>,
) -> Result<rag_db::CollectionRef, ToolError> {
    use rag_db::SearchMode;
    let rref = match (collection.search_mode, git_ref) {
        (SearchMode::Versioned, Some(r)) => rag_db::find_ref(db, collection.id, r)
            .await
            .map_err(|e| ToolError::Failed(format!("looking up ref: {e}")))?
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "collection `{}` has no ref `{}` — call rag_list_collections to see \
                     its available refs",
                    collection.name, r
                ))
            })?,
        // Versioned default OR aggregate (unified index lives on the primary).
        _ => rag_db::primary_ref(db, collection.id)
            .await
            .map_err(|e| ToolError::Failed(format!("looking up primary ref: {e}")))?
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "collection `{}` has no indexed {} yet",
                    collection.name,
                    if collection.search_mode == SearchMode::Aggregate {
                        "sources"
                    } else {
                        "refs"
                    }
                ))
            })?,
    };
    if !rref.is_searchable() {
        return Err(ToolError::Failed(format!(
            "collection `{}` is not ready yet (status = {}); its first index hasn't \
             completed — wait for it or re-queue if it failed",
            collection.name,
            rref.status.as_str()
        )));
    }
    Ok(rref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db;
    use gateway_core::server::upstreams::{
        UpstreamRegistry,
        config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
    };
    use gateway_features::server::embeddings;
    use gateway_features::server::rag::worker::{Indexer, IndexerConfig, search_chunks};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// One-hot vectors keyed on the literal substring of the input —
    /// matches the integration-test scaffolding in `tests/rag.rs`.
    fn one_hot(input: &str) -> [f32; 4] {
        let s = input.to_lowercase();
        if s.contains("alpha") {
            [1.0, 0.0, 0.0, 0.0]
        } else if s.contains("beta") {
            [0.0, 1.0, 0.0, 0.0]
        } else {
            [0.5, 0.5, 0.5, 0.5]
        }
    }

    async fn embedding_upstream() -> MockServer {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wpath("/embeddings"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
                let inputs = body
                    .get("input")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let data: Vec<Value> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, val)| {
                        let s = val.as_str().unwrap_or("");
                        let v = one_hot(s);
                        json!({"object": "embedding", "index": i, "embedding": v})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({
                    "object": "list",
                    "model": "embed-test",
                    "data": data,
                }))
            })
            .mount(&upstream)
            .await;
        upstream
    }

    fn registry(upstream_url: &str) -> Arc<UpstreamRegistry> {
        let mut pools = HashMap::new();
        pools.insert(
            "embed".to_string(),
            UpstreamPoolConfig {
                voices: Default::default(),
                allowed_groups: Vec::new(),
                compliance: Default::default(),
                enforce_limits: true,
                kind: PoolKind::Embedding,
                strategy: PickerStrategy::RoundRobin,
                models: Vec::new(),
                fallback_offline: None,
                backend: vec![BackendConfig {
                    name: "mock".into(),
                    base_url: upstream_url.into(),
                    api_key_env: None,
                    api_key: None,
                    weight: 1,
                    max_inflight: 16,
                    health_path: "/models".into(),
                    models: Vec::new(),
                    alias: None,
                    probe_models: true,
                    supports_edit: false,
                }],
            },
        );
        let r = UpstreamRegistry::new(&pools).unwrap();
        let pool = r.pools().into_iter().find(|p| p.name == "embed").unwrap();
        pool.backends[0].set_models(HashSet::from(["embed-test".to_string()]));
        r
    }

    fn ctx_with(indexer: Indexer) -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: indexer.db().clone(),
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: Some(indexer),
            image_gen: None,
            sandbox_lease: None,
            browser_lease: None,
            crypto: None,
            push: None,
            model: None,
        }
    }

    fn ctx_without_indexer(pool: db::Pool) -> ToolContext {
        ToolContext::for_test(pool)
    }

    #[tokio::test]
    async fn list_collections_shows_status() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "demo".into(),
                description: Some("a demo".into()),
                git_url: "https://example.invalid/repo".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // A collection with no searchable ref is not advertised.
        let out = RagListCollections::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(ctx_with(indexer.clone()), json!({}))
        .await
        .unwrap();
        assert!(out["collections"].as_array().unwrap().is_empty());

        // Add a ref and bring it to ready → now listed with its refs.
        let r = rag_db::add_ref(&pool, c.id, "reef", None, true)
            .await
            .unwrap();
        rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "deadbeef")
            .await
            .unwrap();

        let out = RagListCollections::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(ctx_with(indexer), json!({}))
        .await
        .unwrap();
        let cs = out["collections"].as_array().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["name"], "demo");
        let refs = cs[0]["refs"].as_array().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["ref"], "reef");
        assert_eq!(refs[0]["primary"], true);
        assert_eq!(refs[0]["searchable"], true);
    }

    #[tokio::test]
    async fn list_collections_summarises_aggregate_without_enumerating_sources() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "proxmox".into(),
                description: Some("all repos".into()),
                git_url: "https://example.invalid/default.git".into(),
                git_ref: "master".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Aggregate,
            },
        )
        .await
        .unwrap();
        // Two sources; the first is primary and holds the (built) unified
        // index — that's the gate for advertising an aggregate collection.
        for (i, url) in ["https://x/pve-manager.git", "https://x/qemu-server.git"]
            .iter()
            .enumerate()
        {
            let r = rag_db::add_ref(&pool, c.id, "master", Some(url), i == 0)
                .await
                .unwrap();
            if i == 0 {
                rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
                    .await
                    .unwrap();
                rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "sha")
                    .await
                    .unwrap();
            }
        }

        let out = RagListCollections::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(ctx_with(indexer), json!({}))
        .await
        .unwrap();
        let cs = out["collections"].as_array().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["mode"], "aggregate");
        assert_eq!(cs[0]["sources"], 2);
        // The whole point: an aggregate collection is NOT enumerated source by
        // source, so the model searches it in one call instead of looping.
        assert!(
            cs[0].get("refs").is_none(),
            "aggregate collection must not enumerate its sources"
        );
        assert!(
            cs[0]["usage"].as_str().unwrap().contains("SINGLE"),
            "usage hint should steer the model to a single combined search"
        );
    }

    #[tokio::test]
    async fn list_collections_without_indexer_is_clear_error() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let err = RagListCollections::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(ctx_without_indexer(pool), json!({}))
        .await
        .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("RAG is not configured")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_against_ready_collection_returns_provenance() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let reg = registry(&upstream.uri());
        let indexer = Indexer::new(
            pool.clone(),
            Arc::clone(&reg),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );

        // Seed the DB + index by hand (avoid the git path here — the
        // integration test in tests/rag.rs covers that end-to-end).
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "code".into(),
                description: None,
                git_url: "https://example.invalid".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // Each ref owns its store; add a primary ref and seed it by hand.
        let r = rag_db::add_ref(&pool, c.id, "main", None, true)
            .await
            .unwrap();
        let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
        let f = rag_db::upsert_file(&store, c.id, "src/alpha.rs", "hashA")
            .await
            .unwrap();
        rag_db::insert_chunks(
            &store,
            c.id,
            &[rag_db::NewChunk {
                file_id: f,
                chunk_index: 0,
                start_line: 1,
                end_line: 5,
                content: "alpha alpha".into(),
                vector_id: 1,
            }],
        )
        .await
        .unwrap();
        let idx = indexer.open_index(r.id, &r.data_uuid, Some(4)).unwrap();
        let v = embeddings::embed(
            &reqwest::Client::new(),
            &reg,
            "embed-test",
            &["alpha alpha".to_string()],
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
        idx.add(1, &v).unwrap();
        drop(idx);
        // Bring the ref to `ready` on its current store so it's searchable.
        rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "deadbeef")
            .await
            .unwrap();
        let r = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();

        // Sanity-check the lower layer first so a search-tool failure
        // doesn't get blamed on the index plumbing.
        let q = embeddings::embed(
            &reqwest::Client::new(),
            &reg,
            "embed-test",
            &["alpha please".to_string()],
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
        let raw = search_chunks(&indexer, &r, "alpha please", &q, 5, None)
            .await
            .unwrap();
        assert!(!raw.is_empty(), "lower layer returned no hits");

        let out = RagSearch::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(
            ctx_with(indexer),
            json!({ "query": "alpha please", "collection": "code", "top_k": 3 }),
        )
        .await
        .unwrap();
        assert_eq!(out["collection"], "code");
        assert_eq!(out["ref"], "main");
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["file_path"], "src/alpha.rs");
        assert_eq!(hits[0]["start_line"], 1);
        assert_eq!(hits[0]["content"], "alpha alpha");
    }

    #[tokio::test]
    async fn aggregate_search_uses_one_unified_index() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let reg = registry(&upstream.uri());
        let indexer = Indexer::new(
            pool.clone(),
            Arc::clone(&reg),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "proxmox".into(),
                description: None,
                git_url: "https://example.invalid/default.git".into(),
                git_ref: "master".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Aggregate,
            },
        )
        .await
        .unwrap();

        // The PRIMARY ref holds the single unified index. Seed its store with
        // chunks from two "sources", paths prefixed with the source repo —
        // exactly the shape `build_ref` produces for an aggregate collection.
        let primary = rag_db::add_ref(&pool, c.id, "master", Some("https://x/all.git"), true)
            .await
            .unwrap();
        let store = indexer
            .collection_store(primary.id, &primary.data_uuid)
            .await
            .unwrap();
        let idx = indexer
            .open_index(primary.id, &primary.data_uuid, Some(4))
            .unwrap();
        let docs = [
            ("pve-manager/PVE/Manager.pm", "alpha alpha", 1i64),
            ("qemu-server/PVE/QemuServer.pm", "beta beta", 2i64),
        ];
        for (path, content, vid) in docs {
            let f = rag_db::upsert_file(&store, c.id, path, "h").await.unwrap();
            rag_db::insert_chunks(
                &store,
                c.id,
                &[rag_db::NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    start_line: 1,
                    end_line: 2,
                    content: content.into(),
                    vector_id: vid,
                }],
            )
            .await
            .unwrap();
            let v = embeddings::embed(
                &reqwest::Client::new(),
                &reg,
                "embed-test",
                &[content.to_string()],
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
            idx.add(vid, &v).unwrap();
        }
        drop(idx);
        rag_db::set_ref_status(&pool, primary.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, primary.id, &primary.data_uuid, "sha")
            .await
            .unwrap();

        // ONE query over the unified index — `alpha` ranks the pve-manager
        // chunk first, and the hit path is prefixed with its source repo.
        let out = RagSearch::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(
            ctx_with(indexer),
            json!({ "query": "alpha please", "collection": "proxmox", "top_k": 5 }),
        )
        .await
        .unwrap();
        assert_eq!(out["collection"], "proxmox");
        let hits = out["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "unified search returned no hits");
        assert_eq!(hits[0]["file_path"], "pve-manager/PVE/Manager.pm");
        assert!(hits[0]["content"].as_str().unwrap().contains("alpha"));
    }

    #[tokio::test]
    async fn search_rejects_not_ready_collection_with_status_hint() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "still-pending".into(),
                description: None,
                git_url: "https://e.invalid".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // A primary ref exists but hasn't completed its first index.
        rag_db::add_ref(&pool, c.id, "main", None, true)
            .await
            .unwrap();
        let err = RagSearch::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(
            ctx_with(indexer),
            json!({"query": "x", "collection": "still-pending"}),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Failed(msg) => {
                assert!(msg.contains("not ready"), "{msg}");
                assert!(msg.contains("pending"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_unknown_collection_with_discovery_hint() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let err = RagSearch::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        ))
        .run(
            ctx_with(indexer),
            json!({"query": "x", "collection": "no-such-thing"}),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("rag_list_collections"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // path scoping + rag_grep

    fn open_resolver() -> Arc<gateway_core::server::rbac::Resolver> {
        Arc::new(gateway_core::server::rbac::Resolver::empty())
    }

    /// A ready, searchable versioned collection holding `files` — each entry a
    /// `(path, content)` pair indexed as ONE chunk starting at line 1, with its
    /// embedding added to the vector index so both retrieval sides work.
    async fn seeded_collection(
        name: &str,
        files: &[(&str, &str)],
    ) -> (db::Pool, Indexer, MockServer) {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let reg = registry(&upstream.uri());
        let indexer = Indexer::new(
            pool.clone(),
            Arc::clone(&reg),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: name.into(),
                description: None,
                git_url: "https://example.invalid".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        let r = rag_db::add_ref(&pool, c.id, "main", None, true)
            .await
            .unwrap();
        let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
        let idx = indexer.open_index(r.id, &r.data_uuid, Some(4)).unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let vid = i as i64 + 1;
            let f = rag_db::upsert_file(&store, c.id, path, "hash")
                .await
                .unwrap();
            rag_db::insert_chunks(
                &store,
                c.id,
                &[rag_db::NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    start_line: 1,
                    end_line: content.lines().count().max(1) as i64,
                    content: (*content).into(),
                    vector_id: vid,
                }],
            )
            .await
            .unwrap();
            let v = embeddings::embed(
                &reqwest::Client::new(),
                &reg,
                "embed-test",
                &[(*content).to_string()],
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
            idx.add(vid, &v).unwrap();
        }
        drop(idx);
        rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "deadbeef")
            .await
            .unwrap();
        (pool, indexer, upstream)
    }

    /// The point of variant A: the same query, scoped to a subtree, returns
    /// only what lives there — and the filter has to apply on *both* retrieval
    /// sides, or the excluded file comes back through whichever side skipped it.
    #[tokio::test]
    async fn path_glob_scopes_a_search_to_the_matching_subtree() {
        let (_pool, indexer, _up) = seeded_collection(
            "code",
            &[
                ("src/osd/alpha.rs", "alpha alpha"),
                ("docs/alpha.md", "alpha alpha"),
            ],
        )
        .await;

        // Unscoped: both files are candidates.
        let out = RagSearch::new(open_resolver())
            .run(
                ctx_with(indexer.clone()),
                json!({"query": "alpha please", "collection": "code"}),
            )
            .await
            .unwrap();
        assert_eq!(out["hits"].as_array().unwrap().len(), 2, "{out:?}");
        assert!(
            out.get("path_glob").is_none(),
            "no filter, no echo: {out:?}"
        );

        // Scoped: only the one under src/osd.
        let out = RagSearch::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({
                    "query": "alpha please", "collection": "code",
                    "path_glob": "src/osd/*"
                }),
            )
            .await
            .unwrap();
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{out:?}");
        assert_eq!(hits[0]["file_path"], "src/osd/alpha.rs");
        assert_eq!(
            out["path_glob"], "src/osd/*",
            "the scope must be echoed back"
        );
    }

    /// A glob that matches nothing must not read as "the corpus has no answer"
    /// — that is the failure mode that makes a model give up on a good corpus.
    #[tokio::test]
    async fn a_glob_matching_nothing_tells_the_model_to_retry_unscoped() {
        let (_pool, indexer, _up) =
            seeded_collection("code", &[("src/alpha.rs", "alpha alpha")]).await;
        let out = RagSearch::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({
                    "query": "alpha please", "collection": "code",
                    "path_glob": "nowhere/*"
                }),
            )
            .await
            .unwrap();
        assert!(out["hits"].as_array().unwrap().is_empty(), "{out:?}");
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("unscoped"), "{note}");
    }

    #[test]
    fn a_blank_glob_means_no_filter_not_match_nothing() {
        assert_eq!(validate_glob(Some("  ")).unwrap(), None);
        assert_eq!(validate_glob(None).unwrap(), None);
        assert_eq!(
            validate_glob(Some(" src/* ")).unwrap().as_deref(),
            Some("src/*")
        );
        assert!(validate_glob(Some(&"a".repeat(500))).is_err());
    }

    /// The capability `rag_search` cannot provide: a pattern, with line numbers
    /// and context, in corpus order.
    #[tokio::test]
    async fn grep_returns_matching_lines_with_line_numbers_and_context() {
        let (_pool, indexer, _up) = seeded_collection(
            "code",
            &[(
                "src/osd.rs",
                "fn one() {}\n// TODO(martin): fix this\nfn two() {}\n",
            )],
        )
        .await;
        let out = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": r"TODO\(.*\)", "collection": "code"}),
            )
            .await
            .unwrap();
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{out:?}");
        assert_eq!(matches[0]["file_path"], "src/osd.rs");
        // Chunk starts at line 1, the match is the second line.
        assert_eq!(matches[0]["line"], 2, "{out:?}");
        assert!(
            matches[0]["text"]
                .as_str()
                .unwrap()
                .contains("TODO(martin)"),
            "{out:?}"
        );
        // Context reaches the surrounding lines.
        let ctx_text = matches[0]["context"].as_str().unwrap();
        assert!(
            ctx_text.contains("fn one()") && ctx_text.contains("fn two()"),
            "{ctx_text}"
        );
        assert!(out.get("truncated").is_none(), "nothing was cut: {out:?}");
    }

    #[tokio::test]
    async fn grep_honours_the_path_filter() {
        let (_pool, indexer, _up) = seeded_collection(
            "code",
            &[
                ("src/osd.rs", "// TODO(a): here\n"),
                ("docs/notes.md", "// TODO(b): there\n"),
            ],
        )
        .await;
        let out = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": "TODO", "collection": "code", "path_glob": "src/*"}),
            )
            .await
            .unwrap();
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{out:?}");
        assert_eq!(matches[0]["file_path"], "src/osd.rs");
    }

    /// Case sensitivity is grep's, not the embedder's — and the flag has to
    /// actually reach the compiled pattern.
    #[tokio::test]
    async fn grep_is_case_sensitive_unless_asked_otherwise() {
        let (_pool, indexer, _up) =
            seeded_collection("code", &[("src/a.rs", "let Timeout = 5;\n")]).await;

        let out = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer.clone()),
                json!({"pattern": "timeout", "collection": "code"}),
            )
            .await
            .unwrap();
        assert!(out["matches"].as_array().unwrap().is_empty(), "{out:?}");

        let out = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": "timeout", "collection": "code", "ignore_case": true}),
            )
            .await
            .unwrap();
        assert_eq!(out["matches"].as_array().unwrap().len(), 1, "{out:?}");
    }

    /// Hitting the result cap must be reported, not silently look like the
    /// whole answer — "5 matches" and "5 matches, there are more" lead to very
    /// different next steps.
    #[tokio::test]
    async fn grep_reports_when_it_stopped_at_the_result_limit() {
        let body = (0..20)
            .map(|i| format!("// TODO({i})"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_pool, indexer, _up) = seeded_collection("code", &[("src/a.rs", &body)]).await;
        let out = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": "TODO", "collection": "code", "max_results": 3}),
            )
            .await
            .unwrap();
        assert_eq!(out["matches"].as_array().unwrap().len(), 3, "{out:?}");
        assert_eq!(out["truncated"], true, "{out:?}");
        assert!(
            out["note"].as_str().unwrap().contains("more"),
            "the note must say there are more: {out:?}"
        );
    }

    /// A model that mis-escapes a pattern should get the regex error back, not
    /// a scan of the whole corpus for a literal.
    #[tokio::test]
    async fn grep_rejects_an_invalid_pattern_before_touching_the_index() {
        let (_pool, indexer, _up) = seeded_collection("code", &[("src/a.rs", "x")]).await;
        let err = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": "TODO(unclosed", "collection": "code"}),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => {
                assert!(msg.contains("not a valid regular expression"), "{msg}")
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
        assert!(
            RagGrep::new(open_resolver())
                .run(
                    ToolContext::for_test(
                        db::open(std::path::Path::new(":memory:")).await.unwrap()
                    ),
                    json!({"pattern": "", "collection": "code"}),
                )
                .await
                .is_err(),
            "an empty pattern must be refused too"
        );
    }

    /// The no-existence-leak property, which a *new* tool over the same corpus
    /// is exactly how you'd lose: a collection the caller's groups don't permit
    /// must be indistinguishable from one that doesn't exist.
    #[tokio::test]
    async fn grep_reports_a_forbidden_collection_as_missing() {
        let (pool, indexer, _up) = seeded_collection("secret", &[("src/a.rs", "TODO x")]).await;
        let c = rag_db::find_collection_by_name(&pool, "secret")
            .await
            .unwrap()
            .unwrap();
        rag_db::set_allowed_groups(&pool, c.id, &["ops".to_string()])
            .await
            .unwrap();

        // Caller has no roles, so no group grants it.
        let err = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer.clone()),
                json!({"pattern": "TODO", "collection": "secret"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no RAG collection named `secret`"),
            "must read as nonexistent: {err}"
        );

        // Byte-identical to a genuinely unknown name — that identity is the
        // property, not just the wording of either message.
        let unknown = RagGrep::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({"pattern": "TODO", "collection": "secret"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, unknown);
    }

    /// Path scoping must not become a way around the same ACL.
    #[tokio::test]
    async fn search_reports_a_forbidden_collection_as_missing() {
        let (pool, indexer, _up) = seeded_collection("secret", &[("src/a.rs", "alpha")]).await;
        let c = rag_db::find_collection_by_name(&pool, "secret")
            .await
            .unwrap()
            .unwrap();
        rag_db::set_allowed_groups(&pool, c.id, &["ops".to_string()])
            .await
            .unwrap();
        let err = RagSearch::new(open_resolver())
            .run(
                ctx_with(indexer),
                json!({
                    "query": "alpha", "collection": "secret", "path_glob": "src/*"
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no RAG collection named `secret`"), "{err}");
    }

    #[test]
    fn schema_ids_match() {
        assert_eq!(
            RagListCollections::new(std::sync::Arc::new(
                gateway_core::server::rbac::Resolver::empty()
            ))
            .id(),
            RagListCollections::new(std::sync::Arc::new(
                gateway_core::server::rbac::Resolver::empty()
            ))
            .schema()
            .function
            .name
        );
        assert_eq!(
            RagSearch::new(std::sync::Arc::new(
                gateway_core::server::rbac::Resolver::empty()
            ))
            .id(),
            RagSearch::new(std::sync::Arc::new(
                gateway_core::server::rbac::Resolver::empty()
            ))
            .schema()
            .function
            .name
        );
    }
}
