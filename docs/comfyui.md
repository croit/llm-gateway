# ComfyUI integration

The gateway talks to **ComfyUI** as a headless inference engine for image, video, and audio workflows. Users never see ComfyUI; the gateway exposes curated workflows as tools the model can call, each with a typed, described parameter surface.

This doc captures the design and operator model. For the chat/UI side of tool calls see [`tools-rbac.md`](tools-rbac.md); for the broader server layout see [`architecture.md`](architecture.md).

## Why headless ComfyUI

ComfyUI already solves the hard part of GPU inference: model loading, VRAM accounting, a node ecosystem that supports FLUX, WAN, LTX, LatentSync, MuseTalk, Whisper, and friends. Reimplementing that in the gateway would be a waste.

The cost is that ComfyUI is a workflow engine, not a product API. So the gateway:

- owns the **curated workflow catalog** (versioned JSON + manifests),
- exposes a **small typed parameter surface per workflow** to the model,
- hides everything else (model paths, sampler internals, weight dtype, VRAM strategy) as operator config,
- validates inputs, runs the workflow via ComfyUI's HTTP API, fetches the result, re-hosts it in the gateway's attachment store, and returns a concise metadata blob to the model.

## Topology

```
browser → gateway (OpenAI-compatible API + chat UI + RBAC + tools)
                    ↓
              ComfyUI worker (HTTP, internal only)
                    ↓
              GPU + models (mounted read-only)
```

ComfyUI is **not** exposed publicly. The gateway reaches it at `base_url` from `[comfyui]`. Multiple gateway replicas can share one ComfyUI worker, but the worker itself serialises per GPU.

## Operator config

`[comfyui]` is optional. With no block, no ComfyUI tools register and the gateway boots fine.

```toml
[comfyui]
enabled = true
base_url = "http://comfyui-worker:8188"
content_dir = "/opt/llm-content"   # workflows + manifests live here
timeout_secs = 600                  # per workflow execution
queue_poll_interval_ms = 500        # /history poll cadence
max_concurrent_jobs = 1             # 24 GB VRAM realistically allows 1
```

The `content_dir` is **not** part of the public repo. It is a private, operator-managed directory holding workflows, manifests, and — at the operator's discretion — model files (or symlinks to a shared model volume). The gateway reads from it at startup; nothing in `content_dir` is ever written by the gateway or shipped to the browser.

## `content_dir` layout

One subdirectory per workflow. Each subdirectory holds:

```
/opt/llm-content/
  text-to-image/
    workflow.json      # ComfyUI prompt-API format
    manifest.toml      # tool id, description, params, output wiring
  image-to-video/
    workflow.json
    manifest.toml
  lip-sync/
    workflow.json
    manifest.toml
```

The subdirectory name is informational; `manifest.toml`'s `id` is what becomes the tool name (`comfyui_<id>`).

## `manifest.toml`

The manifest is the **only** surface the model sees. Anything not declared here is invisible to the LLM and controlled by the operator-curated `workflow.json`.

```toml
id = "text_to_image"                 # → tool name `comfyui_text_to_image`
title = "Text to Image"              # short label for the /tools UI
description = "Generate an image from a text prompt using FLUX.2 Klein."
output_kind = "image"                # image | video | audio | json
output_node_id = "9"                 # ComfyUI node id holding the result
output_filename_prefix = "comfyui-t2i"

# Each [[params]] entry becomes one property in the OpenAI tool schema.
# The gateway validates type, range, enum before dispatching to ComfyUI.

[[params]]
key = "prompt"                       # also the placeholder name in workflow.json
node_id = "6"                        # ComfyUI node to inject into
input_key = "text"                   # input field on that node
required = true
description = "What to draw. Be specific about subject, style, composition, colours."

[params.schema]
type = "string"

[[params]]
key = "width"
node_id = "5"
input_key = "width"
description = "Image width in pixels. Larger images use more VRAM and take longer."
default = 1024

[params.schema]
type = "integer"
min = 256
max = 2048

[[params]]
key = "sampler"
node_id = "3"
input_key = "sampler_name"
description = "Sampling algorithm. `euler` is fastest, `dpmpp_2m` smoother for photorealistic output."
default = "euler"

[params.schema]
type = "string"
enum_values = ["euler", "euler_ancestral", "dpmpp_2m", "dpmpp_sde"]

[[params]]
key = "seed"
node_id = "75:73"
input_key = "noise_seed"
description = "Random seed. Same prompt + seed = same image. Pass -1 for a fresh random seed each call."
default = -1

[params.schema]
type = "integer"
min = -1
# Replace a resolved value of -1 with a fresh random seed before dispatch
# (the conventional ComfyUI "seed = -1 → randomize" contract). Opt-in per
# param — the gateway never infers this from the parameter's name.
randomize_on_sentinel = true
```

### Parameter description rules

Every parameter carries a `description`. The model reads this verbatim; the gateway does not embellish. Descriptions must:

- explain **what changing this value does**, in plain English
- call out tradeoffs the model can't infer (VRAM cost, time cost, quality tradeoff)
- never repeat the parameter name back ("the width is the width of the image")
- for enums, describe each option if the names aren't self-explanatory
- for ranges, give a sense of typical values ("default 1024; rarely useful below 512")

A bad description: `"The width."`. A good description: `"Image width in pixels. Larger images use more VRAM and take longer."`.

## `workflow.json`

Plain ComfyUI prompt-API JSON. The operator exports it from ComfyUI's UI once, then parameterises it by replacing concrete input values with `{{param_name}}` placeholders. The gateway substitutes placeholders before sending.

Example (FLUX.2 Klein, abridged):

```json
{
  "3": {
    "class_type": "KSampler",
    "inputs": {
      "sampler_name": "{{sampler}}",
      "model": ["1", 0]
    }
  },
  "5": {
    "class_type": "EmptyLatentImage",
    "inputs": { "width": "{{width}}", "height": "{{height}}" }
  },
  "6": {
    "class_type": "CLIPTextEncode",
    "inputs": { "text": "{{prompt}}" }
  }
}
```

For inputs that don't map cleanly to a placeholder (e.g. node-id routing), `manifest.toml`'s `node_id` + `input_key` form takes precedence — the gateway writes the value directly into `workflow.json[<node_id>].inputs[<input_key>]` before dispatch.

## Execution flow

```
model emits tool_call(comfyui_text_to_image, { prompt, width, ... })
   ↓
gateway resolves tool → workflow manifest
   ↓
gateway validates args against manifest schemas (type, range, enum, required)
   ↓
gateway loads workflow.json, substitutes params
   ↓
gateway POSTs to {base_url}/prompt with {"prompt": <workflow>}
   ↓
gateway polls {base_url}/history/{prompt_id} until status terminal
   ↓
gateway fetches output from {base_url}/view?filename=...&subfolder=...&type=output
   ↓
gateway uploads bytes to chat attachments S3 bucket
   ↓
gateway splices [gw-attachment …] marker into assistant turn
   ↓
tool returns concise terminal metadata to model (no bytes)
   ↓
the normal tool loop wakes the LLM only after success or failure, so it can
continue with the next requested action
```

## RBAC

ComfyUI tools register like any other tool: per-role `tools = ["comfyui_text_to_image", ...]`. The operator grants workflows the same way they grant `typst_*` tools today. There is no special "comfyui" permission — it's just tools.

## Error surface

| Case | Behaviour |
|---|---|
| `[comfyui]` not configured, tool invoked | `ToolError::Failed("ComfyUI is not configured on this gateway")` |
| `content_dir` missing at startup | Gateway boots; no comfyui tools register; logged at WARN |
| Manifest parse error | That workflow is skipped; logged at ERROR; others register |
| ComfyUI HTTP error | `ToolError::Failed` with backend's status + body excerpt |
| Workflow timeout | `ToolError::Failed("ComfyUI workflow did not finish within {timeout_secs}s")` |
| Param validation failure | `ToolError::InvalidArgs` with field-level message |

## What lives where

| Concern | Lives in |
|---|---|
| Operator config (`base_url`, `content_dir`, timeouts) | `gateway.toml` `[comfyui]` |
| Workflow JSON (model paths, samplers, nodes) | `content_dir/<workflow>/workflow.json` |
| Tool surface (id, params, descriptions) | `content_dir/<workflow>/manifest.toml` |
| HTTP client + execution loop | `crates/gateway-core/src/server/comfyui/` |
| Tool registration | `crates/gateway-core/src/server/tools/comfyui_workflow.rs` (one tool impl, parameterised by manifest) |

## Roadmap

- **Phase 1 (this PR)**: `[comfyui]` config block, manifest loader, single-image-output workflow execution, `comfyui_<id>` tool registration, parameter substitution + validation.
- **Phase 2**: multi-output workflows (video + audio), longer-running job queue with progress events surfaced to chat UI.
- **Phase 3**: image-input workflows (edit, image-to-video), mask uploads.
- **Phase 4**: ComfyUI worker as a managed sidecar with health checks + version pinning.
