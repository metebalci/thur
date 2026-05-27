#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end smoke against the `kmip` keystore backend using a local
# PyKMIP server. Zero-prereqs except `python3` and a Rust toolchain —
# the script bootstraps everything else on first run:
#
#   1. Create a venv at /tmp/thurvsa-kmip-venv/ (override via
#      THURVSA_KMIP_VENV=...). Install pykmip + cryptography if
#      missing.
#   2. Generate a self-signed CA + server cert + client cert at
#      /tmp/thurvsa-kmip/ (server SAN covers both localhost and
#      127.0.0.1) on first run; reuses them on later runs.
#   3. Start pykmip-server.py in the background, wait for the port.
#   4. Provision an AES-256 KEK via pykmip-create-kek.py, capture UID.
#   5. Run the ignored `kmip_pykmip` integration tests in
#      shared/keystore/tests/, feeding the UID via env.
#   6. Stop the server on exit (success OR failure).
#
# Usage (from repo root):
#   ./vsa/scripts/test-keystore-kmip.sh [--release]
#
# Re-running is cheap: the venv + certs survive, only the pykmip.db
# is wiped so each run starts with a clean KEK set.

set -u

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)

CERT_DIR=/tmp/thurvsa-kmip
HOST=127.0.0.1
PORT=5696

# Venv discovery: explicit env > existing /tmp default > user's ~/kmip
# (a common dev pattern) > try to create a fresh /tmp venv. The last
# step needs `python3-venv` (Debian/Ubuntu) or equivalent — falls
# through with a clear error if `python3 -m venv` isn't usable.
pick_venv() {
    if [[ -n "${THURVSA_KMIP_VENV:-}" ]]; then
        echo "$THURVSA_KMIP_VENV"
        return
    fi
    if [[ -x /tmp/thurvsa-kmip-venv/bin/python3 ]]; then
        echo "/tmp/thurvsa-kmip-venv"
        return
    fi
    if [[ -x "$HOME/kmip/bin/python3" ]] \
        && "$HOME/kmip/bin/python3" -c "import kmip" 2>/dev/null; then
        echo "$HOME/kmip"
        return
    fi
    echo "/tmp/thurvsa-kmip-venv"
}
VENV=$(pick_venv)

CARGO_PROFILE=""
for arg in "$@"; do
    case "$arg" in
        --release) CARGO_PROFILE="--release" ;;
        -h|--help)
            sed -n '3,28p' "$0" | sed 's/^# //; s/^#$//'
            exit 0
            ;;
        *)
            echo "[!] unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

log() { echo "[$(date +%T)] $*"; }
die() { echo "[!] $*" >&2; exit 1; }

# --------------------------------------------------------------------
# 1. venv bootstrap
# --------------------------------------------------------------------
if [[ ! -x "$VENV/bin/python3" ]]; then
    log "no usable venv found — creating fresh one at $VENV"
    # Prefer `virtualenv` over `python3 -m venv` because the venv module
    # depends on `ensurepip`, which Debian/Ubuntu split into the
    # `python3-venv` package. `virtualenv` bundles its own pip wheel and
    # works on minimal Python installs out of the box.
    if command -v virtualenv >/dev/null; then
        virtualenv -q "$VENV" || die "virtualenv failed"
    elif python3 -c "import ensurepip" 2>/dev/null; then
        python3 -m venv "$VENV" || die "python3 -m venv failed"
    else
        die "no venv builder available — install \`virtualenv\` \
(pip install --user virtualenv) or python3-venv (apt install python3-venv), \
or point THURVSA_KMIP_VENV at an existing venv that has pykmip installed"
    fi
fi
PY="$VENV/bin/python3"

if ! "$PY" -c "import kmip, cryptography" 2>/dev/null; then
    log "installing pykmip + cryptography into $VENV (one-time)"
    "$VENV/bin/pip" install --quiet --upgrade pip || die "pip upgrade failed"
    "$VENV/bin/pip" install --quiet pykmip cryptography || die "pip install pykmip failed"
fi

PYKMIP_VER=$("$PY" -c "import kmip; print(kmip.__version__)")
log "using pykmip $PYKMIP_VER from $VENV"

# --------------------------------------------------------------------
# 2. wipe the pykmip database so each run starts with no KEKs
# --------------------------------------------------------------------
# pykmip-server.py regenerates the .db on startup. Certs survive.
rm -f "$CERT_DIR/pykmip.db"

# --------------------------------------------------------------------
# 3. start the server in the background
# --------------------------------------------------------------------
SERVER_LOG=$(mktemp /tmp/thurvsa-kmip-server.XXXXXX.log)
log "starting pykmip-server.py (log: $SERVER_LOG)"
"$PY" "$HERE/pykmip-server.py" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

cleanup() {
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        log "stopping pykmip server (pid $SERVER_PID)"
        kill "$SERVER_PID" 2>/dev/null || true
        # Give the server up to 5 seconds to exit cleanly.
        for _ in 1 2 3 4 5; do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Wait for the port to accept connections (bash /dev/tcp probe).
log "waiting for $HOST:$PORT to bind"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if (exec 3<>/dev/tcp/$HOST/$PORT) 2>/dev/null; then
        exec 3<&- 3>&- || true
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "[!] server died before binding — log tail:" >&2
        tail -20 "$SERVER_LOG" >&2
        exit 1
    fi
    sleep 1
done
if ! (exec 3<>/dev/tcp/$HOST/$PORT) 2>/dev/null; then
    echo "[!] $HOST:$PORT never bound — log tail:" >&2
    tail -30 "$SERVER_LOG" >&2
    exit 1
fi
exec 3<&- 3>&- || true
log "server bound at $HOST:$PORT"

# --------------------------------------------------------------------
# 4. provision an AES-256 KEK
# --------------------------------------------------------------------
log "provisioning AES-256 KEK"
KEK_UID=$("$PY" "$HERE/pykmip-create-kek.py") || {
    echo "[!] pykmip-create-kek.py failed — server log tail:" >&2
    tail -20 "$SERVER_LOG" >&2
    exit 1
}
log "KEK uid=$KEK_UID"

# --------------------------------------------------------------------
# 5. run the cargo integration tests
# --------------------------------------------------------------------
log "running cargo tests (shared-keystore --test kmip_pykmip)"
cd "$REPO"
THURVSA_KMIP_KEK_UID="$KEK_UID" \
    cargo test $CARGO_PROFILE -p shared-keystore --test kmip_pykmip \
        -- --ignored --nocapture --test-threads=1
RC=$?

if [[ $RC -eq 0 ]]; then
    log "ALL GREEN — kmip backend round-trip + uuid-mismatch refusal verified against PyKMIP"
else
    echo "[!] cargo test exit $RC — server log tail:" >&2
    tail -30 "$SERVER_LOG" >&2
fi
exit $RC
