# Document OCR

The gateway's `ocr` upstream is an internal document-aware sidecar contract.
It is intentionally not the raw vLLM OpenAI endpoint, because that endpoint
accepts image content parts rather than `application/pdf`.

## Sidecar contract

```text
POST <backend base URL>/ocr
Content-Type: multipart/form-data

file        original document bytes, with the original filename and MIME type
model       Unlimited-OCR model id
prompt      document parsing prompt
max_tokens  output limit
ngram_window 128 for one image, 1024 for multi-page documents
```

The sidecar exposes `GET /healthz` for the gateway backend health path. Configure
the gateway backend with `health_path = "/healthz"`, `probe_models = false`,
and list `baidu/Unlimited-OCR` as its static model. The sidecar is not itself a
model-discovery endpoint.

The response is JSON with either `markdown` or `text`:

```json
{"markdown":"# Extracted document\n\n..."}
```

The sidecar owns PDF-to-image conversion. Its implementation may call the
official `infer.py --pdf` workflow, which uses PyMuPDF internally and then
calls Unlimited-OCR multi-image inference. It must configure the vLLM server
with:

```text
--logits_processors vllm.model_executor.models.unlimited_ocr:NGramPerReqLogitsProcessor
```

and pass `skip_special_tokens=false` plus the model's n-gram parameters to the
actual vLLM request. The sidecar should remove `<|det|>` coordinate blocks and
unwrap `<|ref|>` tokens before returning Markdown.

The original upload remains in the gateway's attachment store. OCR output is a
derived result and must not replace or mutate the source document.
