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
| `get_user_location` | — | `get_user_location` | Caller's location: a browser GPS prompt when a live chat turn is watching, else coarse GeoIP. |
| `generate_qr_code` | yes | `generate_qr_code` | QR codes (URL, WiFi, vCard, SEPA) as PNG/SVG, rendered in-process. |
| `search_web` | — | `search_web` | Web search via SearXNG or Brave, with optional domain and recency filters. Backend configured on `/admin/models`. |
| `fetch_url` | — | `fetch_url` | HTTP GET. HTML is reduced to readable text unless `raw` is set; images come back viewable; other binary returns metadata. |
| `wikipedia` | — | `wikipedia` | Summary of the best-matching Wikipedia article. |
| `dns_lookup` | — | `dns_lookup` | DNS records over DoH. |
| `whois_lookup` | — | `whois_lookup` | Domain registration via RDAP. |
| `tls_cert` | — | `tls_cert` | TLS certificate inspection (issuer, validity, days to expiry, SANs). |
| `fetch_attachment` | — | `fetch_attachment` | Read an attachment. Two-tier PDF reading (text layer, then rasterised pages) with `page_from`/`page_to`; Office files return structured content. |
| `upload_attachment` | yes | `upload_attachment` | Attach a model-generated file to the reply. |
| `list_attachments` | yes | `list_attachments` | Inventory of the conversation's files, so assets get reused instead of regenerated. |
| `load_image_url` | yes | `load_image_url` | Fetch an image from a URL and keep it as a reusable conversation attachment. |
| `create_document` | yes | `document` | Open a canvas document. |
| `edit_document` | yes | `document` | Replace a canvas document's content. |
| `edit_document_section` | yes | `document` | Edit one section of a canvas document. |
| `read_document` | yes | `document` | Read a canvas document back. |
| `list_documents` | yes | `document` | List the conversation's canvas documents. |
| `list_document_versions` | yes | `document` | Version history of a canvas document. |
| `restore_document_version` | yes | `document` | Roll a canvas document back to an earlier version. |
| `rag_search` | — | `rag_search` | Hybrid search (dense kNN fused with FTS5/BM25) over an indexed collection. Returns chunks with path, line range, score. |
| `rag_list_collections` | — | `rag_list_collections` | Discover which collections exist before searching them. |
| `remember` | — | `memory` | Persist a durable fact about the user. |
| `recall` | — | `memory` | Retrieve everything remembered about the user, each with its `id`. |
| `update_memory` | — | `memory` | Correct a stored fact in place, addressed by the id `recall` returned. |
| `forget` | — | `memory` | Delete a stored fact by id. |

The canvas tools and the four memory tools collapse to the `document` and
`memory` keys respectively — see `DOCUMENT_IDS` / `MEMORY_IDS` in `catalog`.
Turning `memory` off has to remove the mutating tools too, not just the
read/write pair.

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
| `capture_webpage` | `[sandbox] enabled` | — | `capture_webpage` |
| `render_typst` | `[sandbox] enabled` | — | `render_typst` |
| `render_excalidraw` | `[sandbox] enabled` | — | `render_excalidraw` |
| `read_sandbox_output` | `[sandbox] enabled` | yes | `read_sandbox_output` |

`export_document` is the one sandbox tool that rides the canvas `document`
toggle: it exports a canvas document, so it belongs to that capability from
the user's point of view even though it needs the sandbox to run.

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
