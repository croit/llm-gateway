# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
"""ComfyUI node: speech from text with Qwen3-TTS.

The node itself does no model work. It keeps a `worker.py` subprocess alive in
an isolated venv (see that file for why the process boundary exists) and talks
to it in JSON lines. Everything ComfyUI-facing — the AUDIO tensor, the input
sockets, the preset lists — is handled here, in ComfyUI's own environment.

VRAM behaviour, which is the reason this is a ComfyUI node and not a resident
service: the worker starts on the first synthesis, keeps the checkpoint loaded
while a caller iterates on a script, and is terminated after
`LLMGW_QWEN3_TTS_IDLE_SECS` of inactivity, which returns the whole ~4.5 GB to
the GPU. Nothing is pinned between jobs.
"""

import json
import os
import select
import subprocess
import sys
import tempfile
import threading

import numpy as np
import soundfile as sf
import torch

try:  # ComfyUI core — present at runtime, absent when unit-testing this file.
    import folder_paths
except ImportError:  # pragma: no cover
    folder_paths = None

HERE = os.path.dirname(os.path.abspath(__file__))

#: Built-in timbres. All nine are native to Chinese, English, Japanese or
#: Korean — none to German, so for German narration a described voice
#: (`mode = design`) or a cloned one usually sounds markedly better than a
#: preset speaking a foreign language.
PRESET_SPEAKERS = [
    "Ryan",
    "Aiden",
    "Vivian",
    "Serena",
    "Uncle_Fu",
    "Dylan",
    "Eric",
    "Ono_Anna",
    "Sohee",
]

#: The ten languages the model was trained on, plus `Auto` (detect from text).
LANGUAGES = [
    "Auto",
    "German",
    "English",
    "Chinese",
    "French",
    "Italian",
    "Japanese",
    "Korean",
    "Portuguese",
    "Russian",
    "Spanish",
]

MODES = ["preset", "design", "clone"]
SIZES = ["1.7B", "0.6B"]


def _venv_python() -> str:
    return os.environ.get("LLMGW_QWEN3_TTS_PYTHON", "/opt/qwen-tts/bin/python")


def _models_home() -> str:
    """Where the checkpoints are cached (`HF_HOME` for the worker).

    Under ComfyUI's models directory, so it lands on the same volume as every
    other model the deployment mounts read-write, rather than in root's home
    inside the container where a restart would lose it.
    """
    if env := os.environ.get("LLMGW_QWEN3_TTS_HOME"):
        return env
    base = folder_paths.models_dir if folder_paths else tempfile.gettempdir()
    return os.path.join(base, "Qwen3-TTS", ".hf")


class _Worker:
    """The synthesis subprocess, started on demand and reaped when idle."""

    def __init__(self) -> None:
        self._proc: subprocess.Popen | None = None
        self._lock = threading.Lock()
        self._timer: threading.Timer | None = None
        self._idle_secs = float(os.environ.get("LLMGW_QWEN3_TTS_IDLE_SECS", "300"))
        self._call_timeout = float(os.environ.get("LLMGW_QWEN3_TTS_TIMEOUT", "900"))

    def _start(self) -> subprocess.Popen:
        env = dict(os.environ)
        # Hand the worker our own import paths; it appends them after its
        # pinned site-packages. See worker.py for why this isn't hardcoded.
        env["LLMGW_PARENT_SYS_PATH"] = os.pathsep.join(p for p in sys.path if p)
        home = _models_home()
        os.makedirs(home, exist_ok=True)
        env["HF_HOME"] = home
        return subprocess.Popen(
            [_venv_python(), os.path.join(HERE, "worker.py")],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,  # inherit — tracebacks belong in the ComfyUI log
            env=env,
            text=True,
            bufsize=1,
        )

    def _touch_idle_timer(self) -> None:
        if self._timer is not None:
            self._timer.cancel()
        if self._idle_secs <= 0:
            return
        self._timer = threading.Timer(self._idle_secs, self.shutdown)
        self._timer.daemon = True
        self._timer.start()

    def shutdown(self) -> None:
        """Stop the worker, releasing its VRAM. Safe to call when not running."""
        with self._lock:
            proc, self._proc = self._proc, None
            if proc is None or proc.poll() is not None:
                return
            try:
                proc.stdin.close()
                proc.wait(timeout=20)
            except Exception:  # noqa: BLE001 — a stuck worker gets killed
                proc.kill()

    def request(self, payload: dict) -> dict:
        with self._lock:
            if self._proc is None or self._proc.poll() is not None:
                self._proc = self._start()
            proc = self._proc
            proc.stdin.write(json.dumps(payload) + "\n")
            proc.stdin.flush()
            # A hung worker must not hang ComfyUI's queue forever: wait with a
            # ceiling, then kill it so the next job starts from a clean slate.
            # The worker keeps its protocol channel free of library noise, but
            # tolerate a stray non-JSON line rather than failing a whole render
            # over one print from a future dependency.
            resp = None
            while resp is None:
                ready, _, _ = select.select([proc.stdout], [], [], self._call_timeout)
                if not ready:
                    proc.kill()
                    self._proc = None
                    raise RuntimeError(
                        f"Qwen3-TTS worker did not answer within {self._call_timeout:.0f}s"
                    )
                line = proc.stdout.readline()
                if not line:
                    self._proc = None
                    raise RuntimeError(
                        "Qwen3-TTS worker exited without answering — see the ComfyUI "
                        "log for its traceback (a missing venv is the usual cause)"
                    )
                try:
                    resp = json.loads(line)
                except json.JSONDecodeError:
                    print(f"[llmgw_qwen3_tts] ignoring worker output: {line!r}")
        if not resp.get("ok"):
            raise RuntimeError(f"Qwen3-TTS: {resp.get('error', 'unknown error')}")
        self._touch_idle_timer()
        return resp


_WORKER = _Worker()


class LlmgwQwen3TTS:
    """Text → speech, with a preset, described, or cloned voice."""

    CATEGORY = "audio"
    FUNCTION = "synthesize"
    RETURN_TYPES = ("AUDIO",)
    RETURN_NAMES = ("audio",)

    @classmethod
    def INPUT_TYPES(cls):  # noqa: N802 — ComfyUI's required spelling
        return {
            "required": {
                "text": ("STRING", {"multiline": True, "default": ""}),
                "mode": (MODES, {"default": "design"}),
                "language": (LANGUAGES, {"default": "Auto"}),
                "instruct": ("STRING", {"multiline": True, "default": ""}),
                "speaker": (PRESET_SPEAKERS, {"default": "Ryan"}),
                "size": (SIZES, {"default": "1.7B"}),
                "seed": (
                    "INT",
                    {"default": 0, "min": 0, "max": 0xFFFFFFFF, "control_after_generate": True},
                ),
            },
            "optional": {
                "reference_audio": ("AUDIO",),
                "reference_text": ("STRING", {"multiline": True, "default": ""}),
            },
        }

    def synthesize(
        self,
        text,
        mode,
        language,
        instruct,
        speaker,
        size,
        seed,
        reference_audio=None,
        reference_text="",
    ):
        payload = {
            "mode": mode,
            "text": text,
            "language": language,
            "instruct": instruct,
            "speaker": speaker,
            "size": size,
            "seed": seed,
            "reference_text": reference_text,
        }
        tmp_ref = None
        try:
            if mode == "clone":
                if reference_audio is None:
                    raise RuntimeError(
                        "mode `clone` needs a reference_audio input (a few seconds "
                        "of the voice to copy)"
                    )
                tmp_ref = self._write_reference(reference_audio)
                payload["reference_audio"] = tmp_ref
            resp = _WORKER.request(payload)
            path = resp["path"]
            try:
                # soundfile rather than torchaudio: torchaudio 2.11 routes
                # `load` through TorchCodec, which this image does not ship,
                # and soundfile is already here (the worker writes with it).
                data, sample_rate = sf.read(path, dtype="float32", always_2d=True)
            finally:
                os.unlink(path)
            waveform = torch.from_numpy(np.ascontiguousarray(data.T))
        finally:
            if tmp_ref:
                os.unlink(tmp_ref)
        # ComfyUI's AUDIO is a batched [B, C, T] tensor plus its rate.
        return ({"waveform": waveform.unsqueeze(0), "sample_rate": sample_rate},)

    @staticmethod
    def _write_reference(audio: dict) -> str:
        """Spill an AUDIO input to a wav the worker can open by path."""
        waveform = audio["waveform"]
        if waveform.ndim == 3:  # drop the batch dimension
            waveform = waveform[0]
        fd, path = tempfile.mkstemp(prefix="llmgw-tts-ref-", suffix=".wav")
        os.close(fd)
        # soundfile wants [frames, channels]; ComfyUI hands us [channels, T].
        samples = waveform.to(torch.float32).cpu().numpy().T
        sf.write(path, samples, int(audio["sample_rate"]))
        return path


NODE_CLASS_MAPPINGS = {"LlmgwQwen3TTS": LlmgwQwen3TTS}
NODE_DISPLAY_NAME_MAPPINGS = {"LlmgwQwen3TTS": "Qwen3-TTS (croit)"}
