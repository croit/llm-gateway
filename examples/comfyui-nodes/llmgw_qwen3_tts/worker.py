#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
"""Qwen3-TTS synthesis worker — the half that runs in the isolated venv.

Why a subprocess at all: `qwen-tts` hard-pins `transformers==4.57.3`, while
ComfyUI's own environment runs a much newer major version, and `transformers`
carries no `qwen3_tts` model of its own to fall back on. Installing the pin
into ComfyUI would down-grade the library every other custom node depends on,
so the pinned copy lives in its own venv and is reached across a process
boundary instead.

The venv is deliberately *thin*: only `transformers`, `librosa`, `sox` and
`qwen-tts` are installed into it. Everything heavy (torch, torchaudio,
onnxruntime, soundfile) is inherited from the ComfyUI image by appending the
parent interpreter's `sys.path` — passed in via `LLMGW_PARENT_SYS_PATH` rather
than hardcoded, because those packages live in three different directories in
that image and which one holds what is not guessable. The append happens
*after* the venv's own site-packages so the version pin still wins.

Protocol: one JSON request per line on stdin, one JSON response per line on a
private duplicate of stdout. Requests are served sequentially; the loaded
checkpoint is cached between them, and swapped only when a request needs a
different one (each is ~4.5 GB of VRAM, so exactly one is resident at a time).
Diagnostics go to stderr, which ComfyUI shows in its log.
"""

import json
import os
import sys
import tempfile
import traceback

# Take the protocol channel private BEFORE importing anything heavy. `qwen-tts`
# and its dependencies print banners to stdout on import ("Warning: flash-attn
# is not installed…"), which would land in the middle of the JSON stream and
# leave the caller parsing a library notice. So: keep a duplicate of the real
# stdout for responses, then point fd 1 at stderr so every stray print becomes
# a log line instead of protocol corruption.
_PROTO = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr

# Inherit the image's torch/onnxruntime/soundfile — appended, never prepended,
# so this venv's pinned transformers is the one that gets imported.
for entry in os.environ.get("LLMGW_PARENT_SYS_PATH", "").split(os.pathsep):
    if entry and entry not in sys.path:
        sys.path.append(entry)

import soundfile as sf  # noqa: E402
import torch  # noqa: E402

from qwen_tts import Qwen3TTSModel  # noqa: E402

# One repo per mode: cloning needs the base model, preset speakers the
# CustomVoice fine-tune, a described voice the VoiceDesign one.
REPOS = {
    "preset": "Qwen/Qwen3-TTS-12Hz-{size}-CustomVoice",
    "design": "Qwen/Qwen3-TTS-12Hz-{size}-VoiceDesign",
    "clone": "Qwen/Qwen3-TTS-12Hz-{size}-Base",
}

_loaded: "tuple[str, Qwen3TTSModel] | None" = None


def model_for(repo: str) -> Qwen3TTSModel:
    """The model for `repo`, reusing the resident one when it already matches.

    Swapping frees the previous weights first: two 1.7B checkpoints would not
    fit beside ComfyUI's own video models on a busy GPU.
    """
    global _loaded
    if _loaded is not None and _loaded[0] == repo:
        return _loaded[1]
    if _loaded is not None:
        print(f"unloading {_loaded[0]}", file=sys.stderr, flush=True)
        _loaded = None
        torch.cuda.empty_cache()
    device = os.environ.get("LLMGW_QWEN3_TTS_DEVICE", "cuda:0")
    print(f"loading {repo} on {device}", file=sys.stderr, flush=True)
    model = Qwen3TTSModel.from_pretrained(
        repo,
        device_map=device,
        dtype=torch.bfloat16,
    )
    _loaded = (repo, model)
    return model


def synthesize(req: dict) -> dict:
    mode = req.get("mode", "preset")
    if mode not in REPOS:
        raise ValueError(f"unknown mode `{mode}` (expected one of {sorted(REPOS)})")
    text = (req.get("text") or "").strip()
    if not text:
        raise ValueError("`text` is empty — nothing to speak")
    repo = REPOS[mode].format(size=req.get("size", "1.7B"))
    model = model_for(repo)

    # A fixed seed makes a take reproducible, which is what lets a caller tune
    # the wording of a line without the voice drifting underneath them.
    seed = int(req.get("seed", -1))
    if seed >= 0:
        torch.manual_seed(seed)

    language = req.get("language") or "Auto"
    instruct = (req.get("instruct") or "").strip()
    if mode == "preset":
        kwargs = {"speaker": req.get("speaker") or "Ryan"}
        if instruct:
            kwargs["instruct"] = instruct
        wavs, sr = model.generate_custom_voice(text=text, language=language, **kwargs)
    elif mode == "design":
        if not instruct:
            raise ValueError("mode `design` needs `instruct` — the voice description")
        wavs, sr = model.generate_voice_design(
            text=text, language=language, instruct=instruct
        )
    else:
        ref_audio = req.get("reference_audio")
        if not ref_audio:
            raise ValueError("mode `clone` needs `reference_audio`")
        kwargs = {"ref_audio": ref_audio}
        # The transcript of the reference is optional: without it the model
        # falls back to a speaker embedding, which clones timbre but not
        # prosody as closely.
        if req.get("reference_text"):
            kwargs["ref_text"] = req["reference_text"]
        else:
            kwargs["x_vector_only_mode"] = True
        wavs, sr = model.generate_voice_clone(text=text, language=language, **kwargs)

    out_dir = req.get("out_dir") or tempfile.gettempdir()
    fd, path = tempfile.mkstemp(prefix="llmgw-tts-", suffix=".wav", dir=out_dir)
    os.close(fd)
    sf.write(path, wavs[0], sr)
    return {"ok": True, "path": path, "sample_rate": int(sr)}


def main() -> None:
    # Line-buffered request loop. EOF (the node closed our stdin, or it exited)
    # ends the worker, which is also how the idle timer reclaims the VRAM.
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            resp = synthesize(json.loads(line))
        except Exception as err:  # noqa: BLE001 — every failure is reportable
            traceback.print_exc()
            resp = {"ok": False, "error": f"{type(err).__name__}: {err}"}
        _PROTO.write(json.dumps(resp) + "\n")
        _PROTO.flush()


if __name__ == "__main__":
    main()
