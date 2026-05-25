#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# smoke-install.sh — install a packaged thurvtl / thurvsa artifact,
# start the daemon, run one verb, tear down.
#
# Used by .github/workflows/release.yml to gate every distro x product
# cell before publish. Also runnable locally inside a throwaway
# container:
#
#   podman run --rm -v $PWD:/work -w /work debian:12 \
#       release/smoke-install.sh vtl release-artifacts/thurvtl_*.deb
#
# Dispatches on file extension. Expects root: postinst creates the
# system user, drops conffile perms, and binds sockets under /run.
# No systemd dependency — execs the daemon binary directly.

set -euo pipefail

usage() { echo "usage: $0 <vtl|vsa> <package.deb|.rpm>" >&2; exit 2; }

[ $# -eq 2 ] || usage
PRODUCT="$1"
PKG="$2"
[ -f "$PKG" ] || { echo "error: package not found: $PKG" >&2; exit 1; }
# Resolve to an absolute path: `apt-get install foo.deb` (without a
# leading / or ./) looks `foo.deb` up as a package name in the index.
PKG="$(readlink -f "$PKG")"
case "$PRODUCT" in vtl|vsa) ;; *) usage ;; esac
[ "$(id -u)" -eq 0 ] || {
    echo "error: must run as root (need to install the package + bind /run sockets)." >&2; exit 1; }

# ---- install ----
echo "==> installing $PKG"
case "$PKG" in
    *.deb)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        # apt handles dep resolution for a local .deb when given a path.
        apt-get install -y --no-install-recommends "$PKG"
        ;;
    *.rpm)
        if   command -v dnf    >/dev/null 2>&1; then dnf install -y "$PKG"
        elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install --allow-unsigned-rpm "$PKG"
        else                                         rpm -Uvh "$PKG"
        fi
        ;;
    *) echo "error: unrecognized package extension (need .deb or .rpm): $PKG" >&2; exit 1 ;;
esac

# ---- per-product setup ----
case "$PRODUCT" in
    vtl)
        DAEMON=/usr/bin/thurvtld; CLI=/usr/bin/thurvtl
        CONF=/etc/thurvtl/thurvtl.yaml; DATA=/var/lib/thurvtl
        SOCK=/run/thurvtl/admin.sock;   USER=thurvtl
        cat > "$CONF" <<'EOF'
data_dir: /var/lib/thurvtl
library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8
iscsi:
  listen: "127.0.0.1:3260"
http:
  listen: "127.0.0.1:9090"
cloud:
  backends:
    smoke:
      type: local
      root_dir: /var/lib/thurvtl/local
EOF
        ;;
    vsa)
        DAEMON=/usr/bin/thurvsad; CLI=/usr/bin/thurvsa
        CONF=/etc/thurvsa/thurvsa.yaml; DATA=/var/lib/thurvsa
        SOCK=/run/thurvsa/admin.sock;   USER=thurvsa
        cat > "$CONF" <<'EOF'
data_dir: /var/lib/thurvsa
iscsi:
  listen: "127.0.0.1:3260"
http:
  listen: "127.0.0.1:9090"
cloud:
  backends:
    smoke:
      type: local
      root_dir: /var/lib/thurvsa/local
EOF
        ;;
esac

chown root:"$USER" "$CONF"
chmod 0640 "$CONF"
install -d -m 0750 -o "$USER" -g "$USER" "$DATA" "$DATA/local"
install -d -m 0755 -o "$USER" -g "$USER" "/run/$USER"

# ---- start daemon ----
LOG="/tmp/${PRODUCT}d.log"
echo "==> starting $DAEMON (logs: $LOG)"
runuser -u "$USER" -- "$DAEMON" --config "$CONF" >"$LOG" 2>&1 &
DAEMON_PID=$!
cleanup() {
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> waiting for admin socket $SOCK"
for _ in $(seq 1 60); do
    [ -S "$SOCK" ] && break
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "error: daemon exited before socket appeared. tail of log:" >&2
        tail -80 "$LOG" >&2
        exit 1
    fi
    sleep 1
done
if [ ! -S "$SOCK" ]; then
    echo "error: admin socket never appeared after 60s. tail of log:" >&2
    tail -80 "$LOG" >&2
    exit 1
fi

# ---- one verb per product ----
echo "==> running smoke verb"
case "$PRODUCT" in
    vtl) runuser -u "$USER" -- "$CLI" cartridge create SMOKE01 --backend smoke ;;
    vsa) runuser -u "$USER" -- "$CLI" volume     create smoke   --size 10G --backend smoke ;;
esac

echo "==> smoke OK ($PRODUCT on $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" || echo "unknown distro"))"
