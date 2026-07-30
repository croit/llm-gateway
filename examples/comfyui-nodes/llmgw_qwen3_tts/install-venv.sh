#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
#
# Build the isolated venv this node's worker runs in. Meant to be called from
# a ComfyUI image build (see README.md), but it is safe to run inside a live
# container too — it writes only to $VENV.
#
# Why a venv: `qwen-tts` pins `transformers==4.57.3`. ComfyUI's environment
# ships a newer major version that its other custom nodes need, and
# `transformers` has no built-in Qwen3-TTS model to fall back on, so the pin
# has to live somewhere else. Only the pinned packages are installed here;
# torch, torchaudio, onnxruntime and soundfile are inherited from the image at
# runtime (the node passes its own sys.path to the worker).
set -euo pipefail

VENV=${VENV:-/opt/qwen-tts}
# Any interpreter of the same Python minor version as ComfyUI's works; the
# inherited packages are C extensions built for that ABI.
BASE_PYTHON=${BASE_PYTHON:-python3}
QWEN_TTS_VERSION=${QWEN_TTS_VERSION:-0.1.1}
TRANSFORMERS_VERSION=${TRANSFORMERS_VERSION:-4.57.3}

"$BASE_PYTHON" -m venv "$VENV"

# `librosa` and `sox` are qwen-tts imports; `--no-deps` on qwen-tts itself
# keeps its `gradio` demo dependency (and a second torch) out of the image.
"$VENV/bin/pip" install --no-cache-dir --upgrade pip
"$VENV/bin/pip" install --no-cache-dir \
    "transformers==${TRANSFORMERS_VERSION}" \
    librosa \
    sox
"$VENV/bin/pip" install --no-cache-dir --no-deps "qwen-tts==${QWEN_TTS_VERSION}"

# Fail the build here rather than at the first tool call if the pin drifted.
"$VENV/bin/python" - <<'PY'
import transformers

assert transformers.__version__.startswith("4.57."), transformers.__version__
print("qwen-tts venv ready:", transformers.__version__)
PY
