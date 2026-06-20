#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
#
# One-shot installer for the LLM-gateway code-execution sandbox on a Linux
# host (rootful podman). Run as root from a repo checkout:
#
#   sudo deploy/sandbox/setup-sandbox.sh            # sandbox only (no network)
#   sudo deploy/sandbox/setup-sandbox.sh --egress   # also wire the egress proxy
#
# It installs the shared network (a .network Quadlet) and the sandbox-runner as
# a HOST systemd service. The runner runs on the host — not as a container —
# because it must drive LOCAL podman to select the gVisor runtime; remote
# podman over the socket can't pass `--runtime`.
#
# Prerequisites it does NOT do for you (host-specific): install gVisor (runsc)
# and register the --network=host wrapper as a podman runtime — see
# docs/sandbox.md -> Installing a sandbox runtime.
set -euo pipefail

EGRESS=0
[ "${1:-}" = "--egress" ] && EGRESS=1

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root (sudo $0)" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QUADLET_DIR="$SCRIPT_DIR/../quadlet"
SYSTEMD_DIR=/etc/containers/systemd          # Quadlet units (network, egress proxy)
UNIT_DIR=/etc/systemd/system                 # native units (the host runner)
CONF_DIR=/etc/gateway/sandbox
RUNNER_IMAGE=ghcr.io/croit/llm-gateway-sandbox-runner:latest
SANDBOX_IMAGE=ghcr.io/croit/llm-gateway-sandbox:latest

command -v podman >/dev/null || { echo "error: podman not installed" >&2; exit 1; }
# The runner runs each job under gVisor (runsc). On rootful podman, runsc's
# default network mode fails ("cannot run with network enabled in root network
# namespace"), so it must be wrapped to pass --network=host — see
# docs/sandbox.md -> Installing a sandbox runtime.
if ! command -v runsc >/dev/null; then
    echo "WARNING: runsc (gVisor) not found — install it first (docs/sandbox.md -> Quick start)." >&2
    echo "         The runner will start but its boot isolation self-check will FAIL." >&2
elif ! grep -q '^runsc *=' /etc/containers/containers.conf 2>/dev/null; then
    echo "WARNING: runsc is installed but not registered under [engine.runtimes] in" >&2
    echo "         /etc/containers/containers.conf (must point at the --network=host wrapper)." >&2
    echo "         See docs/sandbox.md -> Installing a sandbox runtime. Isolation will FAIL otherwise." >&2
fi

echo "==> Pulling images"
podman pull "$RUNNER_IMAGE"
podman pull "$SANDBOX_IMAGE"

echo "==> Installing the runner binary (extracted from $RUNNER_IMAGE)"
cid="$(podman create "$RUNNER_IMAGE")"
podman cp "$cid":/usr/local/bin/sandbox-runner /usr/local/bin/sandbox-runner
podman rm "$cid" >/dev/null
chmod +x /usr/local/bin/sandbox-runner

# The runner binds the host's podman bridge gateway IP — reachable from the
# gateway container (which is already on that bridge) but not externally. Detect
# it from the default network so this works regardless of the host's subnet.
BRIDGE_IP="$(podman network inspect podman --format '{{(index .Subnets 0).Gateway}}' 2>/dev/null || true)"
[ -n "$BRIDGE_IP" ] || BRIDGE_IP=10.88.0.1
echo "==> Runner will bind ${BRIDGE_IP}:9000 (set runner_url=http://${BRIDGE_IP}:9000 in the gateway config)"

echo "==> Installing the host runner unit"
install -m 0644 "$SCRIPT_DIR/sandbox-runner.service" "$UNIT_DIR/"
sed -i "s|^Environment=SANDBOX_BIND=.*|Environment=SANDBOX_BIND=${BRIDGE_IP}:9000|" \
    "$UNIT_DIR/sandbox-runner.service"

if [ "$EGRESS" -eq 1 ]; then
    echo "==> Installing the egress proxy (allowlisted outbound)"
    install -d -m 0755 "$CONF_DIR"
    install -m 0644 "$QUADLET_DIR/squid.conf"             "$CONF_DIR/"
    install -m 0644 "$QUADLET_DIR/allowlist.txt"          "$CONF_DIR/"
    install -m 0644 "$QUADLET_DIR/sandbox-egress.network" "$SYSTEMD_DIR/"
    install -m 0644 "$QUADLET_DIR/egress-proxy.container" "$SYSTEMD_DIR/"
    # Point the runner at the proxy (uncomment the env lines in the unit).
    sed -i \
        -e 's|^#Environment=SANDBOX_EGRESS_NETWORK=.*|Environment=SANDBOX_EGRESS_NETWORK=sandbox-egress|' \
        -e 's|^#Environment=SANDBOX_EGRESS_PROXY=.*|Environment=SANDBOX_EGRESS_PROXY=http://egress-proxy:3128|' \
        "$UNIT_DIR/sandbox-runner.service"
fi

echo "==> Reloading systemd + enabling the runner"
systemctl daemon-reload
[ "$EGRESS" -eq 1 ] && systemctl start egress-proxy.service
systemctl enable --now sandbox-runner.service

cat <<EOF

Done. Next (no gateway network change needed — it already reaches ${BRIDGE_IP}):
  1. Add to the gateway config, then daemon-reload + restart gateway.service:
       [sandbox]
       runner_url = "http://${BRIDGE_IP}:9000"
  2. Confirm isolation actually applied:
       journalctl -u sandbox-runner.service | grep -i isolation
     Expect "isolation confirmed". If you see "SANDBOX IS NOT ISOLATED",
     fix gVisor before using the tool (see docs/sandbox.md -> Verify it works).
EOF
