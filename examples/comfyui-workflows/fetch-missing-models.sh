#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
#
# Provision the model weights a ComfyUI workflow catalog needs.
#
# The list is DERIVED FROM THE WORKFLOWS, not hardcoded: every `workflow.json`
# in the catalog is scanned for inputs whose value looks like a weights file
# (`unet_name`, `lora_name`, `ckpt_name`, `text_encoder`, … — matched by value,
# so loader-specific key names don't matter), each is checked against what's
# already installed, and only the missing ones are fetched. Re-pin a model in a
# workflow and this picks it up on the next run. Where each filename comes from
# is recorded in `models.json` next to this script.
#
# This matters because ComfyUI rejects a job naming a file it doesn't have with
# an opaque `HTTP 400 prompt_outputs_failed_validation` / `value_not_in_list`
# from `POST /prompt`, before any GPU work — which reaches the user as a failed
# tool call rather than as a configuration error.
#
# Usage:
#   MODELS=/path/to/ComfyUI/models ./fetch-missing-models.sh
#   MODELS=... ./fetch-missing-models.sh --dry-run              # just report
#   MODELS=... ./fetch-missing-models.sh llmgw-merge-images     # one bundle
#   MODELS=... CATALOG=/etc/gateway/comfyui-workflows ./fetch-missing-models.sh
#
# Environment:
#   MODELS    ComfyUI models/ directory (required, must exist)
#   CATALOG   directory holding the <bundle>/workflow.json tree
#             (default: the directory this script lives in)
#   COMFY     ComfyUI base URL for the closing check (default 127.0.0.1:8188)
#   HF_TOKEN  needed for licence-gated repos (models.json marks them `gated`)
#   JOBS      parallel downloads (default 4). Hugging Face throttles per
#             connection — ~2 MiB/s on one stream vs ~19 MiB/s each in parallel —
#             so this is worth more than it looks.
#
# Idempotent and resumable: present files are skipped, interrupted transfers
# resume via `curl -C -`, and one failure doesn't abort the rest. Needs bash,
# curl and python3 (for JSON only — no third-party modules).
set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
MODELS=${MODELS:-}
CATALOG=${CATALOG:-$HERE}
COMFY=${COMFY:-http://127.0.0.1:8188}
JOBS=${JOBS:-4}
HF=https://huggingface.co
DRY=0

BUNDLES=()
for arg in "$@"; do
  case $arg in
    -h|--help)
      awk 'NR>3 && /^#/ { sub(/^# ?/, ""); print; next } NR>3 { exit }' "${BASH_SOURCE[0]}"
      exit 0 ;;
    -n|--dry-run) DRY=1 ;;
    -*) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
    *) BUNDLES+=("$arg") ;;
  esac
done

if [[ -z $MODELS || ! -d $MODELS ]]; then
  echo "error: set MODELS to your ComfyUI models/ directory (got '${MODELS:-<unset>}')" >&2
  exit 1
fi
if [[ ! -d $CATALOG ]]; then
  echo "error: CATALOG=$CATALOG is not a directory" >&2
  exit 1
fi

# --- plan: what does the catalog reference that isn't installed? -------------
# Emits TSV rows: REF <file> | PLAN <dir> <file> <repo> <path> <gated> | UNMAPPED <file> <bundles>
PLAN=$(MODELS="$MODELS" CATALOG="$CATALOG" HERE="$HERE" BUNDLES="${BUNDLES[*]:-}" python3 - <<'PY'
import json, os, sys, glob

models  = os.environ["MODELS"]
catalog = os.environ["CATALOG"]
picked  = set(filter(None, os.environ.get("BUNDLES", "").split()))

srcfile = os.path.join(catalog, "models.json")
if not os.path.exists(srcfile):     # a deployed catalog may not carry a copy
    srcfile = os.path.join(os.environ["HERE"], "models.json")
try:
    with open(srcfile) as fh:
        sources = {k: v for k, v in json.load(fh).items() if not k.startswith("_")}
except FileNotFoundError:
    print(f"error: models.json not found next to the catalog ({srcfile})", file=sys.stderr)
    raise SystemExit(1)

# A weights file is recognised by its value, not by the input's key name.
EXT = (".safetensors", ".pt", ".pth", ".ckpt", ".gguf", ".bin", ".onnx")

installed = set()
for _, _, files in os.walk(models):
    installed.update(files)

refs = {}   # filename -> [bundle, ...]
for wf in sorted(glob.glob(os.path.join(catalog, "*", "workflow.json"))):
    bundle = os.path.basename(os.path.dirname(wf))
    if picked and bundle not in picked and bundle.removeprefix("llmgw-") not in picked:
        continue
    with open(wf) as fh:
        doc = json.load(fh)
    for node in doc.values():
        if not isinstance(node, dict):
            continue
        for value in node.get("inputs", {}).values():
            if isinstance(value, str) and value.endswith(EXT):
                refs.setdefault(value, []).append(bundle)

if picked and not refs:
    print(f"error: no bundle matched {sorted(picked)}", file=sys.stderr)
    raise SystemExit(1)

for name in sorted(refs):
    print(f"REF\t{name}")
    if name in installed:
        continue
    src = sources.get(name)
    if not src:
        print(f"UNMAPPED\t{name}\t{','.join(sorted(set(refs[name])))}")
        continue
    print("\t".join(["PLAN", src["dir"], name, src["repo"], src["path"],
                     "gated" if src.get("gated") else "-"]))
PY
)

# Portable array fill — `mapfile` is bash 4+, and macOS still ships 3.2.
REFS=(); TODO=(); UNKNOWN=()
while IFS= read -r line; do [[ -n $line ]] && REFS+=("$line"); done \
  < <(awk -F'\t' '$1=="REF"{print $2}' <<<"$PLAN")
while IFS= read -r line; do [[ -n $line ]] && TODO+=("$line"); done \
  < <(awk -F'\t' '$1=="PLAN"' <<<"$PLAN")
while IFS= read -r line; do [[ -n $line ]] && UNKNOWN+=("$line"); done \
  < <(awk -F'\t' '$1=="UNMAPPED"{print $2" (referenced by "$3")"}' <<<"$PLAN")

echo "catalog: $CATALOG"
echo "models:  $MODELS"
echo "${#REFS[@]} weight file(s) referenced, ${#TODO[@]} missing, ${#UNKNOWN[@]} with no known source"
echo

if [[ ${#UNKNOWN[@]} -gt 0 ]]; then
  echo "--- referenced but not in models.json — add an entry or re-pin the workflow: ---"
  printf '  %s\n' "${UNKNOWN[@]}"
  echo
fi

if [[ ${#TODO[@]} -eq 0 ]]; then
  echo "nothing to download."
else
  while IFS=$'\t' read -r _ dir name repo path gated; do
    [[ -n ${name:-} ]] || continue
    printf '  %-24s %s%s\n' "$dir" "$name" \
      "$([[ $gated == gated && -z ${HF_TOKEN:-} ]] && echo '   [GATED — needs HF_TOKEN]' || true)"
  done < <(printf '%s\n' "${TODO[@]}")
  echo
fi

if [[ $DRY == 1 ]]; then
  echo "(dry run — nothing fetched)"
  exit 0
fi

# --- fetch -------------------------------------------------------------------
STATE=$(mktemp -d)
trap 'rm -rf "$STATE"' EXIT

fetch() { # fetch <subdir> <filename> <repo> <path-in-repo>
  local dir="$1" name="$2" repo="$3" path="$4"
  local dest="$MODELS/$dir/$name"
  echo "== fetch  $dir/$name  <- $repo"
  mkdir -p "$MODELS/$dir"
  if curl -fsSL -C - --retry 5 --retry-delay 5 --retry-all-errors \
       ${HF_TOKEN:+-H "Authorization: Bearer $HF_TOKEN"} \
       -o "$dest.part" "$HF/$repo/resolve/main/$path"; then
    mv "$dest.part" "$dest"
    echo "== done   $dir/$name"
  else
    # Keep the .part file: a re-run resumes it via curl -C -.
    echo "== FAIL   $dir/$name" >&2
    # One marker per file — $$ is identical across the background jobs.
    : > "$STATE/fail.$(printf '%s' "$name" | tr -c 'A-Za-z0-9._-' '_')"
  fi
}

running=0
if [[ ${#TODO[@]} -gt 0 ]]; then
  while IFS=$'\t' read -r _ dir name repo path _; do
    [[ -n ${name:-} ]] || continue
    fetch "$dir" "$name" "$repo" "$path" &
    if (( ++running % JOBS == 0 )); then wait; fi
  done < <(printf '%s\n' "${TODO[@]}")
  wait
fi

fails=$(find "$STATE" -name 'fail.*' | wc -l | tr -d ' ')

# --- verify ------------------------------------------------------------------
# `ok` only means ComfyUI lists the file; it does not prove the weights load.
# Validate a bundle for real by submitting it once (see docs/comfyui.md).
echo
echo "--- ComfyUI at $COMFY reports: ---"
OBJINFO="$STATE/object_info.json"
if curl -sf "$COMFY/object_info" -o "$OBJINFO"; then
  for n in ${REFS[@]+"${REFS[@]}"}; do
    if grep -qF "$n" "$OBJINFO"; then echo "  ok      $n"; else echo "  MISSING $n"; fi
  done
else
  echo "  (could not reach ComfyUI — restart it if a fetched file stays MISSING)"
fi

if [[ $fails -gt 0 ]]; then
  echo
  echo "$fails download(s) failed — re-run to resume." >&2
  exit 1
fi
