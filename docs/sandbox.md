# Code-execution sandbox

The gateway can let the chat model **run code** — Python, shell, document
generation, headless-browser capture — inside a strongly isolated, single-use
gVisor sandbox, and return the results (stdout/stderr + produced files) into the
conversation. It's exposed as these tools:

| Tool | What it does |
|---|---|
| `run_in_sandbox` | Run arbitrary Python or shell; returns stdout/stderr + any files written. |
| `generate_document` | Markdown → PDF / DOCX / PPTX via pandoc (a safe preset). |
| `capture_webpage` | Headless-chromium screenshot / PDF / text of a URL (needs egress). |
| `convert_document` | Convert an uploaded file (pptx/docx/xlsx/odf/pdf) to PDF / DOCX / TXT / HTML / per-slide images via LibreOffice. |
| `edit_presentation` | Modify an uploaded `.pptx` with python-pptx (`input.pptx` → `output.pptx`). |

### Working on uploaded files

`convert_document` / `edit_presentation` — and `run_in_sandbox` itself — can
operate on files the user uploaded. The model never holds the bytes, so the
gateway bridges them in server-side: the **current turn's uploads are staged
into the sandbox working directory automatically** (under their original
names), and the model can pull in a file from **earlier in the conversation**
by passing its attachment id (`<turn>/<file>`, from an `[attached …]` stub) —
`run_in_sandbox` takes an `attachments: [{id}]` array, the presets take an
`attachment_id`. Resolution is scoped to the caller's own chat session, and a
per-run input budget (50 MiB) bounds what gets staged. Because each run is
single-use (no `/work` persistence between calls), multi-file work must happen
in **one** call — the staging assembles every needed input up front. On the
`/v1` API path there's no session and no S3-backed upload, so staging is a
no-op there and the tools fall back to inline text inputs.

## Why two services

Executing LLM-generated code is the textbook untrusted-code problem, so the
isolation has to be real. The design is two-layer: an **unprivileged gateway**
that only speaks HTTP, and a **privileged runner** that drives podman.

### What runs where

The runner is a **host process** (a systemd service), *not* a container — so it
never shows up in `podman ps`. It needs *local* podman to select the gVisor
runtime (remote podman over the socket can't pass `--runtime`). The things it
*creates* — the warm-pool sandboxes — are the containers you see.

```
host: llm01
│
├─ systemd services  ── host processes, NOT in `podman ps` ──────────────────┐
│    └─ sandbox-runner.service                                               │
│         listens on 10.88.0.1:9000 (podman bridge gw IP, host-only)         │
│         drives LOCAL podman:  podman run --runtime runsc …  ───────────┐   │
│                                                                        │   │
├─ podman containers  ── these ARE in `podman ps` ──────────────────────│──┐ │
│    ├─ gateway          ghcr.io/croit/llm-gateway        :8080  ◄─HTTP /run │
│    ├─ qwen / embedding / voxtral   (vLLM model servers) :8002/3/5          │
│    └─ warm pool: N × llm-gateway-sandbox  "sleep infinity"  ◄──────────┘   │
│         gVisor (runsc) · --network none · uid 1001 · mem/cpu/pids capped   │
│         created + destroyed by the runner, one job each (single-use)       │
└────────────────────────────────────────────────────────────────────────────┘
```

So the three idle `…/llm-gateway-sandbox  sleep infinity` containers in
`podman ps` **are** the runner's warm pool — proof it's running. To see the
runner itself: `systemctl status sandbox-runner.service` /
`curl 10.88.0.1:9000/healthz`.

### Request flow

```
1. model emits tool_call: run_in_sandbox(code)
2. gateway tool  ──HTTP POST /run (RunRequest JSON)──►  runner @ 10.88.0.1:9000
3. runner pops a warm sandbox (or boots one on demand)           [host process]
4. runner ──podman exec──►  /usr/local/bin/sandbox-agent          [gVisor container]
5. agent runs the code:  uid 1001 · --network none · read-only rootfs
                         · mem/swap/cpu/pids capped · wall-clock timeout
6. agent ──stdout/stderr + produced files (artifacts)──►  runner
7. runner DESTROYS that sandbox (single-use) and refills the pool in the bg
8. gateway returns stdout to the model; files → S3 chat attachment
                                              + bearer download URL (/v1 path)
```

The **gold image** baked for step 4 is a batteries-included "system-engineer
shell": python + data/science stack, LibreOffice, pandoc, typst, ffmpeg, duckdb,
ripgrep/jq, tshark, tesseract OCR, headless chromium. Default per call: **no
network, single-use**.

### What's in the gold image

A batteries-included "system-engineer shell" so the model can debug logs,
handle office files, and convert formats without runtime network:

- **Languages/build:** python3 (in a venv) + pip, gcc/make/build-essential.
- **Data/science:** pandas, numpy, scipy, scikit-learn, statsmodels, sympy,
  polars, pyarrow, **duckdb** (+ CLI) and sqlite3 — SQL over CSV/JSON/Parquet
  and large/compressed logs; matplotlib/seaborn for charts.
- **Logs/text/CLI:** ripgrep, jq, yq, **jc**, awk/sed, file, xxd, lnav, tree,
  pv, moreutils, dateutils; gzip/zstd/xz/bzip2/7z for compressed logs.
- **Office:** LibreOffice (writer/calc/impress/draw, `soffice --headless` for
  office↔pdf), python-docx, python-pptx, openpyxl/xlsxwriter/xlrd, odfpy.
- **PDF:** poppler-utils, ghostscript, qpdf, pypdf, pdfplumber, **pymupdf**,
  reportlab, img2pdf.
- **Images/OCR/media:** ffmpeg, imagemagick (PDF/PS coders enabled), libvips,
  pillow, opencv, **tesseract OCR** (eng+deu) via pytesseract.
- **Docs/diagrams:** pandoc, typst, weasyprint, markdown, **graphviz**; Latin +
  CJK + emoji fonts.
- **Networking:** **tshark/tcpdump** + scapy/dpkt to read `.pcap`/`.pcapng`;
  curl/wget, dig, rsync, openssl, netcat, iproute2 (egress gated by the proxy
  allowlist).
- **DB/ops clients:** sqlalchemy, psycopg (postgres), pymysql, dnspython,
  paramiko, psutil; rich/humanize for readable output.
- **Browser:** headless chromium + playwright.

Edit `sandbox-image/Containerfile` to add or trim tools, then rebuild/push.

- **Isolation = a separate-kernel runtime.** On a podman host the practical
  choice is **gVisor (`runsc`)** — a real OCI runtime (its own userspace
  kernel) that podman can drive directly and a battle-tested untrusted-code
  sandbox. Plain containers (`crun`/`runc`) share the host kernel and are
  *not* a sufficient boundary.
  See [Installing a sandbox runtime](#installing-a-sandbox-runtime).
- **The gateway stays unprivileged.** It only does HTTP. The
  **`sandbox-runner`** is the one component that drives podman and spawns the
  sandboxes, so the powerful surface is small, separate, and never
  internet-facing. It runs as a host service (it needs **local** podman to
  select the gVisor runtime — remote podman over the socket can't pass
  `--runtime`).
- **Single-use:** every job runs in a fresh container that's destroyed
  afterwards — no state leaks between calls or users. A warm pool of
  pre-booted sandboxes hides cold-start latency.
- **Default-deny network:** a sandbox has no network unless the call requests
  it *and* the operator wired an egress proxy, which only forwards to an
  allowlist.

Because gateway↔runner is just HTTP, the runner tier can live on separate
hosts and scale independently (each runner uses its own host's local podman) —
but then the channel MUST be mTLS-protected and the runner MUST NOT be publicly
reachable (it's arbitrary-code-execution as a service).

## Pieces in this repo

| Path | What |
|---|---|
| `crates/sandbox-runner/` | The runner service (warm pool, podman + OCI-runtime orchestration, `/run` API). |
| `crates/gateway/src/server/tools/sandbox.rs` | The three gateway tools. |
| `crates/shared/src/sandbox.rs` | The runner↔gateway wire contract. |
| `sandbox-image/` | The gold workload image (`Containerfile` + `sandbox-agent`). |
| `deploy/sandbox-runner/Containerfile` | The runner image — built by CI; the host runner binary is extracted from it. |
| `deploy/sandbox/sandbox-runner.service` | Host systemd unit for the runner. |
| `deploy/quadlet/*.{container,network}`, `squid.conf`, `allowlist.txt` | Network + egress-proxy Quadlets and config. |

## Installing a sandbox runtime

The runner spawns each job under an OCI runtime via podman, so the runtime must
implement the OCI CLI (create/start/delete). **Use gVisor (`runsc`)** — podman
drives it directly and it's a strong untrusted-code sandbox. Install on Debian:

```sh
curl -fsSL https://gvisor.dev/archive.key | sudo gpg --dearmor -o /usr/share/keyrings/gvisor-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main" | sudo tee /etc/apt/sources.list.d/gvisor.list
sudo apt-get update && sudo apt-get install -y runsc

# Wrap runsc to always pass --network=host (see note below — required on
# rootful podman):
sudo tee /usr/local/bin/runsc-sandbox >/dev/null <<'EOF'
#!/bin/sh
exec /usr/bin/runsc --network=host "$@"
EOF
sudo chmod +x /usr/local/bin/runsc-sandbox

# Register the wrapper as the `runsc` runtime under [engine.runtimes]
# (don't add a second [engine.runtimes] header — that's a TOML error):
printf '[engine.runtimes]\nrunsc = ["/usr/local/bin/runsc-sandbox"]\n' | sudo tee -a /etc/containers/containers.conf
sudo podman run --rm --network none --runtime runsc docker.io/library/alpine uname -r   # kernel ≠ host → isolated
```

> **Why the wrapper?** Two podman gotchas combine here:
> 1. On rootful podman, runsc's default network mode (its own netstack) aborts
>    with `cannot run with network enabled in root network namespace`.
>    `--network=host` makes gVisor use the network namespace podman hands the
>    container instead of building its own — empty for default-deny runs,
>    proxy-only for egress runs. The **kernel/syscall isolation is unchanged**;
>    only who owns the netstack changes.
> 2. `containers.conf` runtime entries are a list of **binary paths**, not a
>    command line — you cannot pass `--network=host` there directly (it would be
>    read as a second path and ignored). Hence the one-line wrapper script.

> **Why gVisor and not a full VM?** gVisor gives each sandbox its own
> userspace kernel — a strong untrusted-code boundary that podman drives
> directly, with no KVM/hypervisor requirement and far lower per-job
> overhead than a microVM. That makes it the right fit for short-lived,
> single-use tool calls on a podman host.

## Quick start (Debian 13, podman + gVisor)

Copy-paste runbook for a host already running the gateway as a Quadlet, with a
repo checkout present. `/dev/kvm` is NOT required (gVisor runs in userspace).

```sh
# 0. one-time: make the GHCR images pullable — GitHub → org Packages →
#    llm-gateway-sandbox and -sandbox-runner → make Public
#    (or: sudo podman login ghcr.io  with a read:packages PAT)

# 1. install gVisor (runsc) and register it as a podman runtime
curl -fsSL https://gvisor.dev/archive.key | sudo gpg --dearmor -o /usr/share/keyrings/gvisor-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main" | sudo tee /etc/apt/sources.list.d/gvisor.list
sudo apt-get update && sudo apt-get install -y runsc
# wrap runsc with --network=host (required on rootful podman — see
# "Installing a sandbox runtime" for why):
sudo tee /usr/local/bin/runsc-sandbox >/dev/null <<'EOF'
#!/bin/sh
exec /usr/bin/runsc --network=host "$@"
EOF
sudo chmod +x /usr/local/bin/runsc-sandbox
# register the wrapper under [engine.runtimes] (create the section if absent):
if grep -q '^\[engine.runtimes\]' /etc/containers/containers.conf 2>/dev/null; then
  sudo sed -i '/^\[engine.runtimes\]/a runsc = ["/usr/local/bin/runsc-sandbox"]' /etc/containers/containers.conf
else
  printf '\n[engine.runtimes]\nrunsc = ["/usr/local/bin/runsc-sandbox"]\n' | sudo tee -a /etc/containers/containers.conf
fi
# smoke test: the printed kernel MUST differ from `uname -r` (→ isolated)
sudo podman run --rm --network none --runtime runsc docker.io/library/alpine uname -r
uname -r

# 2. deploy the runner (extracts the binary, installs the host unit, binds the
#    podman bridge gateway IP — setup prints the exact runner_url to use)
sudo deploy/sandbox/setup-sandbox.sh            # add --egress for pip / capture_webpage

# 3. wire the gateway to the runner (no gateway network change — it already
#    reaches the bridge gateway IP that setup printed, usually 10.88.0.1):
#      add to the gateway config:
#         [sandbox]
#         runner_url = "http://10.88.0.1:9000"
sudo podman pull ghcr.io/croit/llm-gateway:latest     # the rebuilt gateway has the sandbox tools
sudo systemctl daemon-reload
sudo systemctl restart gateway.service

# 4. verify
journalctl -u sandbox-runner.service | grep -i isolation   # expect: isolation confirmed
curl -s 10.88.0.1:9000/healthz                             # {"status":"ok"}
```

Then in the chat UI: *"run a python script in the sandbox that prints the
kernel version"* — the kernel must differ from the host's `uname -r`.

## Deploy (detailed)

Prereqs: a Linux host with rootful podman and the gateway already running as a
Quadlet. (`/dev/kvm` is not required — gVisor runs entirely in userspace.)

1. **Install the runtime** (see [above](#installing-a-sandbox-runtime)) and
   confirm `sudo podman run --rm --network none --runtime runsc docker.io/library/alpine uname -r`
   prints a kernel different from the host's `uname -r`.

2. **Pull access to the images.** CI builds + pushes all three to GHCR on
   `main`/tags (`ghcr.io/croit/llm-gateway`, `…-sandbox`, `…-sandbox-runner`;
   tags `latest`/branch/tag/SHA). GHCR packages are **private by default** —
   make the two `…-sandbox*` packages **public** in the org's package settings,
   or `podman login ghcr.io` on the host. (To build locally instead, see the
   `docker build` lines in `sandbox-image/Containerfile` and
   `deploy/sandbox-runner/Containerfile`.)

3. **Run the setup script** (collapses the steps below):
   ```sh
   sudo deploy/sandbox/setup-sandbox.sh          # add --egress to also wire the proxy
   ```
   It pulls the images, extracts the runner binary to `/usr/local/bin/`,
   installs the host runner unit (with `SANDBOX_BIND` set to the detected podman
   bridge gateway IP), and `enable --now`s the runner. Equivalent manual steps:
   ```sh
   # runner binary (extracted from the runner image)
   cid=$(sudo podman create ghcr.io/croit/llm-gateway-sandbox-runner:latest)
   sudo podman cp "$cid":/usr/local/bin/sandbox-runner /usr/local/bin/sandbox-runner
   sudo podman rm "$cid"; sudo chmod +x /usr/local/bin/sandbox-runner
   # find the podman bridge gateway IP the runner should bind (usually 10.88.0.1)
   BRIDGE_IP=$(sudo podman network inspect podman --format '{{(index .Subnets 0).Gateway}}')
   sudo cp deploy/sandbox/sandbox-runner.service /etc/systemd/system/
   sudo sed -i "s|^Environment=SANDBOX_BIND=.*|Environment=SANDBOX_BIND=${BRIDGE_IP}:9000|" \
       /etc/systemd/system/sandbox-runner.service
   # optional egress (pip / web):
   sudo mkdir -p /etc/gateway/sandbox
   sudo cp deploy/quadlet/squid.conf    /etc/gateway/sandbox/
   sudo cp deploy/quadlet/allowlist.txt /etc/gateway/sandbox/
   sudo cp deploy/quadlet/sandbox-egress.network /etc/containers/systemd/
   sudo cp deploy/quadlet/egress-proxy.container  /etc/containers/systemd/
   sudo systemctl daemon-reload
   sudo systemctl enable --now sandbox-runner.service
   ```
   For egress also uncomment `SANDBOX_EGRESS_NETWORK=sandbox-egress` +
   `SANDBOX_EGRESS_PROXY=http://egress-proxy:3128` in `sandbox-runner.service`
   and start `egress-proxy.service`.

4. **Point the gateway at the runner.** No gateway network change is needed —
   it already reaches the bridge gateway IP over the default podman network. In
   the gateway config (use the `BRIDGE_IP` from step 3, usually `10.88.0.1`):
   ```toml
   [sandbox]
   runner_url = "http://10.88.0.1:9000"
   ```
   Produced-file delivery (chat attachments + API download URLs) needs
   `[chat.s3]` configured. Then `sudo systemctl daemon-reload && sudo systemctl
   restart gateway.service`.

## Docker / Compose deployment (gVisor via the Docker socket)

The podman path above runs the runner as a **host service** because remote
podman can't pass `--runtime` over its socket. **Docker is different** — the
Docker daemon honors `--runtime` passed over its socket, so on a Docker host the
runner *can* run as a container and still get **real gVisor isolation**. That's
the `sandbox-runner` service in [`compose.example.yml`](../deploy/compose.example.yml)
(it drives the host Docker socket with `SANDBOX_PODMAN=docker`,
`SANDBOX_RUNTIME=runsc`, spawning each job as a single-use sibling container).

One-time host setup:

1. Install gVisor and register `runsc` as a **Docker** runtime:
   ```sh
   # install runsc (see "Installing a sandbox runtime" above), then:
   sudo runsc install            # adds the runsc runtime to /etc/docker/daemon.json
   sudo systemctl restart docker
   docker run --rm --runtime=runsc docker.io/library/alpine uname -r  # kernel ≠ host
   ```
   Under Docker no `--network=host` wrapper is needed — that shim is a rootful-
   podman quirk; `runsc install` is the Docker-native equivalent.

2. Bring up the sandbox profile and point the gateway at the runner:
   ```sh
   docker compose -f deploy/compose.example.yml --profile sandbox up -d
   ```
   ```toml
   # gateway.toml
   [sandbox]
   runner_url = "http://sandbox-runner:9000"
   ```

The boot self-check still guards this: the runner logs `isolation confirmed` or
a loud `SANDBOX IS NOT ISOLATED` — the latter means `runsc` isn't actually
registered/applied. Fix it before granting the tools.

> **Docker Desktop / macOS:** gVisor isn't available, so there is **no isolated
> Docker path locally**. For dev only, set `SANDBOX_RUNTIME=local-unsafe` on the
> runner (runs code with NO isolation) or use the native `cargo run` path below.
> Never run `local-unsafe` in a deployment.

> **Security:** mounting `docker.sock` is host-root-equivalent, and this service
> runs untrusted code. The compose file never publishes its port; keep it off
> any public interface, front cross-host hops with mTLS, and grant the sandbox
> tools per-role/-token deliberately.

## Verify it works

1. **Isolation is real.** Check the runner's startup log:
   ```sh
   journalctl -u sandbox-runner.service | grep -i isolation
   ```
   Expect `isolation confirmed: the sandbox runs a separate kernel from the
   host`. If you instead see **`SANDBOX IS NOT ISOLATED`**, the runtime didn't
   apply — fix the runtime before exposing the tool (the runner self-checks
   this at boot by comparing the sandbox's kernel to the host's).

2. **End to end.** In the chat UI (or via `/v1`), ask the model to
   *"run a python script in the sandbox that prints the kernel version"* and
   confirm you get output — and that the kernel differs from `uname -r` on the
   host (i.e. it really ran in an isolated sandbox, not a bare container).

### Building the runner binary from source

`setup-sandbox.sh` extracts the runner binary from the prebuilt image. To use
your own build instead (e.g. local changes), drop it in before installing the
unit:

```sh
cargo build --release -p sandbox-runner
sudo install -m0755 target/release/sandbox-runner /usr/local/bin/sandbox-runner
sudo cp deploy/sandbox/sandbox-runner.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now sandbox-runner.service
```

## Testing on a dev machine (macOS, no gVisor)

You don't need gVisor/podman to develop and test most of this.

**1. Automated tests** run anywhere `cargo` + `python3` do (so macOS works):

```sh
cargo test -p shared -p sandbox-runner -p gateway sandbox
```

- `shared` / gateway tests cover the wire contract + tool plumbing (wiremock stands in for the runner).
- `sandbox-runner` pool tests use a fake backend.
- `backend::local_tests` runs the **real** `sandbox-agent` via `python3` end-to-end (file in → run → artifact out), so the agent contract is verified without any container.

**2. Manual end-to-end with the `local-unsafe` backend** — runs code directly on your host (NO isolation; dev only), so you can drive the full gateway → runner → agent path on macOS:

```sh
# Terminal 1: the runner, executing on the host (no podman needed).
SANDBOX_RUNTIME=local-unsafe SANDBOX_BIND=127.0.0.1:9000 \
  cargo run -p sandbox-runner

# Terminal 2: point the gateway at it.
#   [sandbox]
#   runner_url = "http://127.0.0.1:9000"
# then `mise run dev` and call run_in_sandbox from the chat UI / API.
```

Plain Python/shell, and any tool already on your Mac, work. The doc-gen /
browser libraries (LibreOffice, python-pptx, chromium) aren't on the host
unless you install them, so `generate_document` / `capture_webpage` need the
real image — see option 3. To allow network in local mode set
`SANDBOX_EGRESS_NETWORK=local` (the local backend just uses host networking).

**3. Closer-to-prod on macOS** — `podman machine` (a Linux VM) lets you run
the real sandbox image under a container runtime:

```sh
podman machine init && podman machine start
podman build -t llm-gateway-sandbox:dev sandbox-image/
SANDBOX_RUNTIME=crun SANDBOX_IMAGE=llm-gateway-sandbox:dev \
  cargo run -p sandbox-runner          # crun = container only, NOT isolation — dev only
```

`crun` shares the host kernel, so this exercises the image and the runner
plumbing but is **not** an isolation boundary. Validate real gVisor isolation
on a Linux host (`llm01`), not on the Mac.

## Configuration reference

Everything is file-tunable; nothing is hardcoded.

**Gateway** — `[sandbox]` in the gateway config TOML (`gateway.example.toml`):

| Key | Default | Meaning |
|---|---|---|
| (block present) | — | Registers the sandbox tools. Omit the block to leave the feature out entirely. |
| `enabled` | `true` | Master switch — `false` disables the tools while keeping the block (e.g. retain `runner_url`). |
| `runner_url` | — (required) | Where to reach the sandbox-runner, e.g. `http://10.88.0.1:9000`. |
| `timeout_secs` | `120` | HTTP timeout for one `/run` call (the tool also extends the runner loop ceiling to match). |
| `max_artifact_bytes` | `26214400` (25 MiB) | Largest produced file accepted back; larger ones are reported as dropped. |

Per-tool and per-user/-token control is the **existing** mechanism: each tool
(`run_in_sandbox`, `generate_document`, `capture_webpage`, `convert_document`,
`edit_presentation`) is RBAC-granted per role and toggleable per user/token on
the `/tools` page — default-off.

**Runner** — environment variables (set as `Environment=` lines in
`deploy/sandbox/sandbox-runner.service`; see `crates/sandbox-runner/src/config.rs`):
`SANDBOX_BIND`, `SANDBOX_IMAGE`, `SANDBOX_RUNTIME` (`runsc`/`crun`/`local-unsafe`),
`SANDBOX_POOL_SIZE`, `SANDBOX_MAX_CONCURRENT`, `SANDBOX_TIMEOUT_SECS`,
`SANDBOX_MAX_TIMEOUT_SECS`, `SANDBOX_MEMORY`, `SANDBOX_CPUS`, `SANDBOX_PIDS_LIMIT`,
`SANDBOX_WORK_SIZE`, `SANDBOX_TMP_SIZE`, `SANDBOX_MAX_OUTPUT_BYTES`,
`SANDBOX_EGRESS_NETWORK`, `SANDBOX_EGRESS_PROXY`.

> **Sizing for large-file work (video, big datasets):** `/work` and `/tmp` are
> RAM-backed tmpfs charged to the `--memory` cgroup, so
> `SANDBOX_WORK_SIZE + SANDBOX_TMP_SIZE` + the job's own RAM must stay under
> `SANDBOX_MEMORY`. Budget `SANDBOX_MEMORY × SANDBOX_MAX_CONCURRENT` against free
> host RAM (with headroom). Produced files also pass back through the gateway's
> `[sandbox] max_artifact_bytes` cap — raise it for large outputs.

**Egress allowlist** — `deploy/quadlet/allowlist.txt` (one host per line),
consumed by the squid proxy. Default-deny: only listed hosts are reachable.

## Large outputs (context management)

Big stdout/stderr must not blow the model's context window. Three layers,
following the patterns in Anthropic's context-engineering guidance and
OpenAI/Codex tool-output handling:

1. **Source-side cap + preserve.** The in-sandbox agent caps the stream returned to
   the runner and, when large, writes the FULL stream to a `stdout.txt` /
   `stderr.txt` artifact.
2. **Pointers-as-context.** The `run_in_sandbox` result then carries only a
   small head+tail **preview** plus a `full_output_ref` (the artifact id) —
   not the whole stream. The model reads the rest on demand with
   **`read_sandbox_output`** (`grep` / `head` / `tail` / `range`, with bounded
   defaults). Nothing is lost; it's just not inlined.
3. **Cumulative budget + re-callable eviction.** Across tool-loop rounds the
   gateway keeps the last few `role:"tool"` results verbatim and replaces older
   large ones with a short stub (preserving the `tool_call_id` so the
   tool_call↔result pairing is never orphaned). Evicted output stays
   addressable via its `full_output_ref`.

Operators should still steer the model to filter at the source (grep/awk/duckdb
in-sandbox) rather than dumping raw data.

## Security model & caveats

- **Isolation depends on the runtime.** On podman, run under **`runsc`
  (gVisor)** — a real OCI runtime with its own userspace kernel.
  `crun`/`runc` (plain containers) share the host kernel and are NOT a safe
  boundary (local testing only).
- **The runner is host-root-equivalent** (it drives the host's podman). It binds
  the podman bridge gateway IP (usually `10.88.0.1`), which exists only on that
  bridge — never bind it to an external interface, and front any cross-host hop
  with mTLS.
- **Isolation is self-checked at boot.** The runner runs one probe and logs
  `isolation confirmed` or a loud `SANDBOX IS NOT ISOLATED` if the sandbox
  shares the host kernel (e.g. the runtime didn't apply). Treat the warning as
  a hard stop.
- **Resource caps are enforced at the host cgroup**, not just requested: every
  sandbox gets `--memory` + `--memory-swap` (equal → swap can't be used to
  exceed the cap), `--cpus`, and `--pids-limit`. Sandboxes run with
  `--oom-score-adj=1000` and the runner unit with `OOMScoreAdjust=-800`, so a
  guest memory bomb is reaped before it can take the runner or the host down.
  Per-call wall-clock timeout (clamped to `SANDBOX_MAX_TIMEOUT_SECS`) bounds CPU
  spins; the sandbox is destroyed on overrun. The cgroup view *inside* the guest
  reads "max" (gVisor presents a synthetic cgroupfs) — that's cosmetic; the real
  limit lives on the host cgroup. Tune via the `SANDBOX_*` env in the unit.
- **Defense-in-depth gaps that gVisor already contains** (lower priority): the
  guest permits `unshare -Ur` (user namespaces), `open(/dev/fuse)`, and `mknod`.
  gVisor blocks the privileged follow-on syscalls and doesn't back those devices,
  so none are exploitable today — but they're surface. They're gVisor-internal
  (not controllable via podman flags), so tightening them means a runsc/seccomp
  policy change, not a deploy-config change. `--cap-drop=ALL` +
  `no-new-privileges` are already set.
- **Egress is default-deny** and, when enabled, allowlist-only via squid on an
  `Internal=true` network — a sandbox can reach *only* the proxy, which
  forwards *only* to `allowlist.txt`. Runtime `pip install` works against the
  PyPI entries; add scraping/API hosts deliberately.
- **Runtime `pip install` pulls arbitrary third-party code** into the sandbox.
  That's contained to the one-shot sandbox, but it's still code you didn't
  vet — keep the allowlist tight.
- **Gating:** the tools are off by default and gated per role (RBAC) and per
  token, like every other tool (see `/tools`). Grant them deliberately.
- **Host validation required:** the Rust + wiring is unit-tested in CI, but the
  podman + runtime orchestration can only be verified on the host — run a real
  `run_in_sandbox` call after deploying and confirm `uname -r` inside differs
  from the host (i.e. it ran in an isolated sandbox).
