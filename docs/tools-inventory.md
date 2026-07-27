# Tool inventory

Every tool the gateway can offer an LLM, with the condition under which it is
registered. For *how* tools work — the trait, the registry, RBAC, the
tool-call loop — see [`tools-rbac.md`](tools-rbac.md).

This file is **drift-guarded**: `crates/gateway/tests/it/tools_inventory.rs`
discovers the real tool ids from the source and fails CI when one is missing
here (or when this file names an id no tool implements). Adding a tool
therefore forces a conscious choice — document it, or allow-list it in that
test's `UNDOCUMENTED`.

Deliberately **no line numbers and no crate-relative paths** in this file.
Both rot on every refactor, and this document existing in a stale state is the
exact failure it was written to fix. Symbols (`category_for`,
`requires_chat_session`, `ToolRegistry::with`) survive file moves; a tool id is
itself a stable contract.

## How to read the columns

- **Gate** — what has to be true for the tool to be registered at all. A tool
  that is *never* registered is invisible to the model; a tool that is
  registered but unusable would waste a round-trip on a guaranteed error,
  which is why several tools are gated rather than always-on (see
  "Registration gates" below).
- **Chat-only** — `requires_chat_session` returns true, so the `/v1` proxy
  paths do not advertise it. These need a live chat turn to attach output to.
- **Toggle key** — the switch on `/tools` (and the argument the model passes
  to `enable_tools`). Several tools share one key: users reason about
  "Memory", not about `remember` and `recall` separately.

## Always registered

No configuration required. A tool here can still fail at runtime when its
*storage* is unconfigured (e.g. `[chat.s3]`), but it fails with a clear
message rather than being absent.

| Tool | Chat-only | Toggle key | What it does |
|---|---|---|---|
| `enable_tools` | — | *(hidden, always on)* | The lazy-disclosure bootstrap: turns other tool groups on for the rest of the conversation. Not presented as a toggle — `allowed_tools_for_session` force-keeps `BOOTSTRAP_TOOL_ID`, so a switch for it would be inert. |
| `company_echo` | — | *(hidden)* | Smoke test for the tool-call loop. Hidden from `/tools` and from `enable_tools` via `is_hidden`; still RBAC-grantable. |
| `get_current_timestamp` | — | `get_current_timestamp` | Timezone-aware current date/time, from the caller's `users.timezone`. |
| `convert_currency` | — | `convert_currency` | Currency conversion at daily ECB reference rates. |
| `ask_user` | yes | `ask_user` | Ask the user a short question mid-turn and wait for the answer, instead of guessing. Needs a live chat turn *and* someone watching it; times out and reports `answered: false` otherwise. |
| `notify_user` | — | `notify_user` | Send the user a Web Push notification (long work finished, a scheduled action found something). Deliberately *not* chat-only — a notification lands on a device, not in a conversation, so it also works from the headless scheduler. Hard limit of **one per turn**, latched in `PushNotifier`; errors clearly when `[push]` is unconfigured or the user has no subscribed device. |
| `get_user_location` | — | `get_user_location` | Caller's location: a browser GPS prompt when a live chat turn is watching, else coarse GeoIP. |
| `generate_qr_code` | yes | `generate_qr_code` | QR codes (URL, WiFi, vCard, SEPA) as PNG/SVG, rendered in-process. |
| `search_web` | — | `search_web` | Web search via SearXNG or Brave, with optional domain and recency filters. Backend configured on `/admin/models`. |
| `fetch_url` | — | `fetch_url` | HTTP GET. HTML is reduced to readable text unless `raw` is set; images come back viewable; other binary returns metadata. |
| `wikipedia` | — | `wikipedia` | Summary of the best-matching Wikipedia article. |
| `dns_lookup` | — | `dns_lookup` | DNS records over DoH. |
| `whois_lookup` | — | `whois_lookup` | Domain registration via RDAP. |
| `tls_cert` | — | `tls_cert` | TLS certificate inspection (issuer, validity, days to expiry, SANs). |
| `fetch_attachment` | — | `fetch_attachment` | Read any file of the conversation — an attachment or (by id/title) a canvas document, which comes back as text plus its version. Tiered PDF reading — text layer, rasterised pages, `mode="ocr"` (gateway OCR, images too), `mode="auto"` (text-or-OCR) — with `page_from`/`page_to`; Office files return structured content. |
| `upload_attachment` | yes | `upload_attachment` | Attach a model-generated file to the reply. |
| `offer_download` | yes | `upload_attachment` | Hand a file the conversation *already holds* to the user as a download chip on the current reply — an attachment, or a canvas document (its current version is written out as a file, named from the document's title) — including objects with no chip of their own (a typst render's hidden `.json` data base, an intermediate artifact) and files from earlier turns. Takes a reference, never content: the object is copied inside S3, so a large payload never round-trips through the model as prose. Session-scoped twice over — a marker-backed id is proven in-session by the enumeration, an unlisted `<turn_id>/<filename>` by `turn_in_session`. Shares the `upload_attachment` toggle: one switch for "let the assistant hand me files". |
| `list_attachments` | yes | `list_attachments` | Inventory of the conversation's files, so assets get reused instead of regenerated. |
| `load_image_url` | yes | `load_image_url` | Fetch an image from a URL and keep it as a reusable conversation attachment. |
| `import_file` | yes | `document` | Turn a text attachment (upload or produced artifact) into an editable, versioned canvas document — server-side, so the content never round-trips through the model. The on-ramp that makes an uploaded `.typ`/`.csv`/`.json`/`.md` editable a passage at a time (and hand-editable by the user); `offer_download` is the exit ramp. Text formats only: binary attachments stay attachments, already usable by id (`att:` refs, sandbox staging, `fetch_attachment`). Capped at the same 512 KB the document tools can write. |
| `create_document` | yes | `document` | Open a canvas document. |
| `edit_document` | yes | `document` | Replace a canvas document's content. |
| `edit_document_section` | yes | `document` | Edit one section of a canvas document. |
| `read_document` | yes | `document` | Read a canvas document back. |
| `list_documents` | yes | `document` | List the conversation's canvas documents. |
| `list_document_versions` | yes | `document` | Version history of a canvas document. |
| `restore_document_version` | yes | `document` | Roll a canvas document back to an earlier version. |
| `delete_document` | yes | `document` | Soft-delete a canvas document: hidden from the canvas and from `list_documents`, version history kept, reversible. |
| `undelete_document` | yes | `document` | Undo a `delete_document`. Deliberately *not* named `restore_document` — that would sit one suffix away from `restore_document_version`, which does something else entirely. |
| `schedule_action` | yes | `schedule` | Create a recurring prompt (5-field cron + IANA timezone, defaulting to `users.timezone`). Validates with `Cron::parse` and returns `describe()` + the next 3 run times, so a wrong-but-valid expression is caught before the first run. Inherits the turn's model from `ToolContext.model`; **always** creates the action with tools off. |
| `list_scheduled_actions` | — | `schedule` | The caller's own scheduled actions, each with the same human schedule preview. Read-only, so it works off-chat. |
| `delete_scheduled_action` | yes | `schedule` | Delete one of the caller's actions. Another user's id is reported as missing, not forbidden (no existence leak). |
| `rag_search` | — | `rag_search` | Hybrid search (dense kNN fused with FTS5/BM25) over an indexed collection. Returns chunks with path, line range, score. Optional `path_glob` scopes it to part of the corpus — filtered inside the query on the lexical side, and after kNN candidate generation on the dense side (which is why the candidate pool widens when it is set). |
| `rag_grep` | — | `rag_grep` | Regex scan over an indexed collection's chunk text: matching lines with file, line number and context. For patterns BM25 cannot express (`TODO\(.*\)`, `impl .* for Tool`). Full scan with no index behind it, so it is bounded by result / row / wall-clock limits and reports which one it hit. |
| `rag_list_collections` | — | `rag_list_collections` | Discover which collections exist before searching them. |
| `remember` | — | `memory` | Persist a durable fact about the user. |
| `recall` | — | `memory` | Retrieve everything remembered about the user, each with its `id`. |
| `update_memory` | — | `memory` | Correct a stored fact in place, addressed by the id `recall` returned. |
| `forget` | — | `memory` | Delete a stored fact by id. |

The canvas tools, the four memory tools and the three scheduling tools
collapse to the `document`, `memory` and `schedule` keys respectively — see
`DOCUMENT_IDS` / `MEMORY_IDS` / `SCHEDULE_IDS` in `catalog`. Turning `memory`
off has to remove the mutating tools too, not just the read/write pair; the
same reasoning groups `list_scheduled_actions` with the two tools that change
a schedule, since "can see my schedule but not change it" is not a
distinction anyone configures.

`schedule_action` and `delete_scheduled_action` are chat-only for a reason
that isn't about attaching output: both require an `ask_user` confirmation,
because the action they write later runs **as the user**, unattended, until
removed. That makes a scheduled action a persistent prompt-injection vector,
so a human has to approve it — which also means a scheduled run cannot create
further scheduled actions (the headless worker has a session but nobody
watching, so the confirmation goes unanswered and the write is refused).

## Conditionally registered

Gated so the model is never offered a tool whose every call could only answer
"not configured".

| Tool | Gate | Chat-only | Toggle key |
|---|---|---|---|
| `lookup_ip` | `[geoip]` configured | — | `lookup_ip` |
| `generate_image` | an `image`-kind upstream pool exists | yes | `generate_image` |
| `edit_image` | an image backend advertises `supports_edit` | yes | `edit_image` |
| `read_skill` | `[skills]` configured | — | *(always on when the caller has a permitted skill)* |
| `run_in_sandbox` | `[sandbox] enabled` | — | `run_in_sandbox` |
| `generate_document` | `[sandbox] enabled` | — | `generate_document` |
| `export_document` | `[sandbox] enabled` | yes | `document` |
| `convert_document` | `[sandbox] enabled` | — | `convert_document` |
| `edit_presentation` | `[sandbox] enabled` | — | `edit_presentation` |
| `capture_webpage` | `[sandbox] enabled` **and** the runner reports egress | — | `capture_webpage` |
| `browse_page` | `[sandbox] enabled` **and** the runner reports egress | — | `browse_page` |
| `render_typst` | `[sandbox] enabled` | — | `render_typst` |
| `render_excalidraw` | `[sandbox] enabled` | — | `render_excalidraw` |
| `read_sandbox_output` | `[sandbox] enabled` | yes | `read_sandbox_output` |

`export_document` is the one sandbox tool that rides the canvas `document`
toggle: it exports a canvas document, so it belongs to that capability from
the user's point of view even though it needs the sandbox to run.

### Egress is a *runner* capability, not gateway config

Whether a sandbox can reach the network is decided by the **runner's**
deployment (`SANDBOX_EGRESS_NETWORK` + `SANDBOX_EGRESS_PROXY`), which nothing
in the gateway's own config can see. So the gateway asks: at boot it reads
`GET /healthz` on the runner, which reports `egress: true|false`
(`shared::sandbox::RunnerHealth`), and remembers the answer for the process
lifetime.

That answer changes what the model is offered:

- **No egress** → `capture_webpage` and `browse_page` are **not registered**
  at all, and `run_in_sandbox`'s schema has **no `network` property**, with a
  description that states plainly there is no network rather than implying a
  permission the model could ask for. This is the "absent beats
  always-failing" rule below: before this, `capture_webpage` was advertised on
  an offline runner and every call failed with `network egress requested but
  not configured on this runner`.
- **Egress** → both tools register and `network` appears as an option.
- **Unknown** (runner unreachable at boot, or one older than the health field)
  → treated as *available*. Withdrawing capabilities needs positive evidence;
  an unreachable runner breaks every sandbox tool anyway, so hiding a subset
  would turn a transient outage into an apparent permanent capability loss.

Not re-probed: egress changes when an operator edits a unit file and restarts
things, and a capability set that shifted under a running conversation would be
worse than a slightly stale one.

**`render_typst` is deliberately not chat-only, even though part of it needs a
session.** It renders either inline `source` or a `document_id` from the canvas.
The inline path is the common one and works fine on `/v1`; only the
`document_id` path needs a chat session, and it fails there with a message
naming the reason ("canvas documents are only available inside a chat session")
rather than a generic error. Marking the whole tool chat-only would remove a
working capability from the proxy paths to protect an argument the model
wouldn't have a use for there — a `/v1` caller has no canvas to reference. Same
reasoning applies to `run_in_sandbox`'s optional canvas-document staging, which
degrades to a note instead of failing the run. `export_document` is different
and *is* chat-only: a canvas document is its only possible input.

### The two shapes a conversation's files come in

A conversation holds files in two stores, on purpose, and the tools cross
between them rather than duplicating either:

| | Attachments | Canvas documents |
|---|---|---|
| Address | `<turn_id>/<filename>` | `doc_…` |
| Mutable | no — one immutable blob per write | yes — every change appends a version |
| Content | any bytes (images, PDFs, archives) | UTF-8 text, ≤ 512 KB |
| User can edit | no | yes (the document panel) |
| Reached by | `fetch_attachment`, `att:` refs, sandbox `attachments`, `offer_download` | `fetch_attachment`, `read_document`/`edit_document`, sandbox `documents` *or* `attachments`, `typst_*` `document_id`/`base`, `export_document`, `offer_download` |

**One reference syntax, every tool.** Naming a file used to depend on which
tool you were calling: `run_in_sandbox` took ids and filenames in
`attachments` but documents only in `documents`, `fetch_attachment` couldn't
read a document at all, and typst image fields wanted an `att:` prefix. Every
file-taking tool now resolves through `file_refs::resolve`, which accepts all
of it — an `<turn_id>/<filename>` id, a bare filename (newest match wins), a
`doc_…` id, an unambiguous document title or its materialised filename, and a
leading `att:` / `doc:` / `file:` that some models add unprompted. So a
reference the model got from *any* result works in *any* argument, and a wrong
one produces the same message (naming both inventories) wherever it is passed.

Session scoping is part of resolution rather than a check each caller
remembers: a marker-backed attachment is proven in-session by the enumeration,
an unlisted `<turn>/<file>` by `turn_in_session`, a document by the
session-scoped `get_version`. Another conversation's real id resolves to the
same "not found" as a typo, so nothing leaks. A *deleted* document is its own
error, because the fix is `undelete_document` rather than a different id.

Crossing over: **`import_file`** turns a text attachment into a document;
**`offer_download`** writes a document's current version back out as a
downloadable file. Both copy inside the gateway — content never round-trips
through the model, which is what made "give me that file" cost two passes of
the whole payload and invite retyping drift.

The user can **hand-edit** a document in the panel (`POST
/chat/{id}/document/{doc_id}/edit`, owner-only, newest version only). That save
is a normal new version, marked as authored by the user — and the request
context then tells the model which documents were hand-edited and at what
version, because nothing in the transcript would: its history still holds the
content *it* wrote, so an unwarned edit reverts the correction. The warning
stops once the model writes on top (it has seen the change by then).

A **typst render** now parks its field data in a canvas document (its
`document_id` comes back in the result) instead of the hidden
`<turn>/<basename>.json` it used to write. That data was the one file the model
worked on constantly and nobody could see: not in the panel, not downloadable,
not stageable, editable only through `_edit`. The `_read` / `_edit` / `_pptx`
tools take either id — a slash means the old attachment shape — so
conversations from before the change keep editing their existing base, and
`_edit` writes back to whichever surface it read from. The render deliberately
does *not* push the panel open for a data document: the deliverable is the PDF.

All three refuse a **soft-deleted** document explicitly. `documents::get`
resolves deleted rows on purpose (see `delete_document`), so without that check
a stale id would quietly produce an export, a PDF, or a staged file from work
the user threw away.

## Dynamic families

These have no fixed id — one tool per discovered template / workflow /
connected server. The drift guard matches them by prefix.

| Family | Source | Toggle key |
|---|---|---|
| `typst_<id>` plus `_edit` / `_read` / `_pptx` | one per directory under `[typst] templates_dir`, discovered at boot | `typst_<id>` (the render id; variants collapse onto it via `entry_key_for`) |
| `comfyui_<id>` | one per manifest in the `[comfyui]` catalog, hot-reloadable | `comfyui` — **one key for the whole family**, so a newly reloaded workflow is enabled automatically |
| `mcp__<server>__<tool>` | per-user MCP connectors, connected lazily per request | `mcp__<server>` — one key per integration |

`typst_<id>_pptx` additionally requires `[sandbox]` (the conversion runs
there) and the template opting in via a `[pptx]` block in its manifest.

## Registration gates: why not just register everything

A registered-but-unusable tool costs a full model round-trip to discover it
cannot work, and the model has no way to tell "misconfigured" from "wrong tool
for the job". So the rule is:

- **Missing capability → don't register.** `lookup_ip` can do nothing without
  a GeoIP database; `generate_image` needs an image pool. Absent is better
  than always-failing.
- **Missing storage → register, fail clearly.** `generate_qr_code` renders
  in-process and only needs `[chat.s3]` to *deliver* the file. Registering it
  keeps the RBAC config stable across deployments that differ only in storage.
- **Wrong request path → register, don't advertise.** The chat-only tools
  above are real capabilities that simply need a chat turn.
  `requires_chat_session` keeps them out of the `/v1` tool list instead of
  letting the model pick one and get an error.

## Adding a tool

1. Implement `Tool` and register it with `ToolRegistry::with` (ids must match
   OpenAI's function-name regex — the registry asserts this).
2. Grant it to at least one role in `[rbac]`, or no one can use it.
3. Give it a category in `category_for` and display copy in `display_meta`,
   or it renders on `/tools` under "Utility" with its LLM-facing description.
4. If its `run` hard-fails without a chat session, add it to
   `requires_chat_session`.
5. Add a row above. CI will remind you if you forget.
