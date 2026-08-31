# Remote document sources for RAG — implementation plan

**Status: all four phases are implemented**, except ACL-faithful per-user filtering and provider delta feeds — see the end of [`fileshare-rag.md`](fileshare-rag.md). Both of the questions this feature exists to answer now work end to end. This document is the
design agreement for indexing a customer's file host into the gateway's
existing RAG subsystem. It started as "index a Nextcloud" and was widened, on
purpose, to *any* file host: Nextcloud, ownCloud, OpenCloud, OneDrive/SharePoint,
Dropbox, plain WebDAV. Once phase 3 lands it collapses into an operator-facing
`docs/remote-rag-sources.md` and this file goes away.

**Built so far** (`cargo fmt` / `clippy -D warnings` / full suite green):

| Piece | Where |
| --- | --- |
| `FileProvider` trait, capability model, provider registry, config-field descriptors | `gateway-features/src/server/rag/source/mod.rs` |
| WebDAV provider (Nextcloud / ownCloud / OpenCloud / generic), PROPFIND parsing, extension detection | `…/source/webdav.rs` |
| Provider-agnostic concurrent tree walker with subtree pruning, cycle and size bounds | `…/source/tree.rs` |
| `source_kind` / `source_config_json` / sealed `source_secrets`, per-ref `dir_versions_json` + `delta_cursor` | migration `0058_rag_remote_sources.sql`, `db/rag.rs` |
| Worker branch: enumerate → fetch → chunk → embed, sharing the whole indexing path with git | `…/rag/worker.rs` (`gather_remote`, `index_items`, `read_item`) |
| Admin surface: source picker + credential form rendered from each provider's declared fields, secret sealing, **Test connection** | `gateway-web/src/pages/rag_source.rs`, `pages/rag.rs`, `POST /rag/test-source` |
| JSON API: `source_kind` + `source_config` on create and PATCH, `GET /api/v0/rag/providers` for field discovery | `gateway/src/rama_server/rag_api.rs` |
| **Extraction ladder**: text → PDF text layer → OCR → office, with page-accurate provenance | `…/rag/extract.rs`, `…/rag/chunk.rs`, migration-free store DDL change (`loc_kind`/`loc_from`/`loc_to`) |
| Office reading shared with `fetch_attachment` — one python extractor, two consumers | `gateway-runtime/…/sandbox/office.rs` |
| **Document profiles**: operator-defined extraction schema, seeded `invoice` + `project_document` | migration `0059_rag_document_profiles.sql`, `db/rag_documents.rs` |
| **Extraction pass**: one LLM call per document → normalised fields + summary, cached by content hash | `…/rag/profile.rs`, `rag_extractions` |
| **Structured query layer**: filter / sort / aggregate over extracted fields, with `total_matches` and ambiguity reporting | `db/rag_documents.rs` |
| **Contextual chunk headers** — document identity prepended to each chunk *before embedding* | `…/rag/worker.rs` |
| `rag_query_documents`, `rag_list_documents`, `rag_fetch_document` | `gateway-tools/src/rag_documents.rs` |
| Profile picker on `/rag`, `GET /api/v0/rag/profiles`, `profile` on create/PATCH | `pages/rag.rs`, `rag_api.rs` |
| Links back to the original file on every hit | `rag_files.web_url`, `FileProvider::web_url` |
| ~130 tests, including the customer's question answered end to end through WebDAV → OCR → extraction → query | `tests/it/rag_profile.rs`, `tests/it/rag_extract.rs`, `tests/it/rag_webdav.rs`, `tests/it/rag_api.rs`, and the module unit tests |

**Usable end to end.** An operator creates a WebDAV collection on `/rag`,
picks an extraction profile, tests the connection, and:

- *"When did we last get an invoice from ACME, how much, and what are the
  details?"* → `rag_query_documents` filters by vendor, sorts by date, returns
  the right invoice with its number, total and currency, a link to the original,
  and the total number of matches so the model cannot mistake a page for the
  whole set. Pinned by `tests/it/rag_profile.rs`.
- *"Find all documentation of project X and summarise it"* → `rag_list_documents`
  returns every document under a folder with the summary written at index time,
  ~200 tokens each, so a whole folder costs one call instead of re-reading every
  file. The deck then comes from the existing `typst_presentation` tooling.

**Not built yet:** incremental in-place sync (§9), so each build is still a full
re-fetch — though never a re-OCR and never a re-extraction, both being cached by
content hash. Phase 4's reranker and ACL-faithful filtering are also open.

**The architecture that came out of this lives in
[`fileshare-rag.md`](fileshare-rag.md)** — read that first if you want to know
how the thing works. This file is the design record: what was decided, why,
and what was deliberately left out.

Companion reading: the [RAG section of the README](../README.md#rag-codebase-search)
for what exists today, [`ocr.md`](ocr.md) for the extraction backend,
[`architecture.md`](architecture.md#crate-boundaries) for the layering rules this
plan has to respect.

> This file deliberately exceeds the ~400-line guideline in
> [`docs/README.md`](README.md#editing-rules). It is one topic — a four-phase
> feature agreed before implementation — and splitting it would separate the
> schema from the reasoning that produced it. It shrinks to a normal subsystem
> doc when the work lands.

---

## 1. Scope

Two questions from the customer define done:

1. **"When did we last get an invoice from company X, how much, and what are the
   details?"** — over thousands of small PDFs, many of them scans, in mixed
   German/English.
2. **"Find all documentation of project X, summarise it, then produce a 50-page
   deck for a technical audience."**

**In scope:** read-only indexing of shared Nextcloud folders; extraction of text
from scans, images and Office files; retrieval that can answer questions about
*sets* of documents, not just passages; provenance back to the original file.

**Out of scope, explicitly:**

- Writing to Nextcloud. The gateway reads; it never modifies the customer's files.
- Payment reconciliation. The archive answers *what invoices we received*. Whether
  and when an invoice was paid lives in an ERP/banking system and is a separate
  data source (reachable later through an MCP connector).
- Indexing users' personal home folders. Nextcloud does not permit an
  admin/service account to read them (§7); the corpus is the shared corpus.
- Sub-minute freshness. Nightly or hourly sync in phases 1–3; event-driven is
  phase 4.

---

## 2. What already exists

The gateway is roughly 80% of the way there. Nothing in this table gets rebuilt.

| Capability | Where | Relevance |
|---|---|---|
| Hybrid retrieval (dense kNN ⊕ FTS5/BM25 via RRF) | `gateway-features/src/server/rag/worker.rs::search_chunks` | Exact identifiers (invoice numbers, project codes) survive alongside paraphrase. |
| Per-collection store (`rag.sqlite` + `index.usearch`) | `db/mod.rs::open_collection_store`, `rag/index.rs` | Heavy, regenerable state already lives off the backup-critical DB. |
| Multi-source collections | migration `0017_rag_multi_source.sql` | A collection already aggregates several sources into one unified index. A Nextcloud folder set is just another source shape. |
| Vector delete | `rag/index.rs::remove` (implemented + tested) | Makes incremental sync possible instead of rebuild-only. |
| Zero-downtime index swap | `rag/worker.rs::index_ref_inner` | A long rebuild never takes search offline. |
| OCR with a content-hash cache | `gateway-features/src/server/ocr.rs`, migration `0054_ocr_derivatives.sql` | Keyed by `doc_sha256` — **a full re-index never re-OCRs a file it has already read.** This is the single biggest cost saver in the whole plan. |
| Scan detection without word lists | `ocr.rs::pdf_needs_ocr` | Character-count based, so it behaves identically for German and English. Born-digital PDFs never touch the GPU. |
| PDF text layer, per page | `gateway-features/src/server/pdf.rs::extract_text_pages` | Tier 1 of the extraction ladder, in-process, no sandbox. |
| Office extraction (docx/pptx/xlsx → structured JSON) | `gateway-tools/src/fetch_attachment.rs::extract_office` | Tier 3. Runs in the sandbox; see §6 for the layering problem this creates. |
| Per-collection group ACL | migration `0046_rag_allowed_groups.sql`, `rbac::Resolver::resource_allowed` | Enforced on *both* list and search, so a hidden collection cannot be reached by naming it. |
| Index log + status timeline | migration `0026_rag_index_log.sql`, `/rag` page | A multi-hour ingest is already observable. |
| Deck + document production | `typst_presentation`, `generate_document`, canvas documents, Web Push on turn completion | Use case 2's output half needs no new code. |

---

## 3. The four structural gaps

In pipeline order. The first three are plumbing; the fourth decides whether the
feature answers the customer's questions at all.

### 3.1 The only source is git

`build_ref` clones a repo and walks the working tree. Everything about a source —
URL shape, auth, change detection, the notion of a "commit" — is git-specific.

### 3.2 Non-UTF-8 files are silently dropped — **fixed**

`rag/worker.rs`, in the per-file loop:

```rust
let content = match std::fs::read(&file.abs_path) {
    Ok(bytes) => match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => continue, // binary — skip
    },
    Err(_) => continue,
};
```

Every PDF, scan and Office file in the corpus vanished here, with no log line
and no counter. For a code corpus that is correct. For this one it was the
whole corpus.

Replaced by `extract::DocumentExtractor` (§5.1). What did *not* change is worth
noting: a file nothing can read is still skipped — but it is now counted,
grouped by reason, and reported on the ref's timeline. "Ready" next to 3000
unreadable scans was the most expensive silence in this system.

### 3.3 Every build is a full rebuild

`index_ref_inner` always builds into a fresh `data_uuid` folder and swaps. Perfect
for a repo you re-clone in 30 seconds; wrong for a corpus whose first pass costs
hours of GPU and whose daily delta is a handful of files.

### 3.4 The only question shape is "find me passages" — **fixed**

`rag_search` returns the top *k* chunks. Neither customer question is a top-*k*
question:

- **"When did we *last*…"** is a superlative over a filtered set. Answering it
  correctly requires *every* invoice from that vendor, ordered by date. Five
  semantically similar chunks drawn from three thousand near-identical invoice
  layouts is a coin flip — and worse, the model cannot tell that it only saw five.
  Embeddings actively work against us here: invoices are near-duplicates of each
  other, so the dense side has almost no signal to separate them. BM25 on the
  vendor name helps with *which* invoices; it does nothing for *ordering* them.
- **"Find *all* documentation of project X and summarise it"** is exhaustive and
  corpus-wide. Top-*k* returns a biased sample, and feeding forty full documents
  into the context window is neither affordable nor better than feeding forty good
  summaries.

**The fix for both is the same: extract structure at index time.** See §5.4
and §8; both shipped, and the invoice question is pinned end to end by
`tests/it/rag_profile.rs`.

Three details that only became clear building it:

- **`total_matches` is not optional.** Every result carries the full match
  count alongside the returned page, because the failure mode is not a wrong
  answer, it is a confident one: a model handed 10 of 47 invoices will say "we
  received 10" unless told otherwise.
- **Documents missing the sort key sort last, in both directions.** A document
  whose date the extractor could not find is not "the oldest" — it is unknown,
  and surfacing it at the top of a *most recent* answer would be actively
  misleading.
- **Ambiguity is surfaced, not resolved.** When a text filter matches more than
  one distinct value (`ACME` hitting both `ACME GmbH` and `ACME Deutschland
  AG`), the result says so and the model is told to ask. Entity resolution is
  genuinely hard, and the alternative — a legal-suffix list — is a
  language-specific word list, which this product does not do.

---

## 4. Source abstraction

### 4.1 Model — **implemented**

An earlier draft of this plan proposed enum dispatch over `{Git, Webdav}` on
KISS grounds. That was right for two sources and wrong for the actual
requirement, which is that OneDrive, Dropbox and S3 must slot in without
reopening the indexer. The shipped design is a trait object plus a registry —
the same shape `sandbox-runner` already uses for `dyn ContainerBackend`:

```rust
#[async_trait::async_trait]
pub trait FileProvider: Send + Sync + 'static {
    fn kind(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn root(&self) -> DirRef;
    async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError>;
    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<Vec<u8>, ProviderError>;
    fn web_url(&self, entry: &RemoteEntry) -> Option<String> { None }
    async fn probe(&self) -> Result<ProbeReport, ProviderError>;
    async fn delta(&self, cursor: Option<&str>) -> Result<DeltaPage, ProviderError> { /* unsupported */ }
}
```

Four decisions carry the extensibility, and each is load-bearing rather than
decorative:

1. **Capabilities, not product names.** `ProviderCapabilities { subtree_pruning,
   delta, stable_ids, web_links }` is how the worker decides what it may do.
   `worker.rs` contains no mention of WebDAV, and `grep -c webdav worker.rs`
   returning zero is the property worth keeping. `Default` is the pessimistic
   set, so a provider that opts into nothing still works — just with a full
   walk, path identity, and no links.

2. **Identity is `RemoteEntry::id`, not the path.** Every serious host has a
   stable per-file id that survives a move (`oc:fileid`, a Graph `driveItem`
   id, a Dropbox `id:` handle). Keying on it turns a moved folder of 400
   documents into a path update instead of a re-extraction. Providers without
   one report `stable_ids: false` and fall back to the path.

3. **`RemoteEntry::version` is opaque.** An etag, a ctag, a Dropbox `rev`, a
   content hash — compared for equality, never parsed. That is what lets one
   walker serve hosts with completely different change models.

4. **Providers describe their own settings.** `ProviderFactory::config_fields()`
   returns `&[ConfigField]` (key, label, help, `FieldKind::{Text,Secret,Url,Bool}`,
   required, default). The admin form renders from that, validation runs from
   that, and `with_defaults` means a declared default is written in exactly one
   place. **This is the difference between an extensible design and one that
   merely contains a trait**: adding Dropbox must not require editing
   `pages/rag.rs`.

Adding a provider is therefore: one module, one `register` call in
`ProviderRegistry::with_builtins`. Nothing in the worker, chunker, store,
tools, or admin page changes.

The shared vocabulary (all in `source/mod.rs`):

```rust
pub struct RemoteEntry {
    pub id: String,          // stable across rename/move where supported
    pub locator: String,     // provider-native; never shown to the model
    pub rel_path: String,    // the provenance the model and user see
    pub kind: EntryKind,     // File | Dir
    pub version: String,     // opaque change token
    pub size_bytes: u64,
    pub mime: Option<String>,
    pub modified_at: Option<Timestamp>,
}

pub enum DirListing {
    /// The directory's version matches what we stored: nothing beneath it
    /// changed, so the whole subtree is skipped without listing it.
    Unchanged,
    Listed { entries: Vec<RemoteEntry>, version: Option<String> },
}
```

The walker (`source/tree.rs`) is breadth-first, lists each level concurrently,
and returns a `TreeSnapshot`. Two of its fields exist purely to stop a
plausible data-loss bug: `failed` (directories that errored) and `truncated`
(a bound was hit) both make `is_complete()` false, and **only a complete walk
may drive deletions or update stored directory versions**. A folder that
returned 503 is otherwise indistinguishable from a folder that was emptied,
and treating it as the latter silently deletes a live subtree from the index.

`rag_collections` gains `source_kind TEXT NOT NULL DEFAULT 'git'`. `build_ref`
branches once on it; git keeps its existing clone path verbatim, because a
clone has already materialised the tree on disk and re-fetching would be
waste. Below `Indexer::read_item` the two are identical.

### 4.2 WebDAV client

New module `gateway-features/src/server/rag/webdav.rs`. Plain `reqwest` +
`quick-xml`; no WebDAV crate (see §11).

**Enumeration.** `PROPFIND` with `Depth: 1`, recursing directory by directory.
`Depth: infinity` is commonly disabled on Nextcloud instances and is not worth
depending on. Request body:

```xml
<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <oc:fileid/>
    <d:getetag/>
    <d:getlastmodified/>
    <d:getcontentlength/>
    <d:getcontenttype/>
    <d:resourcetype/>
    <oc:permissions/>
  </d:prop>
</d:propfind>
```

Base path: `{base_url}/remote.php/dav/files/{account}/{root}`. Everything shared
with the service account — including Team/Group folders — appears inside that
tree, which is why it is preferred over the separate groupfolders endpoint.
*(Verify against the customer's instance during phase 1; if their Team folders do
not surface there, the `/remote.php/dav/groupfolders/{account}` endpoint is the
fallback and the code should treat the root path as configuration, not a
constant.)*

**The pruning trick that makes this cheap.** Nextcloud propagates content changes
up the tree — *"ETag of a directory changes if a file or file metadata somewhere
underneath the directory changes […] every change of a file somewhere in the tree
propagates up to the root directory and changes the ETag of every parent
directory"* ([ownCloud client wiki][etags]). So the walker compares each
directory's etag against the stored one and, on a match, **skips the entire
subtree without descending**. A re-sync of an unchanged corpus costs one PROPFIND
at the root of each unchanged branch and zero downloads. This is exactly the
optimisation the desktop sync clients use ([sync algorithm][syncalg]), and it is
the difference between a nightly sync that finishes in seconds and one that walks
3000 files every night.

**Identity.** Items are keyed by `oc:fileid`, not path: *"File IDs never change
for the lifetime of a file"* ([ownCloud client wiki][etags]). A renamed or moved
file keeps its fileid, so the sync updates one path column instead of deleting and
re-embedding — which for a moved folder of 400 invoices is the difference between
a no-op and a re-OCR of the lot.

**Deletion.** Anything in the store whose fileid was not seen in a completed full
walk is deleted (chunks, vectors, document row). Guarded: a walk that errored
part-way must not be treated as authoritative, or a transient 503 deletes half the
corpus. Only a walk that completed every branch may drive deletions.

**Auth.** Nextcloud app password over HTTPS Basic. Stored **sealed** with
`server::crypto` under `GATEWAY_ENCRYPTION_KEY`, unlike the existing git `pat`
column (plaintext, a decision made when the only secret was a repo token). A
Nextcloud app password grants read access to a company's entire shared document
store; it does not belong in the clear.

**Bounding.** Two new semaphores on the indexer, alongside `clone_sem`:
`fetch_sem` (concurrent GETs — do not hammer the customer's Nextcloud) and
`extract_sem` (concurrent OCR/sandbox work — do not starve chat off the GPU).

---

## 5. Extraction and structure

### 5.1 The ladder — **implemented**

New module `gateway-features/src/server/rag/extract.rs`. Dispatch on MIME +
extension, never on content sniffing:

| Class | Path | Notes |
|---|---|---|
| Text-ish (`.md`, `.txt`, `.csv`, `.json`, code) | UTF-8 decode | Unchanged from today. |
| PDF | `pdf::extract_text_pages` → `ocr::pdf_needs_ocr` → `OcrService::recognize` on a scan | Born-digital PDFs never reach the GPU. The threshold is already configured as `auto_min_text_chars_per_page`. |
| Image | `OcrService::recognize` | |
| Office | sandbox extractor (§6) | Falls back to skipping with a logged reason if the sandbox is unconfigured. |
| Everything else | skipped, **counted and logged** | Unlike today's silent `continue`. The `/rag` timeline gets a per-build "skipped N files (unsupported type)" line. |

Output:

```rust
pub struct ExtractedDoc {
    pub pages: Vec<String>,      // one entry per page; single-entry for flat text
    pub extractor: Extractor,    // Text | PdfTextLayer | Ocr | Office
    pub pages_total: Option<usize>,
    pub pages_processed: Option<usize>,
    pub truncated: bool,
}
```

`pages_total` vs `pages_processed` propagates from the OCR outcome into the
document row, so a document the OCR page limit truncated is *visibly* partial
rather than quietly wrong.

**Usage accounting.** `OcrService::recognize` takes a `UsageMeta { user_id,
source }`. Indexing has no user. Add `UsageSource::Indexer` (`"indexer"`) and
attribute rows to a synthetic user id. `usage_events.source` is plain `TEXT` with
no CHECK constraint (migration `0022_usage.sql`), so this is a code-only change —
but the usage dashboards need to not break on the new value.

### 5.2 Chunking with real provenance — **implemented**

Today a chunk carries `start_line`/`end_line`. For a scanned PDF, lines are
meaningless and pages are what a human can verify against. Per AGENTS.md rule 7
(no backwards-compat shims), generalise rather than add a parallel pair:

```
rag_chunks.loc_kind TEXT NOT NULL   -- 'line' | 'page'
rag_chunks.loc_from INTEGER NOT NULL
rag_chunks.loc_to   INTEGER NOT NULL
```

Per-collection stores are regenerable and every build currently creates a fresh
folder, so this cost one reindex and no migration of user data.

Two decisions came out of building it:

- **Chunking runs per page** for paginated documents, so a chunk never straddles
  a page boundary. A chunk spanning pages 3–4 cannot be cited precisely, and an
  imprecise citation is worse than a coarse one: the user opens page 3, does not
  find the sentence, and stops trusting the answer.
- **The tools emit one `location` string** (`lines 12-30`, `page 4`) rather than
  a `start_line`/`end_line` pair. The unit now differs by document, and handing
  a model two integers with no unit invites it to quote a page number as a line.
  `rag_grep` composes the two (`page 4, line 7`) rather than adding a line
  offset to a page number, which would produce a confident, wrong citation.

### 5.3 Contextual chunk headers

Before embedding — not before storing — each chunk gets a one-line header derived
from its document's extracted metadata:

```
[Invoice · ACME GmbH · 2025-11-04 · /Finance/Invoices/2025 · p.2]
<chunk text>
```

The stored `content` stays raw, so `rag_grep` and the rendered hit are clean; only
the embedding input carries the header. This costs a handful of tokens per chunk
and is the single cheapest large win available: a bare paragraph from page 2 of an
invoice is embedding-identical to the same paragraph in 400 other invoices, and
the header is what separates them in vector space. (It also means the profile pass
must run *before* embedding — see the pipeline order in §5.4.)

### 5.4 Document profiles — the load-bearing piece — **implemented**

A **profile** is an operator-defined extraction schema attached to a collection. It
turns each document into a row of normalised fields plus a short summary, both
written at index time by a cheap chat model.

Central DB, migration `0058_rag_document_profiles.sql`:

```sql
CREATE TABLE rag_document_profiles (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT,
    -- Instruction text prepended to the extraction call. Operator-editable.
    prompt       TEXT NOT NULL,
    -- JSON array of field definitions:
    --   { key, type: text|number|date|enum, description,
    --     filterable: bool, sortable: bool, enum_values?: [..] }
    fields_json  TEXT NOT NULL,
    -- Bumped by the operator on any semantic edit; part of the cache key so a
    -- changed prompt re-extracts instead of serving stale fields.
    version      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
) STRICT;

ALTER TABLE rag_collections ADD COLUMN source_kind      TEXT NOT NULL DEFAULT 'git';
ALTER TABLE rag_collections ADD COLUMN profile_id       INTEGER;   -- NULL = no extraction
ALTER TABLE rag_collections ADD COLUMN extraction_model TEXT;
```

Two profiles ship as seeded examples — `invoice` and `project_document` — because
the two use cases want genuinely different fields, and hardcoding invoice columns
would make use case 2 worse rather than better.

Per-collection store gains:

```sql
CREATE TABLE IF NOT EXISTS rag_documents (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id        INTEGER NOT NULL,
    remote_id      TEXT,          -- oc:fileid
    title          TEXT,
    doc_type       TEXT,
    summary        TEXT,
    language       TEXT,
    pages_total    INTEGER,
    pages_processed INTEGER,
    extractor      TEXT NOT NULL,
    extracted_at   TEXT,
    FOREIGN KEY (file_id) REFERENCES rag_files(id) ON DELETE CASCADE
) STRICT;

-- Entity-attribute-value rather than a wide table, because the field set is
-- per-profile. Three typed columns so ordering and range filters use an index
-- instead of SQLite's text collation.
CREATE TABLE IF NOT EXISTS rag_doc_fields (
    doc_id      INTEGER NOT NULL,
    key         TEXT NOT NULL,
    value_text  TEXT,
    value_num   REAL,
    value_date  TEXT,          -- ISO-8601, so lexical order is chronological
    PRIMARY KEY (doc_id, key),
    FOREIGN KEY (doc_id) REFERENCES rag_documents(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_doc_fields_text ON rag_doc_fields (key, value_text);
CREATE INDEX IF NOT EXISTS idx_doc_fields_num  ON rag_doc_fields (key, value_num);
CREATE INDEX IF NOT EXISTS idx_doc_fields_date ON rag_doc_fields (key, value_date);

-- Fuzzy vendor/title matching without a language-specific normaliser.
CREATE VIRTUAL TABLE IF NOT EXISTS rag_doc_fields_fts USING fts5(
    value_text, content='rag_doc_fields', content_rowid='rowid',
    tokenize='unicode61'
);
```

**Normalisation is the extractor's job, in the prompt, not in Rust.** Dates come
back ISO-8601, amounts as a decimal string plus an ISO-4217 currency code. That is
how `31.12.2025` and `1.234,56 €` and `12/31/2025` and `$1,234.56` all land in one
comparable column without a single language-specific rule in the codebase — which
matters, because this corpus is German and English and the product must not care.

**Extraction caching**, mirroring `ocr_derivatives` exactly (migration
`0059_rag_extractions.sql`, central DB): key `(doc_sha256, profile_id,
profile_version, model)`, value the extracted JSON. A failed row is kept for the
operator but reads as a miss, so a transient backend failure retries instead of
poisoning the document forever. Consequence worth stating: a full corpus rebuild
re-embeds but re-runs **neither** OCR nor field extraction.

**Input budget.** Long documents are truncated head + tail before the extraction
call — invoice totals live at the bottom of the page and a head-only truncation
loses exactly the field that matters most.

### 5.5 Pipeline order

```
PROPFIND walk → etag diff → fetch bytes → extract text (cached by sha256)
    → profile pass: fields + summary (cached by sha256+profile version)
    → chunk per page → prepend context header → embed → usearch + FTS5
```

The profile pass sits *before* chunking because §5.3's headers depend on it.

---

## 6. The layering problem — **resolved as planned**

`gateway-features` sits **below** `gateway-runtime`, and `SandboxClient` lives in
`gateway-runtime/src/server/tools/sandbox/mod.rs`. The indexer therefore cannot
call the sandbox for Office extraction without violating AGENTS.md's "never
reference upward" rule, which is load-bearing for build times.

**Recommendation: invert the dependency.** Declare the capability in
`gateway-features` and implement it in `gateway-runtime`, injected at boot from
`gateway/src/main.rs`. Use the manual boxed-future shape the codebase already uses
for `ToolFuture` rather than adding `async-trait` to `gateway-features`:

```rust
// gateway-features/src/server/rag/extract.rs
pub trait OfficeExtractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        ext: &'a str,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ExtractError>> + Send + 'a>>;
}
```

`Indexer` holds an `Option<Arc<dyn OfficeExtractor>>`; `None` means Office files
are skipped with a logged reason, exactly as OCR degrades when no `ocr` pool
exists. `gateway-runtime` gets a thin impl wrapping `SandboxClient` and reusing
`fetch_attachment.rs`'s `EXTRACT_PY` — which should move to a shared const so the
extractor script exists in exactly one place (DRY).

Rejected alternative: moving `SandboxClient` down into `gateway-features`. It is
reachable — the type depends only on `SandboxConfig`, `reqwest` and
`shared::sandbox` — but it is used by a dozen tools and moving it churns the layer
that churns least.

**What building it actually required.** The python extractor lived in
`gateway-tools/src/fetch_attachment.rs`, which sits *above* `gateway-runtime`,
so the office implementation could not reach it. Rather than copy the script,
it moved down to `gateway-runtime/…/sandbox/office.rs` and both consumers now
share it: `fetch_attachment` takes the structured JSON plus re-attached images,
the indexer takes flattened text. Two copies of office-parsing logic would have
drifted, and the copy used for indexing would have been the one nobody noticed
had rotted.

**One more shared-instance trap, found while wiring boot.** `OcrService` carries
the gateway-wide concurrency gate, and that gate only bounds anything if every
caller holds the *same* instance. The indexer needs one before `RamaState`
exists, so `main.rs` now builds it once and hands it to both
(`RamaState::with_ocr`). Letting each construct its own would have quietly
allowed twice the configured number of concurrent OCR calls against one GPU.

---

## 7. Access control

The sharpest risk in the feature is a payroll or HR PDF surfacing in the wrong
person's chat.

**Hard constraint from Nextcloud's side:** there is no WebDAV impersonation. An
admin account cannot read another user's home folder over DAV. What a service
account can read is what has actually been shared with it, plus Team/Group
folders. The indexable corpus *is* the shared corpus, and that is not a limitation
to work around — it happens to be exactly where an invoice archive and project
documentation already live.

| Model | Mechanism | Verdict |
|---|---|---|
| **Service account + group ACL** | Index what the service account can see; gate each collection with the existing `allowed_groups`, already enforced on both `rag_list_collections` and `rag_search`. | **Phases 1–3.** Simple, one index, and the granularity matches how the folders are actually organised. |
| **ACL-faithful filtering** | Record per file which Nextcloud principals may read it; filter at query time by the caller's Nextcloud identity. | Phase 4. |
| **Per-user index** | Each user connects their own Nextcloud, as the MCP connectors do for Gmail. | Rejected for a shared archive: N indexes, N× embedding cost. Reasonable only as live unindexed access to personal folders. |

**Design for phase 4 now, even though it ships later:** add `acl_json TEXT` to
`rag_files` in `COLLECTION_STORE_DDL` from the first phase and populate it from
`oc:permissions` plus the OCS shares API, even while nothing reads it.
Retrofitting an ACL column means re-walking and re-indexing the entire corpus;
carrying an unused column costs nothing.

Two implementation notes for when it does ship:

- The filter must be applied **before** reciprocal-rank fusion truncates to *k*,
  not after — otherwise a user with narrow access gets a thin result set for
  reasons that look like bad retrieval. `search_chunks` already solves exactly
  this shape for `path_glob` (it widens the dense candidate pool when a filter is
  present); reuse that mechanism rather than inventing a second one.
- Mapping gateway user → Nextcloud user is free if both sit behind the same OIDC
  issuer (match on `sub`), and an operational chore otherwise (`users.nextcloud_uid`
  plus an admin UI to maintain it). This is a question for the customer, not a
  code decision.

**Untrusted content.** Indexed documents are input, not instruction. A PDF that
says "ignore your previous instructions" must be wrapped and labelled the way OCR
output already is in the chat path. This matters more here than for a code
corpus: nobody mails you a hostile git repo, but anyone can mail you a hostile
invoice.

---

## 8. Retrieval surface

| Tool | Shape | State |
|---|---|---|
| `rag_search` | unchanged, plus page-based `loc_*`, document title/type, and a Nextcloud link per hit | extend |
| `rag_list_collections` | unchanged | — |
| `rag_query_documents` | filter · sort · aggregate over profile fields | new |
| `rag_list_documents` | folder-scoped document listing with stored summaries | new |
| `rag_fetch_document` | full extracted text of one document, paged | new |
| `nextcloud_get_file` | attach the original file to the current reply (chat-only) | new |

### `rag_query_documents`

**Not free-form SQL.** A constrained query object, validated against the
collection's profile field list, so an invalid field is a clear error naming the
available fields rather than a SQL error or a silent empty result:

```jsonc
{
  "collection": "invoices",
  "filters": [
    { "field": "vendor",   "op": "matches", "value": "ACME" },
    { "field": "doc_date", "op": "gte",     "value": "2025-01-01" }
  ],
  "order_by": "doc_date",
  "direction": "desc",
  "limit": 10,
  "aggregate": { "op": "sum", "field": "total_gross" }   // optional
}
```

The response leads with the **distinct values it matched** for each `matches`
filter, before the rows:

```jsonc
{
  "matched_values": { "vendor": ["ACME GmbH", "ACME Deutschland AG"] },
  "total_count": 47,
  "documents": [ /* … */ ]
}
```

That shape exists for one reason: entity resolution is genuinely hard and we are
not going to solve it with a legal-suffix list (which would be a
language-specific word list, which this product does not do). "Deutsche Telekom
AG" and "Telekom Deutschland GmbH" are one vendor to a human and two strings to
SQLite. Surfacing the matched set lets the model notice the ambiguity and — when
it actually matters — resolve it with the existing `ask_user` tool instead of
silently answering about one of them. `total_count` alongside a truncated
`documents` list is the other half: it stops the model concluding "we received 10
invoices" when it was handed the first 10 of 47.

## 8a. The admin surface — **implemented**

`/rag`'s create and edit forms carry a **source kind** picker built from
`ProviderRegistry::factories()`, and below it a field set rendered from the
chosen provider's `config_fields()`. Every provider's inputs are present in the
DOM at once, namespaced `src_<kind>_<key>`, and gated on one datastar signal —
so switching source kind is instant, and only the selected kind's fields are
read on submit.

`pages/rag_source.rs` names no provider. That is the property worth keeping: a
page that matched on `"webdav"` to decide which inputs to draw would put the
extensibility back where it started, and adding Dropbox would mean editing the
admin UI. `git` is the single deliberate special case — it is not a
`FileProvider` (a clone materialises the tree on disk) so it has no factory to
enumerate, and its `git_url` / `git_ref` / `pat` inputs are gated on the same
signal.

**Secret handling.** `FieldKind::Secret` renders as a password input, is sealed
with `Crypto::seal_str` into `source_secrets_ct`/`_nonce`, and is never
rendered back. A stored secret shows an empty input badged "stored"; leaving it
blank keeps what is stored, and an explicit **Clear** checkbox is the only way
to remove one. Both behaviours are pinned by tests, because the failure mode —
an edit form silently wiping a credential because the operator did not retype
it — is the classic way admin forms lose secrets.

**Test connection** posts the live form to `POST /rag/test-source`, which
builds the provider and calls `FileProvider::probe()`. It reports the account,
the number of entries under the configured root, and whether the ownCloud
extensions were detected — which is what tells the operator whether this
collection gets move-proof identity and cheap re-syncs. An edit form sends its
`collection_id` too, so testing an existing source does not require retyping
the password. Worth having before committing to a multi-hour first index
against a mistyped folder path.

**The JSON API** takes `source_kind` plus a flat `source_config` map on both
`POST` and `PATCH /api/v0/rag/collections`; the server splits secret from
non-secret using the provider's own field declarations and seals the secret
half, so a caller never has to know which keys are sensitive. Responses carry
`source_kind`, the non-secret `source_config`, and `source_secrets_set` — never
a credential. `GET /api/v0/rag/providers` returns the same field descriptors
the UI renders from, so a script can drive a provider it has no compiled-in
knowledge of. `source_kind` and `source_config` must be sent together on PATCH:
a settings map has no meaning without the kind whose schema defines it.

Changing a collection's source re-queues its refs, since everything indexed
under the old source is no longer what the collection points at.

---

### `rag_list_documents`

`{ collection, folder?, query?, since?, until?, limit }` → documents with title,
date, path, Nextcloud link and the **stored** summary. This is what makes use
case 2 affordable: forty summaries at ~200 tokens each is 8k tokens and one tool
call, versus forty PDFs the model cannot read in one turn. Full text is pulled
with `rag_fetch_document` only for the handful that carry the technical detail
the deck actually needs.

---

## 9. Incremental sync

Phases 1–2 keep the existing full-rebuild-and-swap. Phase 2 introduces the
alternative for WebDAV sources, because a nightly full rebuild of an OCR'd corpus
is not a thing anyone will run twice.

An incremental build mutates the ref's **live** store folder rather than building
a fresh one, which forfeits the atomic swap. Three things replace it:

1. **Per-file transactionality.** One file's delete-old-chunks + insert-new-chunks
   is one SQLite transaction; the matching `index.remove` / `index.add` calls
   bracket it. A crash leaves at most one file inconsistent.
2. **A sync cursor.** New `rag_sync_state` in the per-collection store (last full
   walk, last completed item, walk-complete flag). `index.save()` and a cursor
   write every N files, so a restart resumes rather than restarting a five-hour
   ingest. This matters most on the *first* pass, which is the one that takes
   hours.
3. **Store schema versioning.** `COLLECTION_STORE_DDL` is a bag of
   `CREATE TABLE IF NOT EXISTS` with no version marker — fine today because every
   build starts from an empty folder. Once folders persist across builds, adding a
   column needs a real step: set `PRAGMA user_version` in the DDL and run a small
   ordered migration list on open. Without this, the first schema change silently
   does nothing on existing stores.

**Compaction.** usearch `remove` leaves tombstones; a corpus with heavy churn
degrades. Track deleted-vector ratio in `rag_sync_state` and trigger a full
rebuild (via the existing fresh-folder-and-swap path) past a threshold. Full
rebuild stays cheap because OCR and extraction are both cached.

---

## 10. Cost and sizing

Numbers to validate in phase 1 against the customer's real corpus, not to quote at
them:

- **Pages.** Thousands of invoices at 1–2 pages ≈ 5–10k pages, of which only the
  scans reach the GPU.
- **First ingest.** At a few seconds per page on one OCR stream, the first pass is
  a multi-hour job. Design consequences, all already covered: resumable (§9),
  observable (existing index log), bounded (`extract_sem`), and ideally scheduled
  off-hours since it competes with chat and image generation for the same GPUs.
- **Field extraction.** One cheap chat call per document. Thousands of them, cached
  by content hash.
- **Storage.** ~16 KB of vector per chunk at 4096 dimensions. 50k chunks ≈ 800 MB
  on `[rag] data_dir`, which is already the cheap-disk mount.
- **Re-sync.** Unchanged corpus: one PROPFIND per unchanged branch, zero
  downloads, zero GPU.

New config:

```toml
[rag]
data_dir          = "/mnt/data/gateway-rag"
clone_concurrency = 4
fetch_concurrency = 8    # concurrent WebDAV GETs
extract_concurrency = 2  # concurrent OCR / sandbox extractions

[rag.extraction]
enabled         = true
model           = "qwen3-30b"   # cheap chat model for the profile pass
max_input_chars = 24000         # head+tail truncation budget per document
```

Per AGENTS.md rule 8, `README.md` gets these in the same commit as the code.

---

## 11. Dependencies

One addition, per the `docs/dependencies.md` process:

| Crate | Used in | Why |
|---|---|---|
| `quick-xml` | `gateway-features` | Parsing WebDAV `PROPFIND` multistatus responses. Already in the tree transitively (via `rust-s3`/`aws-creds`), so this does not grow the dependency graph — it makes an existing one direct. Pull-parser only; no serde-xml layer. |
| `async-trait` | `gateway-features` | Object safety for `Arc<dyn FileProvider>`: the indexer dispatches over pluggable sources and tests swap in an in-memory fake, exactly as `sandbox-runner` does for `dyn ContainerBackend`. Already a workspace dependency used by `session-core`, `gateway-runtime` and `sandbox-runner`. |

One API note worth recording, because it cost a debugging cycle and would
silently corrupt data otherwise: **quick-xml 0.40 does not unescape inside
text events.** An entity arrives as its own `Event::GeneralRef`, so a
`getetag` of `&quot;abc&quot;` is three events. Reading only the first
truncates every quoted etag on servers that escape them — which would make
change detection compare a truncated token against a full one and re-index
the whole corpus on every pass. The parser therefore accumulates character
data across events and applies it when the element closes.

Deliberately **not** added: a WebDAV client crate. What we need is PROPFIND +
GET against one server implementation whose quirks we care about (the folder-etag
propagation in §4.2 is a Nextcloud property, not a WebDAV one). A general client
would abstract away exactly the thing the design depends on.

---

## 12. Test plan

TDD, Chicago style, per AGENTS.md rule 3 — real collaborators, in-memory SQLite,
`wiremock` for the Nextcloud/OCR/embedding upstreams. Red before green.

**Phase 1**
- `webdav::parse_multistatus` — a captured Nextcloud response yields fileid, etag,
  content type, size; a directory is distinguished from a file by `resourcetype`.
- Namespace handling: `d:`/`D:`/`oc:` prefixes and default-namespace forms all parse.
- Walker prunes: a directory whose etag matches the stored one is not descended
  into (assert on the wiremock request count, which is the observable behaviour).
- A partial walk (one branch 503s) produces no deletions.
- Auth failure surfaces through `friendly_error` as an actionable message, not a
  raw 401.

**Phase 2**
- Extraction ladder: born-digital PDF never calls the OCR mock; a thin-text-layer
  PDF does; an image does; an unsupported type is counted and logged, not silent.
- A moved file (same fileid, new path) updates the path and re-embeds nothing.
- A deleted file removes its chunks *and* its vector ids from the index.
- Resume: kill a build after N files, restart, assert it resumes at the cursor
  rather than re-fetching.
- Page provenance survives into a hit's `loc_kind: "page"`.

**Phase 3**
- Field extraction normalises `31.12.2025`, `2025-12-31` and `12/31/2025` to one
  ISO date, and `1.234,56 €` / `$1,234.56` to a decimal + currency code.
- The extraction cache: second index pass over identical bytes makes zero chat
  calls; bumping `profile_version` makes them again.
- `rag_query_documents` rejects an unknown field with a message naming the
  available ones.
- Superlative end to end: seed three invoices from one vendor across three dates,
  ask for the latest, assert the right document comes back — the regression test
  that pins the whole point of the feature.
- `total_count` reflects the unlimited match count, not `len(documents)`.
- `matched_values` surfaces two vendor spellings when both match.
- ACL: a collection restricted to a group is invisible *and* unsearchable by name
  to a user outside it (extend the existing `allowed_groups` tests to the new
  tools — every new tool needs the same gate, and this is exactly the kind of
  wiring bug a per-tool test catches).

**Routes.** Every new route goes in the README endpoints table or
`tests/readme_routes.rs` fails.

---

## 13. Phases

| Phase | Ships | Exit criteria |
|---|---|---|
| **1 — spike** ✅ *built; unvalidated against a live server* | Provider abstraction + registry, WebDAV provider, generic walker, `source_kind` + sealed credentials, worker wiring, admin form + Test connection, JSON API, text files only, full rebuild each pass | Search returns real hits from the customer's Team folder. Auth, tree shape, folder-etag propagation and rough corpus size confirmed against the live instance rather than assumed. **Only remaining blocker: a real server to point at.** |
| **2 — ingest** ✅ *extraction done; sync work deferred* | Extraction ladder (text → PDF text layer → OCR → office via §6), page provenance, per-page chunking, skip/partial reporting on the timeline | The whole corpus is searchable. **Still to do in this phase:** incremental etag sync + resume cursor, store schema versioning, `nextcloud_get_file`, links back to the original on each hit. |
| **3 — structure** ✅ | Document profiles (two seeded), field + summary extraction with its own content-hash cache, `rag_query_documents`, `rag_list_documents`, `rag_fetch_document`, contextual chunk headers, profile picker on `/rag` and in the API | **Both customer questions answer correctly, with provenance.** The superlative regression test is green. Still open here: a profile *editor* (profiles are seeded and DB-editable, but there is no UI to author one). |
| **4 — scale** ✅ *mostly* | Incremental in-place sync with subtree pruning, cross-encoder reranking (`PoolKind::Rerank`), sync-hook freshness (`POST /hooks/rag/{token}`), store schema versioning | Precision holds on near-duplicate corpora; an unchanged corpus re-syncs for the cost of a walk; a changed folder is searchable in minutes. **Open:** ACL-faithful per-user filtering, provider delta feeds. |

Phases 1–3 are the product. Phase 4 is what the second customer will ask for.

---

## 14. Risks

- **OCR quality is the ceiling on everything downstream.** A German invoice whose
  table layout OCRs badly produces a wrong total stated with total confidence.
  Sample and measure on the real corpus in phase 1 before anyone promises
  accuracy. Consider carrying an extraction-confidence field so the model can
  hedge rather than invent — and prefer "here is the number and here is the source
  PDF" over a bare number in every answer template.
- **A changed extraction prompt invalidates thousands of cached rows.** Version it
  deliberately (`profile_version`) and make the `/rag` UI say how many documents a
  version bump will re-extract before the operator clicks it.
- **Duplicates.** Archives are full of the same invoice scanned twice under two
  names. Content-hash dedup at ingest is nearly free and prevents "we were invoiced
  twice" answers.
- **Shared GPU.** A full ingest competes with chat, OCR and image generation.
  Bounded concurrency plus an off-hours window.
- **Nextcloud's own Context Chat exists** and the customer will ask about it. It is
  Nextcloud-side, wants its own GPU and its own vector store. The trade we are
  making: giving that up in exchange for structured queries over extracted fields,
  the deck and document tooling, gateway group ACLs, and one system to operate
  instead of two. Worth being able to say out loud.

---

## 15. Open questions for the customer

1. **Which folders, under which account?** Team folders shared with a service user
   is the assumption. If they expect personal home folders indexed, the honest
   answer is that Nextcloud does not allow it without per-user credentials, and
   that changes the architecture (§7).
2. **One identity provider or two?** Determines whether phase 4's ACL filtering is
   free or a mapping table to maintain.
3. **How fresh must the index be?** Nightly is a cron line. Minutes means wiring
   Nextcloud webhook listeners or `notify_push` into a queue. Both are reasonable;
   they are not the same amount of work.
4. **What is the accuracy bar for extracted invoice figures?** "Roughly right, with
   a link to the source PDF" is phase 3. "Correct enough to book against" needs a
   validation pass and a human in the loop, and should be scoped separately.

---

## 16. External references

Claims about Nextcloud's behaviour in this document are load-bearing for the
design, so they are cited rather than assumed. Re-verify against the customer's
actual server version in phase 1.

[etags]: https://github.com/owncloud/client/wiki/Etags-and-file-ids
[syncalg]: https://github.com/nextcloud/gsoc_client/blob/master/doc/dev/sync-algorithm.md

- **ETag propagation + stable file ids** — [ownCloud client wiki: Etags and file ids][etags]. **Since verified live** against Nextcloud 31 by `mise run test-nextcloud`; the citation is now corroboration rather than the only evidence.
  Directory ETags change when anything beneath them changes and propagate to the
  root; file ids never change for the lifetime of a file and are not recycled.
  Both underpin §4.2.
- **Discovery walks only changed subtrees** — [Nextcloud sync algorithm][syncalg].
  Confirms the pruning strategy is the intended use of the ETag propagation, not a
  trick we invented.
- **PROPFIND property set** — [Nextcloud WebDAV: Basic File & Folder Operations](https://docs.nextcloud.com/server/latest/developer_manual/client_apis/WebDAV/basic.html).
  Default properties, requesting `oc:fileid` / `d:getetag`, and `Depth` control.
- **Change events for phase 4** — [Nextcloud webhook listeners](https://docs.nextcloud.com/server/stable/admin_manual/webhook_listeners/index.html)
  (`NodeCreatedEvent` / `NodeWrittenEvent` / `NodeDeletedEvent`, delivered by a
  background job on the cron interval, 5 min by default) and `notify_push` for
  real-time notifications.
- **No WebDAV impersonation** — [Nextcloud community: impersonating users / accessing all files](https://help.nextcloud.com/t/api-impersonate-users-or-to-have-access-to-all-files-folders-from-all-users/179031).
  Underpins §7's constraint that the indexable corpus is the shared corpus.
- **The comparison the customer will raise** — [Nextcloud Context Chat](https://docs.nextcloud.com/server/stable/admin_manual/ai/app_context_chat.html).
