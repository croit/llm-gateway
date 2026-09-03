# Fileshare RAG

How the gateway indexes a company's file share — Nextcloud, ownCloud,
OpenCloud, plain WebDAV, and whatever provider is registered next — and
answers questions about it.

This is the architecture doc. For the decisions behind it, and what is
deliberately not built, see [`nextcloud-rag-plan.md`](nextcloud-rag-plan.md).

## What it is for

Two shapes of question, which need two different mechanisms:

| Question | Mechanism |
| --- | --- |
| "What does the support contract say about SLAs?" | passage retrieval — `rag_search` |
| "When did we last get an invoice from ACME, and how much?" | a **structured query over extracted fields** — `rag_query_documents` |

The second is the reason half of this subsystem exists. "Last" is a
superlative over a filtered set: answering it needs *every* ACME invoice,
ordered by date. Top-k similarity over thousands of near-identical invoices
returns five arbitrary ones and — worse — gives the model no way to notice
that. No amount of better embedding fixes it; it is the wrong shape of query.

## The pipeline

```
                 ┌──────────────┐
   file host ───▶│  provider    │  list_dir / fetch / web_url / probe
  (WebDAV, …)    │  (dyn trait) │  reports its own capabilities
                 └──────┬───────┘
                        │ RemoteEntry { id, rel_path, version, mime }
                 ┌──────▼───────┐
                 │  tree walk   │  breadth-first, concurrent
                 │              │  skips unchanged subtrees
                 └──────┬───────┘
                        │ TreeSnapshot { files, dir_versions, pruned, failed }
                 ┌──────▼───────┐
                 │  sync plan   │  new / changed / moved / deleted
                 └──────┬───────┘
                        │ only the delta
                 ┌──────▼───────┐
                 │  extraction  │  text → PDF text layer → OCR → office
                 │    ladder    │  (OCR cached by content hash)
                 └──────┬───────┘
                        │ ExtractedDoc { pages[], extractor, coverage }
                 ┌──────▼───────┐
                 │ profile pass │  fields + summary, one LLM call
                 │  (optional)  │  (cached by content hash + profile version)
                 └──────┬───────┘
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   chunk + embed   rag_documents   rag_doc_fields
   (usearch+FTS5)  (title,summary) (vendor,date,total…)
        │               │               │
        └──── rag_search ┴─ rag_list_documents ┴─ rag_query_documents
              │
        (optional cross-encoder rerank over the fused candidates)
```

Everything below the provider is provider-agnostic. `worker.rs` names no
provider; it asks `FileProvider::capabilities()` what is available and
behaves accordingly.

## Crate placement

The indexer lives in `gateway-features`, which sits **below**
`gateway-runtime` and must never name it (see
[`architecture.md`](architecture.md#crate-boundaries)). Two capabilities it
needs live above it, so both are inverted — declared as a port down here,
implemented up there, injected at boot from `gateway/src/main.rs`:

| Capability | Port | Implementation |
| --- | --- | --- |
| Office extraction (needs the sandbox) | `rag::extract::OfficeExtractor` | `gateway-runtime/…/sandbox/office.rs` |
| OCR | `OcrService`, constructed in `main.rs` | shared with the chat path |

The OCR service is built **once** in `main.rs` and handed to both the
indexer and `RamaState` (`RamaState::with_ocr`). Its concurrency gate only
bounds GPU work if every caller holds the same instance; two would silently
allow twice the configured concurrency.

| Where | What |
| --- | --- |
| `gateway-features/src/server/rag/source/` | provider trait, registry, WebDAV provider, tree walker |
| `gateway-features/src/server/rag/extract.rs` | bytes → text ladder |
| `gateway-features/src/server/rag/profile.rs` | the per-document LLM extraction pass |
| `gateway-features/src/server/rag/sync.rs` | what changed since last time (pure) |
| `gateway-features/src/server/rag/rerank.rs` | optional cross-encoder second opinion |
| `gateway-features/src/server/rag/worker.rs` | the build loop that ties it together |
| `gateway-core/src/server/db/rag.rs` | collections, refs, files, chunks |
| `gateway-core/src/server/db/rag_documents.rs` | profiles, documents, fields, the structured query |
| `gateway-tools/src/rag*.rs` | the six model-facing tools |
| `gateway-web/src/pages/rag*.rs` | `/rag`, the source form, the profile editor |

## Providers: the extension point

```rust
// pseudocode — see source/mod.rs for the real signatures
#[async_trait]
pub trait FileProvider: Send + Sync + 'static {
    fn kind(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn root(&self) -> DirRef;
    async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError>;
    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<Vec<u8>, ProviderError>;
    fn web_url(&self, entry: &RemoteEntry) -> Option<String>;
    async fn probe(&self) -> Result<ProbeReport, ProviderError>;
    async fn delta(&self, cursor: Option<&str>) -> Result<DeltaPage, ProviderError>;
}
```

**Adding a provider is one module plus one line** in
`ProviderRegistry::with_builtins`. Nothing else changes — not the worker, the
chunker, the store, the tools, or the admin page. Four decisions make that
true:

1. **Capabilities, not product names.**
   `ProviderCapabilities { subtree_pruning, delta, stable_ids }` is
   how the worker decides what it may do. `Default` is the pessimistic set, so
   a provider that opts into nothing still works — full walk, path identity.
   The walker asks the provider at the moment it can answer, never before: a
   provider that *learns* what it can do from its first response (WebDAV
   latches `oc:fileid`) would otherwise be sampled cold and report nothing.

2. **Identity is `RemoteEntry::id`, not the path.** Every serious host has a
   stable per-file id that survives a move (`oc:fileid`, a Graph `driveItem`
   id, a Dropbox `id:` handle). Keying on it turns a reorganised folder of 400
   scans into 400 path updates instead of 400 OCR runs.

3. **`version` is opaque.** An etag, a ctag, a `rev`, a content hash —
   compared for equality, never parsed. That is what lets one walker serve
   hosts with completely different change models.

4. **Providers describe their own settings.** `ProviderFactory::config_fields()`
   returns `&[ConfigField]` (key, label, help, kind, required, default). The
   admin form renders from it, validation runs from it, and defaults are
   written once. A page that matched on `"webdav"` to decide which inputs to
   draw would put the extensibility back where it started.

5. **Providers describe how they are authorised.** `ProviderFactory::auth()`
   returns `AuthKind::Fields` (everything is typed) or `AuthKind::OAuth2`
   (a person consents in a browser), which also names *which* config keys hold
   the client id and secret — every vendor calls them something different, and
   a provider that declared `app_key` would otherwise fail at Connect with a
   message telling the operator to fill in a field they had already filled in.
   The consent route, the Connect button, the connected badge and the
   `auth` block on `GET /api/v0/rag/providers` are all driven by that one
   answer, so a second OAuth source is still a module plus a registration
   line.

### The WebDAV provider

One implementation covers Nextcloud, ownCloud, OpenCloud and generic RFC 4918
servers, because they differ in exactly two places:

- **Where the DAV root lives** — `dav_path` is a template
  (`/remote.php/dav/files/{username}` by default).
- **Whether the ownCloud extension properties are present** — detected from
  the first response carrying an `oc:fileid`, then latched. With them the
  provider reports `stable_ids` and `subtree_pruning`; without them it reports
  neither and gets a correct, slower sync.

Subtree pruning is the property that makes a nightly re-sync cheap: on the
ownCloud lineage a collection's etag changes when anything beneath it changes
and propagates to the root, so an unchanged etag proves an unchanged subtree.
That is a guarantee of *those* servers, not of WebDAV — hence the detection.

Both that and move-proof file identity are **verified against a real server**,
not just cited: `mise run test-nextcloud` boots a throwaway Nextcloud and
asserts that a nested edit moves the root etag, and that `oc:fileid` survives
a MOVE. See [Testing](#testing) — if either stopped holding, a cheap re-sync
would silently miss changes, or a reorganised folder would re-OCR itself.

### The Google Drive provider

Drive is the second provider, and it was chosen partly because it breaks three
assumptions the WebDAV lineage never tested. Each break is a seam that now
exists for the next provider.

**There is no password to type.** Drive is three-legged OAuth2: an operator
registers a client in a Google Cloud project, saves its id and secret on the
collection, and then clicks **Connect** to grant access in the browser. The
callback seals the refresh token into the collection's existing
`source_secrets_ct` blob beside the client secret — no new secret store, and
it inherits the at-rest encryption and delete-cascade that blob already has.
The provider trades that refresh token for a one-hour access token whenever
it needs one, cached behind a mutex so a concurrent walk refreshes once.

Ordering matters and is load-bearing: the client id has to be **saved** before
there is anything to consent with, so `validate()` must accept a
not-yet-connected source and only `build()` refuses. The save path skips its
dry-run build for exactly this case (`awaiting_consent`), otherwise saving and
connecting would deadlock against each other.

The consent is per *collection*, not per user — a corpus is shared and the
background worker has no session to borrow. **Everyone who can search the
collection sees what the connected account can see.** With `allowed_groups`
as the only access control, that is the fact with a security consequence, so
it is recorded rather than inferred: the callback asks the provider who it
just authenticated as and stores `connected_account` / `connected_by` /
`connected_at` on the collection, and the form shows the account beside the
Connect button. Asking the provider rather than trusting the session is
deliberate — the person who clicked Connect and the Google account they
picked on the consent screen need not be the same, and it is the latter that
decides what the index can see.

**Google-native documents have no bytes.** A Doc, Sheet or Slide cannot be
downloaded; it must be *exported*, and the target format decides how much
survives. We ask for the Office formats — docx, xlsx, pptx — rather than
`text/plain`, because the extraction ladder already reads those through the
sandbox and keeps tables, headings and speaker notes. A plain-text export
flattens a budget into a wall of numbers. The export extension is appended to
the file's path, because the ladder dispatches on extension and a Google Doc's
name usually has none.

The consequence is worth stating plainly: **reading Google-native documents
requires the document sandbox.** Without it they land in the skip summary with
a reason naming the sandbox, rather than being silently absent. Binary files
in Drive (PDFs, images, uploaded Office files) need no sandbox for the PDF and
OCR rungs. Drawings export as PNG and go through OCR.

**Names are neither identity nor unique.** Drive lets two files in one folder
share a name and lets a name contain `/`. The Drive id is the identity and is
what `sync::plan` keys on — but the *store* is still keyed on the path
(`rag_files` is `UNIQUE (collection_id, path)`), so a colliding pair would
have one row silently overwrite the other. The provider therefore sanitises
separators out of each segment and suffixes *every* member of a colliding
group with its id. Every member, not just the later ones, because the rule
keys off a count rather than a position: a path that depended on ordering
would shuffle whenever Drive listed the group differently. For the same
reason `list_dir` collects every page before naming anything — resolving
collisions page by page would miss a pair that straddles a page boundary.

**A `RemoteEntry`'s `mime` describes the bytes `fetch` returns**, not the
object in Drive: an exported drawing reports `image/png`, not
`application/vnd.google-apps.drawing`. That matters because `extract` hands
this string to the OCR sidecar as a multipart `Content-Type`. The
export-or-download decision rides on the provider-private `locator` instead,
so the two never disagree.

Capabilities: `stable_ids` yes (a Drive id survives rename and move),
`subtree_pruning` **no** (Drive folders carry no version that propagates from
their descendants, so there is nothing to compare), `delta` not yet. Drive's
`changes.list` is the right answer for a large corpus and is why the `delta`
seam exists, but the worker has no delta consumer, so a re-sync re-walks. The
walk is metadata-only and `sync::plan` still skips fetch, extract and embed
for every file whose `version` is unchanged — a re-sync costs listings, not
documents.

## The extraction ladder

`extract.rs`, cheapest rung first:

| Input | How | Needs |
| --- | --- | --- |
| text-ish | UTF-8 decode | — |
| PDF with a text layer | `pdf::extract_text_pages` | — |
| PDF that is a scan | the OCR backend | an `ocr` pool |
| image | the OCR backend | an `ocr` pool |
| `.docx` / `.pptx` / `.xlsx` | sandbox extractor | `[sandbox]` |

Classification is by **extension first, MIME second**: the extension is chosen
by whoever saved the file, while the MIME type is guessed by the server, and
file hosts return `application/octet-stream` for ordinary documents often
enough that trusting it first loses real content.

Two cost properties matter at corpus scale:

- **A born-digital PDF never reaches the GPU.** The text layer is read first
  and `ocr::pdf_needs_ocr` decides by counting characters per page — no word
  lists, so it behaves identically for German and English.
- **OCR is cached by content hash** (`ocr_derivatives`), so a full rebuild
  re-embeds but does not re-recognise.

Everything degrades rather than fails. With no OCR pool and no sandbox the
ladder still reads every text file and reports the rest as *skipped, with the
reason, grouped and counted* on the collection's timeline. "Ready" next to
3000 unreadable scans is the most expensive silence this system can produce.

## Provenance

Chunks carry `loc_kind` (`line` | `page`) plus a range. Source files are cited
by line; anything that went through extraction is cited by page, because lines
do not survive a PDF and a page is what a person can open the original to.

Chunking runs **per page** for paginated documents so a chunk never straddles
a page boundary — an imprecise citation is worse than a coarse one, because
the reader opens page 3, does not find the sentence, and stops trusting the
answer.

Tools emit a single `location` string (`lines 12-30`, `page 4`) rather than a
pair of integers: the unit now differs per document, and handing a model two
numbers with no unit invites it to quote a page as a line. Each hit also
carries the provider's `web_url` where there is one.

## Document profiles and the structured layer

A **profile** is an operator-defined extraction schema: a prompt plus a list
of typed fields. Attached to a collection, it adds one LLM call per document
that returns normalised fields and a two-sentence summary.

Normalisation is the model's job, in the prompt — `31.12.2025`, `12/31/2025`
and `2025-12-31` all come back as one ISO date; `1.234,56 €` and `$1,234.56`
as a decimal plus an ISO-4217 code. That is how one code path serves a German
and English corpus with no language-specific rule anywhere.

Results land in the per-collection store as `rag_documents` (title, summary,
extractor, page coverage) and `rag_doc_fields` — an EAV table with three typed
columns (`value_text`, `value_num`, `value_date`) so ordering and range
filters use an index. EAV rather than a wide table because the field set
belongs to the profile: an operator adding a field must not need a migration.

Extractions are cached in `rag_extractions`, keyed by
`(doc_sha256, profile_id, profile_version, model)`. **Editing a profile bumps
its version**, which invalidates the cache — without that, an edit meant to
fix a bad extraction would appear to do nothing.

Three properties of the query layer are load-bearing:

- **`total_matches` always travels with the results.** The failure mode is not
  a wrong answer but a confident one: a model handed 10 of 47 invoices will
  say "we received 10" unless told otherwise.
- **Documents missing the sort key sort last, in both directions.** An unknown
  date is not the oldest date, and surfacing it atop a "most recent" answer
  would be actively misleading.
- **Ambiguity is surfaced, not resolved.** When a text filter matches several
  distinct values (`ACME` hitting both `ACME GmbH` and `ACME Deutschland AG`),
  the result says so and the model is told to ask. Entity resolution is hard,
  and the obvious shortcut — a legal-suffix list — is a language-specific word
  list, which this product does not do.

Chunks are embedded with a one-line **context header** derived from the
extracted fields (`[Invoice | ACME GmbH | 2025-11-04 | Finance/2025]`).
Prepended before embedding, never to the stored text. A bare paragraph from
page 2 of an invoice is embedding-identical to the same paragraph in 400
others; the header is what separates them in vector space.

## What invalidates an index

Three separate questions, kept separate because conflating any two of them
caused a bug:

- **Is this corpus searchable?** `last_indexed_commit is not null`. A rebuild
  request no longer clears it — a full rebuild is atomic and the live store
  answers until the swap, so taking the collection offline for the duration
  was a lie about data that was sitting right there.
- **Must the next build start from scratch?** `force_full_rebuild`, set by
  `request_full_rebuild` and cleared by a successful swap. Also implied when
  the ref has never completed a build.
- **Has anything changed about how documents are *read*?** Two inputs. The
  collection's own settings — source, profile, extraction and embedding
  models, chunking, globs — are compared by `rag_db::index_shape_changed`,
  which both the web form and the JSON API call so they cannot drift. And the
  set of available extractors is fingerprinted onto the ref at each build:
  turning on OCR or the document sandbox invalidates what was indexed without
  them, because the files skipped for want of a backend are readable now and
  an incremental diff would find them unchanged at the source and never look
  again.

A pass also decides whether it may be treated as **authoritative**, which is
what licenses the next sync to prune. It may not be if any directory failed
to *list* (`TreeSnapshot::is_complete`) or any file failed to *read*
transiently — a 503, an OCR backend that was down. Both would otherwise let a
later sync prune straight past a document that was never indexed. A file that
is merely *unsupported* does not block it, because it will not become
readable on its own — that is what the extractor fingerprint is for.

## Incremental sync

The first build of a remote collection writes a fresh store folder and swaps
onto it atomically. **Every build after that updates the live store in
place**, driven by `sync::plan`:

| Case | Cost |
| --- | --- |
| unchanged (same id, same version) | nothing |
| moved (same id + version, new path) | one column update |
| changed version | fetch, extract, embed; old chunks and vectors removed |
| new | fetch, extract, embed |
| absent from an authoritative walk | chunks, vectors and rows deleted |

In-place updating forfeits the atomic swap, so three things replace it:

- **Per-file transactionality.** A crash leaves at most one file inconsistent.
- **The diff is the resume cursor.** Directory versions are stored only after
  a fully successful pass, so an interrupted run costs one extra walk: the
  next diff sees already-indexed files as unchanged and skips them. No cursor
  table, and nothing to get wrong.
- **Deletions require a complete walk.** A directory that returned 503 is
  indistinguishable from one that was emptied. `TreeSnapshot::is_complete()`
  gates every deletion, and files under a *pruned* subtree are explicitly kept
  — without that, the first cheap re-sync would wipe most of the corpus.

`BuildOutcome::Swapped` carries `live_uuid` so the caller can tell a folder
swap from an in-place pass; reaping the "old" folder after an incremental
build would delete the live store.

## Reranking

Hybrid search scores the query and each passage **independently** — a chunk's
embedding is computed once, at index time, knowing nothing about what will be
asked. That is what makes it fast enough to run over a whole corpus, and also
what makes it blunt on documents that look alike: three thousand invoices
share a layout, a vocabulary and most of their words.

A cross-encoder scores the *pair*, so it sees the relationship neither vector
saw. It cannot run over a corpus — one model call per passage — but it runs
comfortably over the few dozen candidates fusion narrowed to. Configure a
`kind = "rerank"` pool and the search path widens its candidate net
(`rerank_candidates`, default 50) and re-sorts what came back.

Optional and silently so: with no such pool, search returns the fused ranking
exactly as before. A reranker that errors or times out logs a warning and the
fused order stands — degraded ordering beats no answer.

The `/rerank` request shape is the de-facto one served by TEI, Infinity and
vLLM's scoring endpoint. Both response shapes (`{"results": […]}` and a bare
array) parse, and an out-of-range index from a misbehaving backend is dropped
rather than trusted.

## Freshness

Three ways a collection gets re-synced, cheapest first:

| Trigger | When |
| --- | --- |
| the indexer's poll | every `[rag]` poll interval |
| **the sync hook** | whenever the file host says something changed |
| **Re-index** on `/rag` | an operator decides |

`POST /hooks/rag/{token}` re-queues one collection's refs. Point Nextcloud's
`webhook_listeners` app (or ownCloud's, or a cron line, or any script) at it.
It is unauthenticated by design — the token in the URL *is* the credential,
the same shape `/hooks/{secret}` uses for user webhooks — and only its
SHA-256 is stored, so a leaked database hands out no working URLs. Mint or
rotate it from the collection's row; the plaintext is shown once.

**The body is ignored.** This is a doorbell, not a change feed: what actually
changed is established by the walk that follows, which is cheap on a source
that supports subtree pruning. Accepting a payload would mean trusting an
unauthenticated caller's account of the corpus. Refs already `pending` are
left alone, so a burst of file events does not pile up builds.

## Storage layout

**Central DB** (`[db].path` — small, backed up):

| Table | Holds |
| --- | --- |
| `rag_collections` | config: source kind, provider settings, sealed secrets, profile, embedding model, group ACL, hashed sync token |
| `rag_collection_refs` | per-ref status, `data_uuid`, `dir_versions_json`, `delta_cursor` |
| `rag_document_profiles` | extraction schemas (2 seeded, operator-editable) |
| `rag_extractions` | profile-pass cache |
| `ocr_derivatives` | OCR cache |
| `rag_index_log` | the build timeline shown on `/rag` |

**Per-collection store** (`[rag].data_dir/<uuid>/` — large, regenerable):
`rag.sqlite` (`rag_files`, `rag_chunks`, `rag_chunks_fts`, `rag_documents`,
`rag_doc_fields`) plus `index.usearch`.

Because store folders now persist across builds, the store carries a
`PRAGMA user_version` and an ordered upgrade list in `open_collection_store`.
`CREATE TABLE IF NOT EXISTS` covers a new store but cannot add a column to an
existing one; without the version marker the first schema change would
silently do nothing and surface later as a baffling `ColumnNotFound`.

Provider secrets are sealed with `GATEWAY_ENCRYPTION_KEY` (AES-256-GCM). A
file-host app password grants read access to a company's whole shared document
store, so it does not get the plaintext treatment the older git `pat` column
has.

## The model-facing tools

| Tool | For |
| --- | --- |
| `rag_list_collections` | discovery |
| `rag_search` | hybrid passage retrieval (dense ⊕ BM25, fused by RRF) |
| `rag_grep` | regex over indexed text |
| `rag_query_documents` | filter / sort / aggregate over extracted fields |
| `rag_list_documents` | folder listing with stored summaries |
| `rag_fetch_document` | full text of one document |

All are gated by the collection's `allowed_groups`. A collection the caller
may not see is reported as *unknown*, never *forbidden* — the second answer
would confirm it exists.

Retrieved document text is **untrusted input**. A PDF that says "ignore your
previous instructions" is content, not an instruction; tool results label it
as data. This matters more here than for a code corpus: nobody mails you a
hostile git repo, but anyone can mail you a hostile invoice.

## Operating it

Configured at **`/admin/settings` → Content & data → RAG indexing**, not in a
file. All three fields are **restart-only**: the indexer is a long-running
worker, and `rag.data_dir` additionally does not carry existing indexes with it
— point it somewhere new and everything is reindexed from scratch.

| Field | Meaning |
|---|---|
| `rag.enabled` | Run the indexer at all |
| `rag.data_dir` | Index store, e.g. `/mnt/data/gateway-rag`. Must be on the persistent volume, or every restart reindexes |
| `rag.clone_concurrency` | How many clones and indexing jobs run at once |

A legacy `[rag]` block in `gateway.toml` is imported once on the first boot that
sees the file and ignored afterwards.

Optional capability pools, each of which simply switches a stage on:

| Pool `kind` | Enables |
| --- | --- |
| `embedding` | required — chunks and queries |
| `ocr` | scans and images |
| `chat` | the profile extraction pass |
| `rerank` | cross-encoder reranking |

Plus the code sandbox (`/admin/settings` → Tools) for office documents. Every one of them degrades to "that
stage is skipped, and says so" rather than to a failure.

On `/rag`: pick a **source kind**, fill in the provider's own fields, hit
**Test connection** (which calls `FileProvider::probe()` and reports the
account, the entry count under the configured root, and whether the ownCloud
extensions were detected), optionally attach an extraction profile, and save.
Profiles are authored at `/rag/profiles`.

For an OAuth source (Google Drive) there is one extra step, and it comes
*after* the save:

1. In a Google Cloud project, enable the **Drive API** and create an OAuth 2.0
   Client ID of type *Web application*.
2. Add `<public_url>/rag/oauth/callback` as an authorised redirect URI. It has
   to match `gateway.public_url` exactly — Google compares the string.
3. On `/rag`, create the collection with the client ID and secret and save it.
4. Re-open the collection and click **Connect**. Whoever signs in there is the
   account the corpus is read as; the badge next to the button then reads
   *connected*.

If Google returns no refresh token, it is almost always because that account
had already granted access: revoke the gateway under the account's third-party
access settings and connect again. Reconnecting at any time is safe — it
replaces the stored token and re-queues a build.

The same surface exists as JSON: `POST/PATCH /api/v0/rag/collections` take
`source_kind` + a flat `source_config` map and a `profile` name;
`GET /api/v0/rag/providers` and `GET /api/v0/rag/profiles` return the field
descriptors to build against.

**Cost.** The first pass over a scanned corpus is a multi-hour GPU job: budget
a few seconds per page, and note that it competes with chat and image
generation for the same hardware. Re-syncs are cheap; rebuilds re-embed but
re-run neither OCR nor extraction.

## Testing

Two layers, because they answer different questions.

**Mocked** (`cargo test`, always): the pipeline's own logic — parsing, the
sync plan, the extraction ladder, the query layer, the tools — against
wiremock servers shaped like the documented contracts. Fast, hermetic, and
the right place for "does our code do the right thing".

**Live** (`mise run test-nextcloud`, on demand): the assumptions that are
really claims about someone else's server, and which a mock cannot test
because the mock is built from the same assumption. It boots a throwaway
Nextcloud container, runs the real `WebdavProvider` against it, and tears the
container down afterwards:

| Asserted | Why it matters if wrong |
| --- | --- |
| `oc:fileid` is returned and survives a MOVE | a reorganised folder would re-extract — hours of GPU on a scanned corpus |
| a nested edit moves the parent's etag | pruning would skip a subtree that actually changed |
| an untouched tree answers `Unchanged` | every re-sync would walk the whole corpus |
| the default DAV path + Basic auth work | nothing would index at all |
| spaces and umlauts round-trip href → fetch URL | a German archive would 404 half its documents |
| a wrong password reads as a credential error | the operator would get a puzzle instead of a fix |

The test binary is gated behind `RUN_NEXTCLOUD_E2E`, so a normal `cargo test`
compiles it and skips every case in ~0 ms. It never touches Docker unless you
ask for it. `NEXTCLOUD_E2E_KEEP=1` leaves the container up to poke at;
`NEXTCLOUD_URL` / `NEXTCLOUD_USER` / `NEXTCLOUD_PASS` point it at a server you
already have.

**Still unvalidated live:** the OCR sidecar, the extraction chat model and the
reranker. All three are mocked to their documented contracts, and all three
need real infrastructure to check.

## What is not built

**ACL-faithful per-user filtering.** Today access is per collection, by
gateway group (`allowed_groups`), which fits the shared-folder corpus this
indexes — a service account can only see what has been shared with it anyway.
Filtering individual documents by the *caller's* identity needs a gateway
user → file-host user mapping, which is a deployment question (free if both
sit behind one OIDC issuer, a mapping table otherwise). `rag_files.acl_json`
is populated but unread, so switching it on later needs no re-index.

**Provider `delta()` implementations, and the worker side of them.** The trait
method exists and defaults to unsupported. WebDAV has no delta API; Drive
does (`changes.list`), and it is the obvious next optimisation for a large
Drive corpus. Be honest about the cost: wiring it is not only the provider
method, it is a branch in `gather_remote`, a cursor threaded out through
`build_ref_incremental`, and teaching `sync::plan` a shape that has no
`TreeSnapshot` and no `is_complete()` — a delta page hands you removals
directly. Until then Drive re-walks, which is correct and costs listings
rather than documents. `set_ref_sync_state` deliberately does **not** take a
cursor: nothing can produce one, and a parameter that could only write back
what it was handed made the call sites look wired when they were not.

**Refresh-token rotation.** Google's refresh tokens are long-lived and do not
rotate, so the Drive provider refreshes in memory and never writes a
credential back. A provider that *does* rotate on every redemption —
Microsoft Graph — would need a write-back path from the provider to the
sealed source secrets, which does not exist yet. That is the thing to build
before adding OneDrive, not after.

**No refresh-token write-back.** Repeated here because it is the next thing
to build, not a footnote: Google's refresh tokens are long-lived and do not
rotate, so the Drive provider refreshes in memory and never writes a
credential back. Microsoft Graph rotates on every redemption and would
re-auth-fail after the first restart. The shape to copy already exists —
`McpConnectionManager::refresh` (`gateway-runtime/.../mcp/manager.rs`)
reseals and stores — but a provider built by `ProviderFactory::build` has no
channel back to the sealed source secrets. Settle that before starting
OneDrive, not after.

**Shared-drive coverage is untested.** The listing passes
`supportsAllDrives` and `includeItemsFromAllDrives`, so a shared drive's
folder id should work as a root, but this has not been exercised against a
real shared drive.

The WebDAV provider **is** validated against a real Nextcloud (see
[Testing](#testing)). The Google Drive provider is **not** validated against
real Google: its listing, export and disambiguation logic are unit-tested
against the documented API shapes, and the consent flow is integration-tested
end to end, but no test has ever held a real Google access token. The first
production connect is therefore the first real exercise of the token exchange
and the export endpoints. The OCR sidecar, extraction model and reranker are
likewise mocked to their documented contracts; the protocol assumptions are
cited in
[`nextcloud-rag-plan.md`](nextcloud-rag-plan.md#16-external-references).
