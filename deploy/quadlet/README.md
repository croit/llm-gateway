# Quadlet deployment

systemd-podman unit files for running the LLM gateway as a system service on any host with podman ≥ 4.4 (RHEL 9 / Fedora 38 / Debian 13 / Ubuntu 24.04+).

Quadlet is the systemd-native way to manage Podman containers — you ship `.container` files that systemd's generator turns into `.service` units at boot.

## Layout

```
deploy/quadlet/
├── gateway.container                  # the gateway unit definition
├── gateway.volume                     # named volume for /var/lib/gateway
├── gateway.example.env                # template for gateway secrets
├── google-workspace-mcp.container     # optional: Google Workspace MCP sidecar
├── gworkspace-mcp-oauth.volume        #   its OAuth store (must persist!)
├── google-workspace-mcp.example.env
├── gitlab-mcp.container               # optional: GitLab (self-managed/CE) MCP bridge
├── gitlab-mcp.example.env
├── discord-mcp.container              # optional: Discord MCP bridge
├── discord-mcp.example.env
├── egress-proxy.container             # optional: allowlisting proxy for sandbox runs
├── squid.conf, allowlist.txt          # egress-proxy config
├── sandbox-egress.network             # network the sandbox + egress-proxy share
└── README.md                          # this file
```

The runtime config + secrets stay on the host at `/etc/gateway/`; the SQLite DB (which also holds the session store) lives in a Podman-managed named volume.

## Quick start

```bash
# As root on the target host:
sudo install -d -m 0750 -o root -g root /etc/gateway
sudo install -m 0644 deploy/quadlet/gateway.container /etc/containers/systemd/
sudo install -m 0644 deploy/quadlet/gateway.volume    /etc/containers/systemd/
sudo install -m 0600 deploy/quadlet/gateway.example.env /etc/gateway/gateway.env
sudo install -m 0640 gateway.example.toml             /etc/gateway/config.toml

# Fill in secrets + upstreams:
sudo $EDITOR /etc/gateway/gateway.env
sudo $EDITOR /etc/gateway/config.toml
```

Two edits in `config.toml` are mandatory when deploying via this Quadlet, plus one more if you use the RAG feature:

```toml
[db]
# Default is the relative path `gateway.sqlite`, which would land in /app
# (the container's WORKDIR) — ephemeral. Point it at the named volume
# instead so the DB survives image swaps.
path = "/var/lib/gateway/gateway.sqlite"

[gateway]
# Used to build the OIDC callback URL the IdP redirects to. Set this to
# whatever your reverse proxy exposes externally.
public_url = "https://gateway.example.com"

[rag]
# Required ONLY if you create RAG collections via /rag — the indexer
# writes per-collection usearch index files + a git clone cache here, so
# it MUST land on a writable filesystem. The container's rootfs is
# read-only; point this at a subdirectory of the same named volume that
# backs [db].path. The gateway will mkdir the leaf at startup.
data_dir = "/var/lib/gateway/rag"
```

Generate the service unit, then start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now gateway.service

# Logs + status:
journalctl -u gateway.service -f
systemctl status gateway.service
```

The first start pulls the image from the container registry (`ghcr.io/croit/llm-gateway`). After that, `systemctl restart gateway.service` is a fast restart against the cached image.

## Upgrading

Quadlet treats `Image=` as the source of truth — `:latest` will *not* be re-pulled on restart. Two choices:

- **Pin a digest** (production): edit the `Image=` line to `…@sha256:<digest>` or a content-tagged value like `…:<git-sha>`. CI publishes both `:<sha>` and `:latest`.
- **Force a pull**: `sudo podman pull <image>` then `sudo systemctl restart gateway.service`. Less hygienic; useful for staging.

The SQLite DB + session store live in the named volume, so they survive image swaps.

## Network

The default `PublishPort=127.0.0.1:8080:8080` only binds loopback — put a TLS-terminating reverse proxy in front (Caddy/Traefik/nginx). To expose 8080 publicly anyway, change to `PublishPort=8080:8080`, but you'll lose HTTPS + structured access logs.

The OIDC callback URL the gateway advertises is `<public_url>/auth/callback` from `[gateway].public_url` in `config.toml`. That URL must be reachable from your IdP and registered as an allowed redirect URI on the OIDC client.

## Google Workspace MCP server (optional sidecar)

The single **Google Workspace** connector (Gmail, Calendar, Drive, Docs, …) is
backed by a self-hosted [`taylorwilsdon/google_workspace_mcp`](https://github.com/taylorwilsdon/google_workspace_mcp)
server — Google's *hosted* MCP endpoints are gated behind a developer preview
and don't scale to per-user use (see [`docs/connectors.md`](../../docs/connectors.md)).
Ship it as a second Quadlet next to the gateway:

```bash
sudo cp deploy/quadlet/google-workspace-mcp.container \
        deploy/quadlet/gworkspace-mcp-oauth.volume /etc/containers/systemd/
sudo install -m 0600 deploy/quadlet/google-workspace-mcp.example.env \
     /etc/gateway/google-workspace-mcp.env
sudo $EDITOR /etc/gateway/google-workspace-mcp.env     # OAuth client + URLs + signing key
sudo systemctl daemon-reload
sudo systemctl enable --now google-workspace-mcp.service
```

**Don't drop the `.volume` unit.** This server is the authorization server for
the connector, and its OAuth proxy keeps the gateway's registered client, the
refresh tokens it issues and the upstream Google tokens in one on-disk store.
Without the volume that store lives in the container's writable layer, which
Quadlet discards on every `systemctl restart` — and then every user is
disconnected within ~30 minutes with `invalid_client: Invalid client_id`. Same
if `FASTMCP_SERVER_AUTH_GOOGLE_JWT_SIGNING_KEY` changes (unset, it derives from
the Google client secret). Details: [`../README.md`](../README.md#oauth-state-must-survive-restarts).

**This is not a purely internal sidecar.** The OAuth consent runs in the user's
browser (gateway → this server's `/authorize` → Google → this server's
`/oauth2callback` → back to the gateway), so the server's HTTP endpoint must be
**publicly reachable over TLS**. Give it its own vhost on the same reverse proxy,
e.g. Caddy:

```caddy
gworkspace-mcp.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

Then set `WORKSPACE_EXTERNAL_URL=https://gworkspace-mcp.example.com` in the env
file, add `https://gworkspace-mcp.example.com/oauth2callback` as the redirect URI
on the Google OAuth client, and in the gateway's `/admin/connectors` point the
**Google Workspace** connector's URL at `https://gworkspace-mcp.example.com/mcp/`
(DCR — no client id/secret in the gateway). The gateway reaches it over the same
public hostname, so no extra internal networking is needed.

## GitLab MCP bridge (self-managed / CE, optional sidecar)

GitLab's *native* MCP (`/api/v4/mcp`) is a Duo/Premium feature, so Community
Edition and other self-managed instances instead need the community bridge
[`zereight/gitlab-mcp`](https://github.com/zereight/gitlab-mcp), which backs
the **GitLab (self-managed / CE)** connector. It runs in streamable-HTTP +
remote-authorization mode — each MCP request carries the caller's own GitLab
token, forwarded to GitLab as that user's permissions — so it needs **no
public URL and no OAuth**; the gateway reaches it internally.

```bash
sudo cp deploy/quadlet/gitlab-mcp.container /etc/containers/systemd/
sudo install -m 0644 deploy/quadlet/gitlab-mcp.example.env /etc/gateway/gitlab-mcp.env
sudo $EDITOR /etc/gateway/gitlab-mcp.env               # GITLAB_API_URL=https://<your-gitlab>/api/v4
sudo systemctl daemon-reload
sudo systemctl enable --now gitlab-mcp.service
```

The unit joins `llm.network` (same as the gateway + other MCP sidecars) with no
published host port, so the gateway resolves it by name. It sets
`MCP_ALLOWED_HOSTS=gitlab-mcp:3002` — without it the bridge's DNS-rebinding guard
403s (`Host header is not allowed`) on any non-loopback host, and that network
name is non-loopback.

Then in the gateway's `/admin/connectors`, point **GitLab (self-managed / CE)**
at `http://gitlab-mcp:3002/mcp` → Save → Enable. Each user connects at
`/integrations` and pastes their own GitLab personal access token (scope
`api`, or `read_api` for read-only). Full details, including the compose
equivalent: [`../README.md`](../README.md#gitlab-self-managed--community-edition).

## Discord MCP bridge (optional sidecar)

Discord is a **global** connector — a Discord bot token authenticates one shared
bot for the whole server, not a per-user OAuth account. It ships seeded (and
disabled) in the catalog; an admin enables it and points it at this bridge in
`/admin/connectors`. The bridge, [`croit/discord-mcp`](https://github.com/croit/discord-mcp)
(our fork of `SaseQ/discord-mcp` — channel + DM tools plus full-roster cache and
`fuzz_search_members`), is a published GHCR image — no local build needed.
Member lookup needs the **Server Members Intent** enabled in the Discord
Developer Portal. The unit sets `SPRING_PROFILES_ACTIVE=http` (streamable HTTP on
:8085) and joins the gateway's `llm` network so it's reachable by name:

```bash
sudo cp deploy/quadlet/discord-mcp.container /etc/containers/systemd/
sudo install -m 0600 deploy/quadlet/discord-mcp.example.env /etc/gateway/discord-mcp.env
sudo $EDITOR /etc/gateway/discord-mcp.env              # DISCORD_TOKEN=... (Developer Portal → Bot)
sudo systemctl daemon-reload
sudo systemctl enable --now discord-mcp.service
```

Then, as a gateway admin, open **`/admin/connectors`**, **Enable** Discord and
set its **URL** to `http://discord-mcp:8085/mcp` (resolved over the `llm`
network) — no `gateway.toml` edit, no restart. No credential is configured on
the connector (the bot token lives in the container), and the endpoint stays
internal-only — it grants full bot access with no per-caller scoping. Bot
creation steps (intents, permissions, invite URL) and the compose equivalent:
[`../README.md`](../README.md#discord).

## Hardening

The unit already runs read-only, drops every capability, and sets `NoNewPrivileges=true`. The image runs as the unprivileged `gateway` (uid 1000). Anything writable is either the named volume (`/var/lib/gateway`) or a tmpfs (`/tmp`). If you add features that need to write elsewhere, add another `Tmpfs=` or `Volume=` rather than peeling back the `ReadOnly=true`.

## Troubleshooting

- **`systemctl daemon-reload` then nothing happens**: Quadlet only regenerates units on `daemon-reload`. Check `systemctl list-unit-files | grep gateway` to confirm the service appeared. If not, look for syntax errors with `/usr/libexec/podman/quadlet -dryrun`.
- **Container immediately exits**: `journalctl -u gateway.service` — most common cause is a missing `GATEWAY_SESSION_KEY` (sessions can't initialise) or an unparseable `/etc/gateway/config.toml`.
- **SELinux denials on the bind-mounted config**: the `:z` relabel on the Volume line handles this. If you removed it, run `sudo restorecon -v /etc/gateway/config.toml` or add `:Z` (per-container private label).
- **No in-container `HealthCmd`**: the runtime image is curl-free, so the unit relies on `Restart=on-failure` for crashes. Configure HTTP-level health probes on your reverse proxy (it can hit `/healthz` from outside).
