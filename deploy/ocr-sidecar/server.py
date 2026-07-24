#!/usr/bin/env python3
"""PDF-aware Unlimited-OCR sidecar for the gateway's internal /ocr contract."""

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

MAX_UPLOAD_BYTES = 64 * 1024 * 1024
MAX_PAGES = 64
PDF_DPI = 300
VLLM_BASE_URL = os.environ.get("VLLM_BASE_URL", "http://127.0.0.1:8000/v1").rstrip("/")
VLLM_API_KEY = os.environ.get("VLLM_API_KEY", "")
DEFAULT_MODEL = os.environ.get("OCR_MODEL", "baidu/Unlimited-OCR")


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
    header = (
        "Content-Type: "
        + content_type
        + "\r\nMIME-Version: 1.0\r\n\r\n"
    ).encode("ascii")
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


def pdf_pages(filename, mime, data):
    if mime == "application/pdf" or filename.lower().endswith(".pdf"):
        document = fitz.open(stream=data, filetype="pdf")
        if len(document) > MAX_PAGES:
            raise ValueError(f"PDF has {len(document)} pages; limit is {MAX_PAGES}")
        matrix = fitz.Matrix(PDF_DPI / 72, PDF_DPI / 72)
        pages = []
        for page in document:
            pixmap = page.get_pixmap(matrix=matrix, alpha=False)
            pages.append(("image/png", pixmap.tobytes("png")))
        document.close()
        return pages
    if not mime.startswith("image/"):
        raise ValueError("OCR sidecar accepts PDF and image uploads")
    return [(mime, data)]


def call_vllm(model, prompt, pages, max_tokens, ngram_window):
    content = [{"type": "text", "text": "<image>" + prompt}]
    for mime, data in pages:
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
        "max_tokens": int(max_tokens or 32768),
        "temperature": 0,
        "skip_special_tokens": False,
        "vllm_xargs": {"ngram_size": 35, "window_size": int(ngram_window or 1024)},
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
        with urllib.request.urlopen(request, timeout=1200) as response:
            result = json.loads(response.read())
    except (urllib.error.URLError, TimeoutError) as error:
        raise RuntimeError(f"vLLM request failed: {error}") from error
    content = result.get("choices", [{}])[0].get("message", {}).get("content", "")
    if not content:
        raise RuntimeError("vLLM response contains no OCR text")
    return clean_grounding_tokens(content)


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
                raise ValueError(f"request body must be between 1 and {MAX_UPLOAD_BYTES} bytes")
            body = self.rfile.read(length)
            fields = parse_multipart(self.headers.get("Content-Type", ""), body)
            filename, mime, document = fields["file"]
            pages = pdf_pages(filename, mime, document)
            markdown = call_vllm(
                fields.get("model", DEFAULT_MODEL),
                fields.get("prompt", "Document parsing."),
                pages,
                fields.get("max_tokens", "32768"),
                fields.get("ngram_window", "1024" if len(pages) > 1 else "128"),
            )
            json_response(self, 200, {"markdown": markdown, "pages": len(pages)})
        except (KeyError, ValueError, RuntimeError) as error:
            json_response(self, 400, {"error": str(error)})

    def log_message(self, format, *args):
        print(format % args, file=sys.stderr, flush=True)


if __name__ == "__main__":
    host = os.environ.get("OCR_BIND", "0.0.0.0")
    port = int(os.environ.get("OCR_PORT", "9100"))
    ThreadingHTTPServer((host, port), Handler).serve_forever()
