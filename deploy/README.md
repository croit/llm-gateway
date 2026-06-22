# Deployment

Everything needed to run the gateway in production lives here. Two equivalent
deployment methods are provided — pick one:

| Method | For | Files |
|---|---|---|
| **Docker Compose** | Docker / Docker Desktop hosts | [`compose.example.yml`](compose.example.yml) |
| **systemd + Podman (Quadlet)** | rootful-podman hosts (RHEL/Debian/…) | [`quadlet/`](quadlet/) (+ its [README](quadlet/README.md)) |

## Components & images

| Component | Image | Purpose |
|---|---|---|
| **gateway** | `ghcr.io/croit/llm-gateway` | The OpenAI-compatible proxy + web UI. The only one that's mandatory. |
| **google-workspace-mcp** | `ghcr.io/taylorwilsdon/google_workspace_mcp` | Self-hosted Google Workspace MCP server backing the per-user **Google Workspace** connector (Gmail/Calendar/Drive/Docs/…). Optional. |
| **sandbox-runner** | `ghcr.io/croit/llm-gateway-sandbox-runner` | Code-execution runner (`run_in_sandbox` etc.). Optional; needs gVisor. |
| **egress-proxy** | `docker.io/ubuntu/squid` | Allowlisting proxy for networked sandbox runs. Optional. |
| sandbox workload | `ghcr.io/croit/llm-gateway-sandbox` | The "gold image" the runner spawns per job (pulled by the runner, not run directly). |

Secrets and per-host config live in env files + a config TOML; the SQLite DB
(also the session store) lives in a named volume. Real secret files
(`gateway.env`, `google-workspace-mcp.env`, `gateway.toml`) are git-ignored —
only the `*.example.*` templates are committed.

---

## Quick start — Docker Compose

```bash
# from the repo root
cp deploy/quadlet/gateway.example.env              deploy/gateway.env
cp deploy/quadlet/google-workspace-mcp.example.env deploy/google-workspace-mcp.env
cp gateway.example.toml                            deploy/gateway.toml
$EDITOR deploy/gateway.env deploy/google-workspace-mcp.env deploy/gateway.toml

docker compose -f deploy/compose.example.yml up -d                 # gateway + workspace MCP
docker compose -f deploy/compose.example.yml --profile sandbox up -d  # + sandbox runner + egress
```

Relative paths in the compose file resolve against `deploy/`, so the env/config
files above live there regardless of your shell's CWD.

**Local testing tip (Docker Desktop):** run *only* the MCP server and keep the
gateway native (`mise run dev`) — that avoids a split-horizon URL problem (the
browser and a native gateway both reach the MCP at `http://localhost:8000`):

```bash
docker compose -f deploy/compose.example.yml up google-workspace-mcp
```

## Quick start — Quadlet (podman)

See [`quadlet/README.md`](quadlet/README.md) for the full walkthrough. In short,
install the `.container`/`.volume` units into `/etc/containers/systemd/`, the
env/config into `/etc/gateway/`, then `systemctl daemon-reload && systemctl
enable --now gateway.service`.

---

## Gateway

- **TLS:** the container binds `127.0.0.1:8080` — terminate HTTPS with a reverse
  proxy (Caddy/Traefik/nginx). Set `[gateway].public_url` to the external HTTPS
  URL and register `<public_url>/auth/callback` as an OIDC redirect URI.
- **State:** point `[db].path` (and `[rag].data_dir`, if used) at the named
  volume (`/var/lib/gateway`) so they survive image swaps.
- **Secrets** (`gateway.env`): `GATEWAY_SESSION_KEY`, `GATEWAY_OIDC_CLIENT_SECRET`,
  optional `GATEWAY_MCP_KEY` (encrypts per-user connector tokens at rest), and any
  per-upstream `<POOL>_API_KEY`.

---

## Google Workspace connector (Gmail / Calendar / Drive / …)

The **Google Workspace** connector is backed by the self-hosted
`google-workspace-mcp` service — Google's *hosted* MCP endpoints are gated behind
a developer-preview program and don't scale to per-user use, so the gateway uses
a self-hosted server against the **GA** Google APIs (one sign-in per user, no
preview). Background: [`../docs/connectors.md`](../docs/connectors.md).

### 1. One Google OAuth client (admin, one-time)

APIs & Services → **Credentials** → **OAuth client ID → Web application**:

- **Authorized redirect URI** = the MCP server's callback:
  `https://<mcp-host>/oauth2callback` (local: `http://localhost:8000/oauth2callback`).
- **Audience: Internal** (no verification / no token expiry for an in-org app).
- Enable the **GA** APIs you need (Gmail, Calendar, Drive, Docs, …) — *not* the
  `*mcp.googleapis.com` preview APIs.

Put the client id/secret in `google-workspace-mcp.env`.

### 2. The MCP server — env that actually works

Validated against `google_workspace_mcp` (image ENTRYPOINT is `/bin/sh -c` with a
default CMD that already runs `uv run main.py --transport streamable-http`):

| Env | Value | Note |
|---|---|---|
| `MCP_ENABLE_OAUTH21` | `true` | Multi-user OAuth 2.1 + DCR. |
| `WORKSPACE_MCP_STATELESS_MODE` | `true` | In-memory sessions. |
| `WORKSPACE_MCP_PORT` | `8000` | Endpoint served at **`/mcp`** (no trailing slash; `/mcp/` 307-redirects). |
| `TOOL_TIER` | `core` | `core`/`extended`/`complete`. **Not** `WORKSPACE_MCP_TOOL_TIER`. |
| `WORKSPACE_EXTERNAL_URL` | `https://<mcp-host>` | Public URL the browser reaches during consent. |
| `WORKSPACE_MCP_ALLOWED_CLIENT_REDIRECT_URIS` | `https://<gateway-host>/integrations/callback` | The gateway's callback (DCR allowlist). |
| `UV_CACHE_DIR` / `XDG_CACHE_HOME` | `/tmp/uv-cache` / `/tmp` | uv builds an editable install at startup; **the rootfs must stay writable** (no read-only) and the cache is redirected to tmpfs. |

Do **not** set a `command:`/`Exec=` override — it would be parsed as
`sh -c --transport …` and fail.

### 3. Not an internal sidecar — needs a public URL

The OAuth consent runs in the **user's browser** (gateway → MCP `/authorize` →
Google → MCP `/oauth2callback` → gateway). So the MCP server's HTTP endpoint must
be **browser-reachable over TLS** — give it its own reverse-proxy vhost, e.g.
Caddy:

```caddy
gworkspace-mcp.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

### 4. Wire the connector

In the gateway: **/admin/connectors → Google Workspace**, set the **MCP server
URL** to `https://<mcp-host>/mcp` (no trailing slash), leave client id/secret
empty (DCR), Save, **Enable**.

The connector ships a default **scope set** (Gmail read + compose, Calendar,
Drive, Docs/Sheets/Slides read, Tasks). This is essential: the server does a
base-only login (`openid`+`email`) and rejects every tool call with *"lack
required scopes"* unless the gateway requests the service scopes up front. Trim
the scope list on the connector if you want a narrower consent — **changing it
requires users to disconnect + reconnect**.

Users then connect once at **/integrations → Google Workspace → Connect**.

---

## GitLab (self-managed / Community Edition)

GitLab's **native** MCP (`/api/v4/mcp`) is a GitLab Duo feature requiring
**Premium/Ultimate** — Community Edition can't use it. For CE / self-managed, run
the community bridge [`zereight/gitlab-mcp`](https://github.com/zereight/gitlab-mcp)
in streamable-HTTP + remote-authorization mode. Each MCP request carries the
caller's own GitLab token, which the bridge forwards to GitLab — so every user
gets their own permissions, and the bridge needs **no public URL and no OAuth**
(the gateway reaches it internally). It backs the **GitLab (self-managed / CE)**
connector (a `static_bearer` connector; each user pastes their PAT).

Compose (`gitlab` profile) or Quadlet
([`quadlet/gitlab-mcp.container`](quadlet/gitlab-mcp.container)):

```bash
cp deploy/quadlet/gitlab-mcp.example.env deploy/gitlab-mcp.env
$EDITOR deploy/gitlab-mcp.env          # GITLAB_API_URL=https://<your-gitlab>/api/v4
docker compose -f deploy/compose.example.yml --profile gitlab up -d
```

Key env: `STREAMABLE_HTTP=true`, `REMOTE_AUTHORIZATION=true` (per-request token,
not a fixed PAT), `GITLAB_API_URL=https://<your-gitlab>/api/v4`,
`GITLAB_READ_ONLY_MODE=true` (set `false` to allow writes). Endpoint: `/mcp`
(container port 3002).

Then in the gateway: **/admin/connectors → GitLab (self-managed / CE)** → set the
MCP server URL (`http://gitlab-mcp:3002/mcp` full-stack, or
`http://localhost:3333/mcp` for a native gateway) → Save → Enable. Each user
connects at **/integrations** and pastes a GitLab **personal access token**
(scope `api`, or `read_api` for read-only).

## Sandbox (code execution)

The runner spawns each job as a single-use gVisor (runsc) sandbox. Its security
model, the gVisor install, and the isolation self-check are documented in
[`../docs/sandbox.md`](../docs/sandbox.md). Two deploy shapes:

- **Podman host (recommended):** the runner is a *host* systemd service
  ([`sandbox/sandbox-runner.service`](sandbox/sandbox-runner.service)) so it can
  pass `--runtime runsc` to local podman. Run [`sandbox/setup-sandbox.sh`](sandbox/setup-sandbox.sh).
- **Docker host:** the runner *can* run as a container (the `sandbox` compose
  profile) because Docker honors `--runtime` over its socket — it drives the host
  Docker socket with `SANDBOX_PODMAN=docker`, `SANDBOX_RUNTIME=runsc`. Requires
  gVisor registered as a Docker runtime (`runsc install`). The boot self-check
  logs `SANDBOX IS NOT ISOLATED` if the runtime didn't apply — treat as a hard
  stop. On Docker Desktop / macOS (no gVisor) use `SANDBOX_RUNTIME=local-unsafe`
  for dev only — never in a deployment.

Point the gateway at the runner via `[sandbox] runner_url` in the config TOML
(`http://sandbox-runner:9000` on the compose network, or the podman bridge IP for
the host-service path).

---

## Upgrades & security

- **Image pinning:** both Compose and Quadlet treat the image tag as the source
  of truth and won't re-pull `:latest` on restart. Pin a digest or a `:<git-sha>`
  tag in production; `docker compose pull` / `podman pull` + restart to update.
- **State** survives image swaps (named volume).
- **Never expose** the sandbox-runner port (arbitrary code execution) or bind the
  podman/docker socket on anything public; front any cross-host runner hop with
  mTLS.
- Grant the sandbox + connector tools deliberately — they're off by default and
  gated per role (RBAC) and per token.
