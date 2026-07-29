#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (C) 2026 croit GmbH
#
# One-shot installer for the LLM-gateway code-execution sandbox on a Linux
# host (rootful podman). Run as root from a repo checkout:
#
#   sudo deploy/sandbox/setup-sandbox.sh            # sandbox only (no network)
#   sudo deploy/sandbox/setup-sandbox.sh --egress   # also wire the egress proxy
#   sudo deploy/sandbox/setup-sandbox.sh --work-image 100G   # host-backed scratch
#
# --work-image SIZE puts each sandbox's /work and /tmp on a host filesystem
# instead of gVisor-internal tmpfs, which is what makes large produced files
# (video renders) practical: the runner then reads them straight off disk rather
# than shipping them back through 64 KiB `podman exec` reads (~426 KiB/s).
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
WORK_IMAGE_SIZE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --egress) EGRESS=1 ;;
        --work-image) WORK_IMAGE_SIZE="${2:?--work-image needs a size, e.g. 100G}"; shift ;;
        --work-image=*) WORK_IMAGE_SIZE="${1#*=}" ;;
        *) echo "error: unknown argument $1" >&2; exit 1 ;;
    esac
    shift
done

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root (sudo $0)" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QUADLET_DIR="$SCRIPT_DIR/../quadlet"
SYSTEMD_DIR=/etc/containers/systemd          # Quadlet units (network, egress proxy)
UNIT_DIR=/etc/systemd/system                 # native units (the host runner)
CONF_DIR=/etc/gateway/sandbox
# Host-backed sandbox scratch. The image is SPARSE (thin provisioned): it only
# occupies what is actually stored, while its ext4 size is the hard ceiling a
# runaway job can reach — which is the whole point, since a bind mount has no
# `size=` of its own and would otherwise let a job fill the root filesystem.
# Override SANDBOX_WORK_IMAGE to park the image on a roomier filesystem than /
# (it is thin, but its ceiling must fit): SANDBOX_WORK_IMAGE=/data/sandbox.img
WORK_IMAGE="${SANDBOX_WORK_IMAGE:-/var/lib/sandbox-work.img}"
WORK_ROOT="${SANDBOX_WORK_ROOT_DIR:-/var/lib/sandbox-work}"
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


if [ -n "$WORK_IMAGE_SIZE" ]; then
    echo "==> Host-backed sandbox scratch: ${WORK_IMAGE} (${WORK_IMAGE_SIZE}, sparse) on ${WORK_ROOT}"
    command -v mkfs.ext4 >/dev/null || { echo "error: mkfs.ext4 not found (install e2fsprogs)" >&2; exit 1; }
    install -d -m 0700 "$WORK_ROOT"

    if [ -e "$WORK_IMAGE" ]; then
        echo "    image exists — leaving it alone (delete it to resize)"
    else
        # truncate creates a hole-only file: `ls -l` shows the full size, `du`
        # shows ~0 until data lands. -m 0 drops the 5% root reserve and
        # -T largefile4 gives one inode per 4 MiB, so the metadata a fresh
        # filesystem writes stays small — thin provisioning stays thin.
        truncate -s "$WORK_IMAGE_SIZE" "$WORK_IMAGE"
        chmod 0600 "$WORK_IMAGE"
        mkfs.ext4 -q -F -m 0 -T largefile4 -L sandbox-work "$WORK_IMAGE"
        echo "    formatted; on-disk use so far: $(du -sh --apparent-size=never "$WORK_IMAGE" 2>/dev/null | cut -f1 || du -sh "$WORK_IMAGE" | cut -f1)"
    fi

    # A .mount unit rather than a manual `mount`, so it survives reboots and
    # systemd can order the runner after it.
    #
    # `discard` is what keeps a thin image thin: deletions inside ext4 punch
    # holes back into the sparse file, so freed scratch really does return to
    # the host. (Debian's fstrim.timer covers it too, but only weekly.)
    #
    # NOT noexec: the sandbox's /tmp is bind-mounted from here and chromium /
    # LibreOffice drop helper binaries into it. A noexec backing filesystem
    # cannot be overridden by the container's mount flags and would break them.
    MOUNT_UNIT="$(systemd-escape -p --suffix=mount "$WORK_ROOT")"
    cat > "$UNIT_DIR/$MOUNT_UNIT" <<UNIT
# SPDX-License-Identifier: AGPL-3.0-only
# Written by deploy/sandbox/setup-sandbox.sh — sandbox scratch filesystem.
[Unit]
Description=LLM Gateway sandbox scratch (thin-provisioned loop image)
Documentation=https://github.com/croit/llm-gateway/blob/main/docs/sandbox.md

[Mount]
What=$WORK_IMAGE
Where=$WORK_ROOT
Type=ext4
Options=loop,discard,noatime,nodev,nosuid

[Install]
WantedBy=multi-user.target
UNIT

    # Point the runner at it and refuse to start without the mount: an
    # unmounted WORK_ROOT is a plain directory on /, i.e. silently no quota.
    sed -i \
        -e "s|^#\?Environment=SANDBOX_WORK_ROOT=.*|Environment=SANDBOX_WORK_ROOT=${WORK_ROOT}|" \
        -e "s|^#\?RequiresMountsFor=.*|RequiresMountsFor=${WORK_ROOT}|" \
        "$UNIT_DIR/sandbox-runner.service"
fi

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
if [ -n "$WORK_IMAGE_SIZE" ]; then
    systemctl enable --now "$(systemd-escape -p --suffix=mount "$WORK_ROOT")"
    findmnt -no SOURCE,TARGET,FSTYPE,OPTIONS "$WORK_ROOT" || {
        echo "error: ${WORK_ROOT} did not mount — refusing to start the runner without its quota" >&2
        exit 1
    }
fi
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
