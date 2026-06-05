#!/usr/bin/env bash
# Dev helper: run a thurvtl OR thurvsa daemon from target/release against a
# throwaway /tmp data dir, with the read-only Web UI (#5) served LIVE from
# shared/admin-webui/assets/ — so you can preview UI edits with just a
# browser refresh, no package install and no root.
#
#   scripts/webui-demo.sh vtl|vsa     # start the chosen product, prints the URL
#   scripts/webui-demo.sh stop        # stop any running demo daemon
#
# Env overrides: PORT (9090), BIND (0.0.0.0), ISCSI_PORT (13260),
#                PASSWORD (demo-password-123, must be >= 12 chars).
#
# NOT for production: plaintext HTTP, a demo password, and a 0.0.0.0 bind.
# The admin password crosses the wire base64-encoded; this is a LAN/dev
# convenience only.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-}" in
  stop)
    pkill -x thurvtld 2>/dev/null && echo "stopped thurvtld" || true
    pkill -x thurvsad 2>/dev/null && echo "stopped thurvsad" || true
    exit 0
    ;;
  vtl | vsa)
    PROD="$1"
    ;;
  *)
    echo "usage: $0 {vtl|vsa|stop}" >&2
    exit 2
    ;;
esac

PORT="${PORT:-9090}"
BIND="${BIND:-0.0.0.0}"
ISCSI_PORT="${ISCSI_PORT:-13260}"
PASSWORD="${PASSWORD:-demo-password-123}"
ASSETS="$REPO/shared/admin-webui/assets"
D="/tmp/thur-webui-demo-$PROD"

if [[ "$PROD" == "vtl" ]]; then
  DAEMON=thurvtld; CLI=thurvtl; SOCK_ENV=THURVTL_ADMIN_SOCKET
else
  DAEMON=thurvsad; CLI=thurvsa; SOCK_ENV=THURVSA_ADMIN_SOCKET
fi

# Build the binaries on demand if they're not already there.
if [[ ! -x "$REPO/target/release/$DAEMON" || ! -x "$REPO/target/release/$CLI" ]]; then
  echo "building $DAEMON + $CLI (release)..."
  ( cd "$REPO" && cargo build --release -p "${PROD}-daemon" -p "${PROD}-cli" )
fi

# Fresh data dir each run.
pkill -x "$DAEMON" 2>/dev/null || true
sleep 1
rm -rf "$D"; mkdir -p "$D/data" "$D/backend"
export "$SOCK_ENV=$D/admin.sock"

if [[ "$PROD" == "vtl" ]]; then
  cat > "$D/$CLI.yaml" <<EOF
data_dir: "$D/data"
library: { num_slots: 40, num_drives: 2, lto_generation: 8 }
http:
  listen: "$BIND:$PORT"
  auth: { method: Password }
  webui:
    asset_dir: "$ASSETS"
iscsi: { listen: "127.0.0.1:$ISCSI_PORT" }
storage: { backends: { primary: { type: local, root_dir: "$D/backend" } } }
keystore: { backends: { local: { type: local } } }
EOF
else
  cat > "$D/$CLI.yaml" <<EOF
data_dir: "$D/data"
http:
  listen: "$BIND:$PORT"
  auth: { method: Password }
  webui:
    asset_dir: "$ASSETS"
iscsi: { listen: "127.0.0.1:$ISCSI_PORT" }
storage: { backends: { primary: { type: local, root_dir: "$D/backend" } } }
keystore: { backends: { local: { type: local } } }
EOF
fi

CFG="$D/$CLI.yaml"
nohup "$REPO/target/release/$DAEMON" --config "$CFG" >"$D/daemon.log" 2>&1 &
echo "started $DAEMON (pid $!), log: $D/daemon.log"

for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.2
done

PASSWORD_ENV="$(tr '[:lower:]' '[:upper:]' <<<"$PROD")"  # VTL / VSA
env "THUR${PASSWORD_ENV}_ADMIN_PASSWORD=$PASSWORD" \
  "$REPO/target/release/$CLI" --config "$CFG" system set-admin-password >/dev/null \
  && echo "admin password set: $PASSWORD"

# Seed a little inventory so the dashboard has content.
if [[ "$PROD" == "vtl" ]]; then
  "$REPO/target/release/$CLI" --config "$CFG" cartridge create DEMO0001L8 --lto-generation 8 >/dev/null 2>&1 || true
  "$REPO/target/release/$CLI" --config "$CFG" cartridge create DEMO0002L8 --lto-generation 8 >/dev/null 2>&1 || true
  "$REPO/target/release/$CLI" --config "$CFG" changer load 0 0 >/dev/null 2>&1 || true
else
  "$REPO/target/release/$CLI" --config "$CFG" volume create demo1 --size 1G >/dev/null 2>&1 || true
  "$REPO/target/release/$CLI" --config "$CFG" volume create demo2 --size 2G >/dev/null 2>&1 || true
fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo
echo "Web UI:  http://${IP:-127.0.0.1}:$PORT/ui/"
echo "         (also http://127.0.0.1:$PORT/ui/ on this host)"
echo "Login:   webadmin / $PASSWORD"
echo "Stop:    $0 stop   (or: pkill -x $DAEMON)"
