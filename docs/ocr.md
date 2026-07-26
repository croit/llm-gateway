# Document OCR

Uploaded scans and images are recognised by a dedicated OCR backend and handed
to the chat model as untrusted document text, so a conversation about a scanned
invoice works without the chat model being vision-capable.

The feature is **off** until two things are true: `[chat.ocr] enabled = true`
and a healthy `kind = "ocr"` pool serving a model. Until then the gateway
behaves exactly as if OCR did not exist — no tool, no model, no extra call.

## Where it happens

- **Automatically**, for the attachments of the message being answered
  (`openai_driver::enrich_current_message_with_ocr`). Images are always
  recognised; a PDF only when its text layer is too thin to trust, so a
  born-digital PDF never costs GPU time.
- **On request**, via `fetch_attachment` with `mode="ocr"` (recognise this
  document) or `mode="auto"` (for a PDF: text layer if there is one, OCR if
  there isn't). `mode="ocr"` also works on an image attachment, which is how a
  text-only model reads one at all; an image under `auto` still comes back as an
  image, since a vision model loses nothing that way.

Recognised text is injected into the **user** message inside
`--- BEGIN OCR DOCUMENT DATA: <file> --- … --- END OCR DOCUMENT DATA ---`,
behind a line naming it untrusted data. It is never a system message: a scanned
page that says "ignore your instructions" is content, not an instruction.

The original upload is untouched and still downloadable — OCR output is a
derived result that can be deleted and recomputed at any time.

## Scan detection (`auto`)

A PDF page counts as born-digital when its text layer carries at least
`auto_min_text_chars_per_page` non-whitespace characters. The document is
treated as a scan when **fewer than half** its pages clear that bar, which
tolerates a title page or a full-page figure inside an otherwise digital
document. Character counting is deliberate: no word lists, no
language-specific signals, so it behaves the same for every script.

## Caching

Every completed run is stored in `ocr_derivatives` (migration
`0054_ocr_derivatives.sql`), keyed by

| part | why it is in the key |
| --- | --- |
| `doc_sha256` | the document bytes — the same file uploaded twice costs one run |
| `model` | a different OCR model / revision reads differently |
| `prompt_version` | bumped in code when the parsing prompt changes meaning |
| `settings_key` | digest of prompt text, `max_tokens`, `ngram_window`, `max_pages`, `dpi`, `max_output_chars` |

Consequences worth knowing:

- A cache hit costs one indexed `SELECT`, emits **no** usage row (nothing was
  called), and survives a gateway restart.
- A **failed** row is kept — the operator needs the reason — but reads as a
  miss, so a transient backend failure retries on the next turn instead of
  poisoning the document forever.
- Operational settings (`max_concurrency`, `timeout_secs`, `max_bytes`) are
  *not* in the key: they don't change the recognised text.
- The row is a cache, not a lock. Two turns racing on the same document both
  run; the concurrency gate keeps the cost bounded.

## Status in the chat UI

Each document gets a `document_ocr` row in the turn's activity list, which is
how the existing tool-call UI renders `queued → running → completed | failed`
with a spinner / check / alert and an expandable detail panel. The model never
sees that row: it is not in the upstream message list, and no tool by that name
exists.

- queued behind the concurrency gate → an info banner names the file
- completed → page tally, `truncated`, `cached`, character count
- failed → the error, plus a note that the upload is unchanged and still
  readable through `fetch_attachment`

A failure never fails the turn.

## Limits

All under `[chat.ocr]`; see `gateway.example.toml` for the defaults.

| key | bounds |
| --- | --- |
| `max_bytes` | largest document accepted; bigger uploads are left to the normal attachment path |
| `max_pages` | pages recognised per document — a longer document is reported as partial, not refused |
| `dpi` | PDF rasterisation DPI (part of the cache key) |
| `max_output_chars` | ceiling on recognised text kept per document |
| `timeout_secs` | wall clock for one document, rasterisation included |
| `max_concurrency` | documents in flight gateway-wide |
| `auto_min_text_chars_per_page` | the scan detector's threshold |

`pages_total` vs `pages_processed` is how a caller learns whether the whole
document was read; when they differ, the injected block's header says so, so a
model that got 8 of 40 pages cannot answer as if it read the document.

## Usage accounting

One `usage_events` row per upstream OCR call, `kind = "ocr"`, with the sidecar's
token counts and `input_units` = pages processed. Failures are recorded too
(with the upstream status, or `0` when no answer arrived), so a broken OCR
backend is visible in the dashboards rather than only in the logs. Cache hits
are not recorded — nothing was called.

## Sidecar contract

The `ocr` upstream is an internal document-aware sidecar
(`deploy/ocr-sidecar`), intentionally **not** the raw vLLM OpenAI endpoint:
that endpoint takes image content parts, not `application/pdf`.

```text
POST <backend base URL>/ocr
Content-Type: multipart/form-data

file         original document bytes, with the original filename and MIME type
model        Unlimited-OCR model id
prompt       document parsing prompt
max_tokens   output limit per inference call
ngram_window 128 for one image, 1024 for multi-page input
max_pages    page ceiling for this document
dpi          rasterisation DPI
```

The sidecar exposes `GET /healthz` for the backend health path. Configure the
gateway backend with `health_path = "/healthz"`, `probe_models = false`, and
`baidu/Unlimited-OCR` as its static model — the sidecar is not a
model-discovery endpoint.

Responses come in two shapes. Per-page (what the shipped sidecar returns, and
the reason page numbers survive):

```json
{
  "pages": [{"page": 1, "markdown": "…"}, {"page": 2, "markdown": "…"}],
  "pages_total": 2,
  "pages_processed": 2,
  "failed_pages": [],
  "usage": {"prompt_tokens": 900, "completion_tokens": 400}
}
```

or flat, for a single image or a sidecar doing one multi-image call:

```json
{"markdown": "# Extracted document\n\n…", "pages_total": 1, "pages_processed": 1}
```

The gateway sorts page blocks by page number before assembling them — a sidecar
that recognises pages concurrently may answer out of order — and prefixes each
with `--- page N ---`. Missing page numbers keep their arrival order; empty
pages are dropped from the text but still count as unprocessed.

Errors: `400` for a bad request (wrong shape, unsupported type, empty
document), `502` when the model backend failed. Both surface to the operator
with the sidecar's message.

The sidecar owns PDF-to-image conversion (PyMuPDF, as the official
`infer.py --pdf` workflow does) and all model-specific inference. It must start
vLLM with

```text
--logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor
```

pass `skip_special_tokens=false` plus the model's n-gram parameters, and strip
`<|det|>` coordinate blocks / unwrap `<|ref|>` tokens before answering. (The
gateway strips them again — defence against a sidecar that forgets.)

By default the sidecar issues **one inference call per page**, which is what
gives real page numbers and tolerates a single page failing. `OCR_MULTI_IMAGE=1`
switches to one multi-image call per document: cheaper, but the answer has no
page structure.
