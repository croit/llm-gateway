#!/usr/bin/env python3
"""PDF-aware Unlimited-OCR sidecar for the gateway's internal /ocr contract.

The gateway posts the original document; this service owns everything
model-specific: rasterising PDF pages with PyMuPDF (what the upstream
`infer.py --pdf` wrapper does internally), calling the Unlimited-OCR vLLM
endpoint, and cleaning the grounding tokens out of the answer.

Pages are recognised **one per request** by default, which is what makes the
response carry real page numbers: the gateway assembles them in page order, and
a document whose page 7 fails still returns pages 1-6 with an honest
`pages_processed` tally. Set `OCR_MULTI_IMAGE=1` to send every page in one
request instead (Unlimited-OCR's multi-image mode) -- cheaper per document, but
the answer is then one undifferentiated blob.
"""

import base64
import email.policy
import json
import os
import sys
import urllib.error
import urllib.request
from email.parser import BytesParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import fitz

# Ceiling on the multipart body we will read at all. The gateway enforces its
# own (smaller, configurable) limit first; this one exists so a stray client
# can't make the sidecar allocate without bound.
MAX_UPLOAD_BYTES = int(os.environ.get("OCR_MAX_UPLOAD_BYTES", 64 * 1024 * 1024))
# Fallbacks for the per-request fields the gateway normally supplies.
DEFAULT_MAX_PAGES = int(os.environ.get("OCR_MAX_PAGES", 64))
DEFAULT_DPI = int(os.environ.get("OCR_DPI", 300))
# Hard ceilings a request cannot raise: a caller asking for 10000 pages at
# 1200 DPI would exhaust the box.
PAGE_CEILING = int(os.environ.get("OCR_PAGE_CEILING", 512))
DPI_CEILING = int(os.environ.get("OCR_DPI_CEILING", 600))
# One request per page, or one request for all pages. Per-page is the default:
# it is what gives the gateway page numbers and partial-failure tolerance.
MULTI_IMAGE = os.environ.get("OCR_MULTI_IMAGE", "0") == "1"
# Unlimited-OCR's repetition-control window. The model card calls for 128 with a
# single image and 1024 for multi-page input.
SINGLE_IMAGE_NGRAM_WINDOW = 128
MULTI_IMAGE_NGRAM_WINDOW = 1024
VLLM_BASE_URL = os.environ.get("VLLM_BASE_URL", "http://127.0.0.1:8000/v1").rstrip("/")
VLLM_API_KEY = os.environ.get("VLLM_API_KEY", "")
VLLM_TIMEOUT = int(os.environ.get("OCR_VLLM_TIMEOUT", 1200))
DEFAULT_MODEL = os.environ.get("OCR_MODEL", "baidu/Unlimited-OCR")
DEFAULT_PROMPT = "Document parsing."


def json_response(handler, status, payload):
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def clean_grounding_tokens(content):
    cleaned = []
    rest = content
    while "<|det|>" in rest:
        before, rest = rest.split("<|det|>", 1)
        cleaned.append(before)
        if "<|/det|>" not in rest:
            rest = ""
            break
        _, rest = rest.split("<|/det|>", 1)
    cleaned.append(rest)
    return "".join(cleaned).replace("<|ref|>", "").replace("<|/ref|>", "")


def parse_multipart(content_type, body):
    header = ("Content-Type: " + content_type + "\r\nMIME-Version: 1.0\r\n\r\n").encode(
        "ascii"
    )
    message = BytesParser(policy=email.policy.default).parsebytes(header + body)
    if not message.is_multipart():
        raise ValueError("request must be multipart/form-data")
    fields = {}
    for part in message.iter_parts():
        name = part.get_param("name", header="content-disposition")
        if not name:
            continue
        payload = part.get_payload(decode=True) or b""
        if part.get_filename():
            fields[name] = (part.get_filename(), part.get_content_type(), payload)
        else:
            fields[name] = payload.decode("utf-8")
    return fields


def clamp_int(fields, name, default, ceiling):
    """Read a positive integer form field, clamped to a hard ceiling."""
    raw = fields.get(name)
    try:
        value = int(raw) if raw not in (None, "") else default
    except (TypeError, ValueError):
        value = default
    return max(1, min(value, ceiling))


def document_pages(filename, mime, data, max_pages, dpi):
    """Rasterise a PDF, or pass a single image through.

    Returns (pages, total_pages) where `pages` is a list of
    (page_number, mime, bytes). A document longer than `max_pages` is
    truncated rather than refused: a partial answer plus an honest page tally
    beats no answer at all for a 300-page scan.
    """
    if mime == "application/pdf" or filename.lower().endswith(".pdf"):
        document = fitz.open(stream=data, filetype="pdf")
        total = len(document)
        matrix = fitz.Matrix(dpi / 72, dpi / 72)
        pages = []
        for number, page in enumerate(document, start=1):
            if number > max_pages:
                break
            pixmap = page.get_pixmap(matrix=matrix, alpha=False)
            pages.append((number, "image/png", pixmap.tobytes("png")))
        document.close()
        return pages, total
    if not mime.startswith("image/"):
        raise ValueError("OCR sidecar accepts PDF and image uploads")
    return [(1, mime, data)], 1


def call_vllm(model, prompt, images, max_tokens, ngram_window):
    """One vLLM request over `images` (a list of (mime, bytes)).

    `skip_special_tokens=False` and the n-gram parameters are required by the
    model card; the server must also be started with
    `--logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor`.
    """
    content = [{"type": "text", "text": "<image>" + prompt}]
    for mime, data in images:
        encoded = base64.b64encode(data).decode("ascii")
        content.append(
            {
                "type": "image_url",
                "image_url": {"url": f"data:{mime};base64,{encoded}"},
            }
        )
    payload = {
        "model": model or DEFAULT_MODEL,
        "messages": [{"role": "user", "content": content}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "skip_special_tokens": False,
        "vllm_xargs": {"ngram_size": 35, "window_size": ngram_window},
    }
    request = urllib.request.Request(
        VLLM_BASE_URL + "/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            **({"Authorization": f"Bearer {VLLM_API_KEY}"} if VLLM_API_KEY else {}),
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=VLLM_TIMEOUT) as response:
            result = json.loads(response.read())
    except (urllib.error.URLError, TimeoutError) as error:
        raise RuntimeError(f"vLLM request failed: {error}") from error
    content = result.get("choices", [{}])[0].get("message", {}).get("content", "")
    if not content:
        raise RuntimeError("vLLM response contains no OCR text")
    return clean_grounding_tokens(content), result.get("usage") or {}


def accumulate_usage(total, usage):
    """Sum token counts across the per-page calls of one document."""
    for key in ("prompt_tokens", "completion_tokens", "total_tokens"):
        value = usage.get(key)
        if isinstance(value, int):
            total[key] = total.get(key, 0) + value
    return total


def recognise(fields):
    filename, mime, document = fields["file"]
    max_pages = clamp_int(fields, "max_pages", DEFAULT_MAX_PAGES, PAGE_CEILING)
    dpi = clamp_int(fields, "dpi", DEFAULT_DPI, DPI_CEILING)
    max_tokens = clamp_int(fields, "max_tokens", 32768, 1_000_000)
    model = fields.get("model") or DEFAULT_MODEL
    prompt = fields.get("prompt") or DEFAULT_PROMPT

    pages, total = document_pages(filename, mime, document, max_pages, dpi)
    if not pages:
        raise ValueError("document has no pages to recognise")

    usage = {}
    if len(pages) == 1:
        window = clamp_int(
            fields, "ngram_window", SINGLE_IMAGE_NGRAM_WINDOW, 1_000_000
        )
        text, page_usage = call_vllm(
            model, prompt, [(pages[0][1], pages[0][2])], max_tokens, window
        )
        return {
            "pages": [{"page": pages[0][0], "markdown": text}],
            "pages_total": total,
            "pages_processed": 1,
            "usage": accumulate_usage(usage, page_usage),
        }

    if MULTI_IMAGE:
        # One multi-image call: cheaper, but the answer has no page structure,
        # so it goes back as a flat document.
        window = clamp_int(
            fields, "ngram_window", MULTI_IMAGE_NGRAM_WINDOW, 1_000_000
        )
        text, page_usage = call_vllm(
            model, prompt, [(m, d) for _, m, d in pages], max_tokens, window
        )
        return {
            "markdown": text,
            "pages_total": total,
            "pages_processed": len(pages),
            "usage": accumulate_usage(usage, page_usage),
        }

    # Per-page inference. A page that fails is reported rather than failing the
    # document: the gateway then shows how many of how many pages were read.
    results = []
    failed = []
    for number, page_mime, data in pages:
        try:
            text, page_usage = call_vllm(
                model,
                prompt,
                [(page_mime, data)],
                max_tokens,
                SINGLE_IMAGE_NGRAM_WINDOW,
            )
        except RuntimeError as error:
            print(f"page {number} failed: {error}", file=sys.stderr, flush=True)
            failed.append(number)
            continue
        accumulate_usage(usage, page_usage)
        results.append({"page": number, "markdown": text})
    if not results:
        raise RuntimeError(f"every page failed OCR (pages 1-{len(pages)})")
    return {
        "pages": results,
        "pages_total": total,
        "pages_processed": len(results),
        "failed_pages": failed,
        "usage": usage,
    }


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        if self.path == "/healthz":
            json_response(self, 200, {"status": "ok"})
        else:
            json_response(self, 404, {"error": "not found"})

    def do_POST(self):  # noqa: N802
        if self.path != "/ocr":
            json_response(self, 404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > MAX_UPLOAD_BYTES:
                raise ValueError(
                    f"request body must be between 1 and {MAX_UPLOAD_BYTES} bytes"
                )
            body = self.rfile.read(length)
            fields = parse_multipart(self.headers.get("Content-Type", ""), body)
            json_response(self, 200, recognise(fields))
        except (KeyError, ValueError) as error:
            # A bad request: wrong shape, unsupported type, empty document.
            json_response(self, 400, {"error": str(error)})
        except RuntimeError as error:
            # The model backend failed. 502 so the gateway's error message says
            # "upstream" and an operator can tell the two apart.
            json_response(self, 502, {"error": str(error)})

    def log_message(self, format, *args):
        print(format % args, file=sys.stderr, flush=True)


if __name__ == "__main__":
    host = os.environ.get("OCR_BIND", "0.0.0.0")
    port = int(os.environ.get("OCR_PORT", "9100"))
    ThreadingHTTPServer((host, port), Handler).serve_forever()
