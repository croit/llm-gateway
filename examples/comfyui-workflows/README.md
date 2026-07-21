# Example ComfyUI workflow catalog

This directory ships **example** workflow bundles an operator can drop into the private `[comfyui] content_dir`. The catalog itself is **not** part of the public config — operators copy what they want into their private content dir (or author their own) and back it up out of band. See [`docs/comfyui.md`](../../docs/comfyui.md) for the full story.

## Naming convention

Each bundle lives under `llmgw-<verb>/`. The `llmgw-` prefix marks the directory as belonging to the llm-gateway at a glance (vs. an unrelated operator-dropped bundle). Inside the manifest:

- `id = "<verb>"` (no `llmgw_` prefix) → tool id becomes `comfyui_<verb>` (e.g. `comfyui_text_to_image`). The `comfyui_` prefix is the gateway-side namespace.
- `output_filename_prefix = "llmgw-<verb>"` → ComfyUI stamps every produced file with the prefix, so when the operator looks at ComfyUI's `/history/{id}` or the worker's `output/` directory, every file from a gateway run is unambiguously attributed: `llmgw-text2image_00001_.png`, `llmgw-image2video_00002_.webp`, `llmgw-text2music_00003_.wav`, etc.

## Bundle layout

Each subdirectory holds:

```
llmgw-text2image/
  manifest.toml     # the tool surface the model sees
  workflow.json     # ComfyUI prompt-API document, with {{placeholders}}
```

The manifest's `id` becomes the tool name `comfyui_<id>`. Anything not declared in the manifest (model paths, weight dtype, sampler defaults, mask-blur radius, …) is invisible to the LLM and stays operator-curated in `workflow.json`. **The model never picks the model** — operators pin it in the workflow.

## Bundles

| Bundle | Tool id | Output | What it does |
|---|---|---|---|
| [`llmgw-text2image`](llmgw-text2image/) | `comfyui_text_to_image` | image | Text → image (FLUX.2 Klein fp8) |
| [`llmgw-edit-image`](llmgw-edit-image/) | `comfyui_edit_image` | image | Image + prompt → edited image (Flux.2 Dev fp8 img2img) |
| [`llmgw-merge-images`](llmgw-merge-images/) | `comfyui_merge_images` | image | Two images + prompt → blended image (Qwen Image Edit 2511 multi-reference) |
| [`llmgw-image2video`](llmgw-image2video/) | `comfyui_image_to_video` | video | Image + motion prompt → short clip (Wan 2.2 14B fp8) |
| [`llmgw-merge-video-audio`](llmgw-merge-video-audio/) | `comfyui_merge_video_audio` | video | Existing video + audio → MP4 with sound (VideoHelperSuite) |
| [`llmgw-talking-video`](llmgw-talking-video/) | `comfyui_talking_video` | video | Portrait + audio → talking-head clip with lip sync (LTX-2.3) |
| [`llmgw-text2music`](llmgw-text2music/) | `comfyui_text_to_music` | audio | Text → music clip (ACE-Step 1.5XL Turbo) |
| [`llmgw-upscale-image`](llmgw-upscale-image/) | `comfyui_upscale_image` | image | Image → upscaled image with restored detail (SeedVR2 7B Int8) |

## `workflow.json` are skeletons — operators must validate them

The `workflow.json` files in this catalog are **reference skeletons** built from the ComfyUI templates the gateway authors observed. They wire the parameters the manifest declares to the right `(node_id, input_key)` targets, but they are **not** load-tested against a specific ComfyUI version or custom-node set. Operators must:

1. Open ComfyUI's template (the comment at the top of each `workflow.json` names the exact template).
2. Switch to **API format** (`Workflow → Export ... → API format`).
3. Replace the skeleton's body with the exported JSON, keeping the `(node_id, input_key)` targets the manifest references. The simplest way is to rename the export's nodes to match the manifest's `node_id` strings (e.g. `SaveImage`, `LoadImage`, `KSampler`).
4. Adjust model filenames to match what's actually installed under `models/` on the worker.

The gateway substitutes `{{placeholders}}` and stamps the `output_filename_prefix` onto the output node — everything else stays exactly as the operator authored it.

## Adding new bundles

1. Create a subdirectory `llmgw-<verb>/`.
2. Write `manifest.toml` (use the existing ones as template — every parameter needs a description, because the model reads it verbatim).
3. Write `workflow.json` (export from ComfyUI, parameterise the slots the manifest names).
4. Drop the directory into `[comfyui] content_dir` and hit "Reload catalog" on `/admin/comfyui` — no restart.
