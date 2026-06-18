# Quadlet deployment

systemd-podman unit files for running the LLM gateway as a system service on any host with podman ≥ 4.4 (RHEL 9 / Fedora 38 / Debian 13 / Ubuntu 24.04+).

Quadlet is the systemd-native way to manage Podman containers — you ship `.container` files that systemd's generator turns into `.service` units at boot.

## Layout

```
deploy/quadlet/
├── gateway.container       # the unit definition
├── gateway.volume          # named volume for /var/lib/gateway
├── gateway.example.env     # template for secrets
└── README.md               # this file
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

## Hardening

The unit already runs read-only, drops every capability, and sets `NoNewPrivileges=true`. The image runs as the unprivileged `gateway` (uid 1000). Anything writable is either the named volume (`/var/lib/gateway`) or a tmpfs (`/tmp`). If you add features that need to write elsewhere, add another `Tmpfs=` or `Volume=` rather than peeling back the `ReadOnly=true`.

## Troubleshooting

- **`systemctl daemon-reload` then nothing happens**: Quadlet only regenerates units on `daemon-reload`. Check `systemctl list-unit-files | grep gateway` to confirm the service appeared. If not, look for syntax errors with `/usr/libexec/podman/quadlet -dryrun`.
- **Container immediately exits**: `journalctl -u gateway.service` — most common cause is a missing `GATEWAY_SESSION_KEY` (sessions can't initialise) or an unparseable `/etc/gateway/config.toml`.
- **SELinux denials on the bind-mounted config**: the `:z` relabel on the Volume line handles this. If you removed it, run `sudo restorecon -v /etc/gateway/config.toml` or add `:Z` (per-container private label).
- **No in-container `HealthCmd`**: the runtime image is curl-free, so the unit relies on `Restart=on-failure` for crashes. Configure HTTP-level health probes on your reverse proxy (it can hit `/healthz` from outside).
