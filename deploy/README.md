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
| **gitlab-mcp** | `docker.io/zereight050/gitlab-mcp` | Community bridge backing the per-user **GitLab (self-managed / CE)** connector; forwards each request's bearer as that user's GitLab PAT. Optional. |
| **discord-mcp** | `ghcr.io/croit/discord-mcp` | Discord bot bridge (channel + DM tools, plus full-roster cache + `fuzz_search_members`) backing the seeded **global** Discord connector (enabled + pointed at this bridge in `/admin/connectors`, see below). Our fork of `SaseQ/discord-mcp`. Optional. |
| **sandbox-runner** | `ghcr.io/croit/llm-gateway-sandbox-runner` | Code-execution runner (`run_in_sandbox` etc.). Optional; needs gVisor. |
| **egress-proxy** | `docker.io/ubuntu/squid` | Allowlisting proxy for networked sandbox runs. Optional. |
| **ocr-sidecar** | local `deploy/ocr-sidecar` image | PDF-aware Unlimited-OCR adapter. Optional; needs an external Unlimited-OCR vLLM service. |
| sandbox workload | `ghcr.io/croit/llm-gateway-sandbox` | The "gold image" the runner spawns per job (pulled by the runner, not run directly). |

Per-host secrets live in env files; everything an operator would once have put
in a config TOML now lives in the database and is edited in the browser. The
SQLite DB (also the session store) lives in a named volume. Real secret files
(`gateway.env`, `google-workspace-mcp.env`) are git-ignored — only the
`*.example.*` templates are committed. A `gateway.toml` is optional and only
used to migrate an older install (see below); it is git-ignored too.

---

## Quick start — Docker Compose

```bash
# from the repo root
printf 'GATEWAY_SESSION_KEY=%s\n' "$(openssl rand -hex 32)" > deploy/gateway.env
docker compose -f deploy/compose.example.yml up -d gateway
```

Then open the gateway and finish the **setup wizard** — it asks for your OIDC
provider, proves it with a real sign-in, and hands you an admin account. There
is no config file to write: pools, backends, models and groups are all managed
in the signed-in UI afterwards.

Generate `GATEWAY_SESSION_KEY` once and keep it for the life of the deployment.
It signs sessions *and* derives the key that seals every secret in the database,
so back it up together with the volume. The gateway refuses to boot without it.

Optional extras:

```bash
cp deploy/quadlet/google-workspace-mcp.example.env deploy/google-workspace-mcp.env
$EDITOR deploy/google-workspace-mcp.env
docker compose -f deploy/compose.example.yml up -d                 # gateway + workspace MCP
docker compose -f deploy/compose.example.yml --profile sandbox up -d  # + sandbox runner + egress
OCR_VLLM_BASE_URL=http://host.docker.internal:8000/v1 \
  docker compose -f deploy/compose.example.yml --profile ocr up -d  # + PDF OCR sidecar
```

A `gateway.toml` is not needed. OCR, ComfyUI, the sandbox, Typst, skills, GeoIP
and RAG tuning are all configured at `/admin/settings`, and the two remaining
file keys have environment equivalents (`$GATEWAY_DB_PATH`,
`$GATEWAY_BOOTSTRAP_ADMIN_GROUPS`). Mount one only to migrate an older install:
whatever it contains is imported into the database on the first boot and ignored
afterwards. Copy `gateway.example.toml` to `deploy/gateway.toml` and uncomment
the mount in the compose file for that.

### Locked out?

```bash
docker compose -f deploy/compose.example.yml exec gateway restore-setup
podman exec gateway restore-setup     # quadlet
```

Reopens the setup wizard for 30 minutes and prints a one-time link. The gateway
keeps serving the whole time — nobody is logged out, nothing is deleted, and the
wizard comes up pre-filled with the current provider.

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
  proxy (Caddy/Traefik/nginx). The setup wizard pre-fills the public URL from
  the request (honouring `X-Forwarded-Proto`) and shows the exact
  `<public_url>/auth/callback` to register with your provider.
- **State:** nothing to configure. The image sets
  `GATEWAY_DATA_DIR=/var/lib/gateway`, so the SQLite database and the RAG index
  store both land on the named volume and survive image swaps.
- **Secrets** (`gateway.env`): `GATEWAY_SESSION_KEY` — required, and the only
  one. Optionally `GATEWAY_ENCRYPTION_KEY` to decouple at-rest encryption from
  session signing. The OIDC client secret and backend API keys are entered in
  the UI and stored encrypted in the database.

## Document OCR

The OCR sidecar is inactive unless all of the following are true:

1. The sidecar is running with `OCR_VLLM_BASE_URL` pointing at an Unlimited-OCR vLLM server.
2. An `ocr` pool and backend are configured at `/admin/upstreams`, pointing at `http://ocr-sidecar:9100` with `baidu/Unlimited-OCR` in its model list.
3. `chat.ocr.enabled` is turned on at `/admin/settings`.

Without an available `ocr` backend the gateway neither fetches attachments for OCR nor sends OCR tools or models to an LLM. The sidecar accepts the original PDF/image at `/ocr`, converts PDFs internally, and calls vLLM with the model-specific request recipe.

Operationally worth knowing:

- Results are cached in the gateway's `ocr_derivatives` table by document hash + model + prompt version + settings, so a document costs one OCR run no matter how many turns reference it, and the cache survives restarts. Changing `dpi`, `max_tokens`, `ngram_window`, `max_pages`, or `max_output_chars` invalidates it by design.
- The sidecar issues one inference call **per page** by default (page numbers survive, one bad page doesn't lose the document). `OCR_MULTI_IMAGE=1` switches to one call per document.
- OCR work is metered like any upstream call: `usage_events` rows with `kind = "ocr"`, tokens from the sidecar, and pages in `input_units`. Cache hits cost nothing and are not recorded.
- `[chat.ocr] max_concurrency` bounds documents in flight gateway-wide; everything else queues (visibly, in the chat UI).

See [`../docs/ocr.md`](../docs/ocr.md).

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
| `WORKSPACE_MCP_OAUTH_PROXY_STORAGE_BACKEND` | `disk` | **Required.** See *OAuth state must survive restarts* below. |
| `WORKSPACE_MCP_OAUTH_PROXY_DISK_DIRECTORY` | `/var/lib/gworkspace-mcp/oauth-proxy` | Path **on the mounted volume**. |
| `FASTMCP_SERVER_AUTH_GOOGLE_JWT_SIGNING_KEY` | `openssl rand -hex 32` | Secret (env file). Signs issued tokens and encrypts the store. |

Do **not** set a `command:`/`Exec=` override — it would be parsed as
`sh -c --transport …` and fail.

#### OAuth state must survive restarts

This server *is* the authorization server, and its FastMCP OAuth proxy keeps
everything in one store: the client the gateway registered via DCR, the
authorization codes, the refresh tokens it issues to the gateway, and the
upstream Google tokens. Left unconfigured it writes them under `$HOME` **inside
the container** — and Quadlet recreates the container on every `systemctl
restart` (as does `--force-recreate` / an image update), destroying it.

The symptom is unmistakable once you know it: **every** user's card flips to
*Needs reconnect* with

```
token refresh: token exchange failed: provider rejected the request
  — invalid_client: Invalid client_id
```

within ~30 minutes of the restart (that's the server's access-token lifetime).
The gateway is fine; the server simply no longer knows the client it issued.

So the shipped units mount a named volume (`gworkspace-mcp-oauth.volume` for
Quadlet, `gworkspace-mcp-oauth` for Compose) and point the store at it with the
three env vars above. The signing key matters too: unset, it's derived from
`GOOGLE_OAUTH_CLIENT_SECRET`, so rotating the Google secret wipes the store in
the same way. Set it once and back it up. The volume holds live Google refresh
tokens (encrypted at rest) — treat it as secret material; deleting it forces
everyone to reconnect.

Running more than one replica? Use `…_STORAGE_BACKEND=valkey` with
`WORKSPACE_MCP_OAUTH_PROXY_VALKEY_HOST` — the disk store is single-node.

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
`GITLAB_PERMISSION_MODE=readonly` (or `modify` / `full` to allow writes).
Endpoint: `/mcp` (container port 3002).

**DNS-rebinding guard:** the bridge auto-allows only **loopback** hosts + its
bound address, so when the full-stack gateway reaches it by its network name it
returns `HTTP 403 "Host header is not allowed"`. Set `MCP_ALLOWED_HOSTS` to that
`host:port` (matching the connector URL's authority) — the compose/Quadlet units
ship `MCP_ALLOWED_HOSTS=gitlab-mcp:3002`. The native-gateway loopback URL below
needs nothing, as loopback is always allowed.

Then in the gateway: **/admin/connectors → GitLab (self-managed / CE)** → set the
MCP server URL (`http://gitlab-mcp:3002/mcp` full-stack, or
`http://localhost:3333/mcp` for a native gateway) → Save → Enable. Each user
connects at **/integrations** and pastes a GitLab **personal access token**
(scope `api`, or `read_api` for read-only).

---

## Discord

Discord is a **global** connector: a Discord bot authenticates with a single
**bot token** for the whole server/guild, not a per-user OAuth account, so
there's no "each user connects their own Discord account" flow the way there is
for Slack, GitHub, or Atlassian. One bot, shared by everyone the gateway's RBAC
grants the `mcp__discord__*` tools to (still individually toggleable
always/ask/off on `/tools`, same as any other tool).

It ships as a seeded, disabled connector in the catalog. There are two moving
parts: run the sidecar **bridge** (below), then **enable + point it** at the
bridge in `/admin/connectors` — no `gateway.toml` edit, no restart.

**Create the bot** (once, in the [Discord Developer Portal](https://discord.com/developers/applications)):

1. **New Application** → note the **Application ID**.
2. **Bot** tab → **Reset Token** (copy it — this is `DISCORD_TOKEN`) → enable
   **Message Content Intent**, **Server Members Intent**, and **Presence
   Intent**.
3. **OAuth2 → URL Generator** → scope `bot` → permissions: Send Messages,
   Create/Send in Public Threads, Manage Messages, Manage Threads, Manage
   Channels, Manage Webhooks, Manage Roles, Add Reactions, View Channel (or
   just `Administrator` for simplicity) → open the generated URL and invite
   the bot to your server.

**Run the bridge** — [`croit/discord-mcp`](https://github.com/croit/discord-mcp)
(image `ghcr.io/croit/discord-mcp:latest`), our fork of
[`SaseQ/discord-mcp`](https://github.com/SaseQ/discord-mcp). We use it because it
exposes **DM tools** (`send_private_message`) plus channel messaging. The fork
adds what upstream lacks for day-to-day use: it caches the full guild roster
(upstream never chunks it, so member search hit a near-empty cache) and adds
**`fuzz_search_members`** — a fuzzy lookup across server nickname, account
username *and* global display name, so you can resolve a person by their real
name instead of guessing their exact `@handle`. This needs the **Server Members
Intent** (step 2 above) enabled — without it the roster never loads. It defaults
to stdio, so set
**`SPRING_PROFILES_ACTIVE=http`** to make it serve streamable HTTP on **:8085**
at `/mcp` (the compose/Quadlet configs below already do this). Optionally set
`DISCORD_GUILD_ID` as a default server. (Do *not* use the `mcp/mcp-discord`
verified image — it's a different, stdio-only project the gateway can't reach
over HTTP.)

Compose (`discord` profile):

```bash
cp deploy/quadlet/discord-mcp.example.env deploy/discord-mcp.env
$EDITOR deploy/discord-mcp.env                       # DISCORD_TOKEN=...
docker compose -f deploy/compose.example.yml --profile discord up -d
```

Quadlet ([`quadlet/discord-mcp.container`](quadlet/discord-mcp.container)) — it
joins the gateway's `llm` network so the gateway resolves it by name:

```bash
sudo cp deploy/quadlet/discord-mcp.container /etc/containers/systemd/
sudo install -m 0600 deploy/quadlet/discord-mcp.example.env /etc/gateway/discord-mcp.env
sudo $EDITOR /etc/gateway/discord-mcp.env            # DISCORD_TOKEN=...
sudo systemctl daemon-reload
sudo systemctl enable --now discord-mcp.service
```

Endpoint: `/mcp` (container port 8085). Keep it internal-only (private network),
never exposed publicly: the bot token grants full bot access with no per-caller
scoping. **The bridge must share a DNS-enabled network with the gateway** — the
`llm` network for Quadlet, the compose network by service name — or the gateway
can't resolve `discord-mcp`.

**Enable it** in the gateway UI (as an admin): open **`/admin/connectors`**,
find **Discord**, click **Edit**, set the **URL** to
`http://discord-mcp:8085/mcp` (compose service name or Quadlet `llm` network) or
`http://127.0.0.1:3334/mcp` (native gateway + the loopback port published above),
save, then **Enable**. The connector's auth is **No auth** (the gateway sends no
credentials — the bot token lives in the bridge), its scope is **Global**, and
it ships **audited** — every tool call is logged (the **Audit log** button on
its row). Its tools are then available to everyone the connector's role allows.

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
