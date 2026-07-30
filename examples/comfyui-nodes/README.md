# ComfyUI custom nodes

Nodes the gateway's workflow catalog depends on and that no upstream node pack
provides in a form we want to depend on. Same deal as
[`../comfyui-workflows/`](../comfyui-workflows/): this directory is the source
of truth, and an operator copies it into their ComfyUI image at build time.

Everything here is ours (AGPL-3.0, like the rest of the repo). That is
deliberate — a node in the ComfyUI image runs with the same privileges as
ComfyUI itself, and several of the third-party TTS packs are either unlicensed
or bundle a dozen engines we do not use.

## `llmgw_qwen3_tts` — speech from text

[Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS) (Apache-2.0), the model behind
the `comfyui_text_to_speech` tool. Three ways to pick a voice:

| `mode` | Voice comes from | Notes |
| --- | --- | --- |
| `preset` | one of nine built-in timbres | none of them is a native German speaker |
| `design` | a prose description in `instruct` | best route to a German voice |
| `clone` | a few seconds of `reference_audio` | needs consent from the person |

`instruct` doubles as the direction for a preset voice ("ruhig, mit klarer
Betonung") and as the whole voice description in `design` mode.

### Installing into a ComfyUI image

The node needs an isolated venv, because `qwen-tts` pins
`transformers==4.57.3` while ComfyUI's environment runs a newer major version
that its other nodes need. `install-venv.sh` builds that venv with *only* the
pinned packages in it; torch, torchaudio, onnxruntime and soundfile are
inherited from the image at runtime, so nothing heavy is duplicated.

```dockerfile
COPY llmgw_qwen3_tts /app/custom_nodes/llmgw_qwen3_tts
RUN /app/custom_nodes/llmgw_qwen3_tts/install-venv.sh
```

Checkpoints download on first use into `<models_dir>/Qwen3-TTS/.hf` (~4.5 GB
per variant for the 1.7B models, ~2.5 GB for 0.6B) — mount that directory
read-write, or pre-seed it and mount it read-only.

### Runtime knobs

| Env var | Default | Purpose |
| --- | --- | --- |
| `LLMGW_QWEN3_TTS_PYTHON` | `/opt/qwen-tts/bin/python` | interpreter of the isolated venv |
| `LLMGW_QWEN3_TTS_HOME` | `<models_dir>/Qwen3-TTS/.hf` | checkpoint cache (`HF_HOME`) |
| `LLMGW_QWEN3_TTS_DEVICE` | `cuda:0` | device the worker loads onto |
| `LLMGW_QWEN3_TTS_IDLE_SECS` | `300` | idle time before the worker exits and frees its VRAM (`0` = never) |
| `LLMGW_QWEN3_TTS_TIMEOUT` | `900` | ceiling on one synthesis before the worker is killed |

The idle timeout is the point of the design: the checkpoint stays loaded while
someone iterates on a script, then the whole ~4.5 GB goes back to the GPU
without an operator having to think about it.
