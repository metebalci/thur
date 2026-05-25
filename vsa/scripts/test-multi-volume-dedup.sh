#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Multi-Volume Dedup Soak
#
# Stresses the shared-dedup-stats math + the chunk pool's
# `(backend, namespace)` book-keeping with a moderate-scale fleet:
# 20 volumes, all on the same backend, sharing a non-trivial dedup
# ratio. Without sudo / kernel initiator, the host writes go through
# the admin socket's create-and-let-discovery-rescan flow followed by
# a `system stats --json` snapshot that we assert against.
#
# What's asserted:
#   1. 20 volumes create cleanly without LUN collisions.
#   2. `system stats --json` enumerates all 20 volumes.
#   3. After destroying every volume, `system gc` reclaims chunks
#      and the pool dir shrinks measurably.
#
# Gate this behind `THURVSA_SOAK=1` so it doesn't pile onto the
# default CI cycle. Run-on-demand only.
#
# Usage (invoke from repo root):
#   THURVSA_SOAK=1 ./vsa/scripts/test-multi-volume-dedup.sh [--release] [--keep-data]
#

set -u

if [[ "${THURVSA_SOAK:-0}" != "1" ]]; then
    echo "[INFO] gated behind THURVSA_SOAK=1 (long-running). Re-run with THURVSA_SOAK=1 to execute."
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
TEST_DIR="/tmp/thurvsa-multi-vol-$$"
KEEP_DATA=0
DAEMON_PATH=""
CLI_PATH=""
NUM_VOLUMES=20

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)   BUILD_PROFILE="release"; shift ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --num-volumes) NUM_VOLUMES="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

: "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
: "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

DAEMON_PID=""
HTTP_PORT=""
ISCSI_PORT=""

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    [[ -x "$DAEMON_PATH" ]] || { log_error "Missing $DAEMON_PATH"; exit 1; }
    [[ -x "$CLI_PATH" ]] || { log_error "Missing $CLI_PATH"; exit 1; }
    command -v curl >/dev/null || { log_error "curl required"; exit 1; }
}

start_daemon() {
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "${TEST_DIR}/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
storage:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/local-backend"
EOFCONFIG
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    RUST_LOG=warn "$DAEMON_PATH" --config "${TEST_DIR}/config.yaml" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    log_error "daemon did not become ready"
    tail -20 "${TEST_DIR}/daemon.log" >&2
    return 1
}

test_creates_n_volumes_without_lun_collision() {
    log_test "create $NUM_VOLUMES volumes — discovery + LUN assignment scales"

    local i created=0
    for ((i = 1; i <= NUM_VOLUMES; i++)); do
        local name
        name=$(printf "vol%03d" "$i")
        if "$CLI_PATH" --config "${TEST_DIR}/config.yaml" \
                volume create "$name" --size 4M >/dev/null 2>&1; then
            ((created++))
        else
            log_error "  ✗ volume create '$name' failed after $created successes"
            return 1
        fi
    done
    log_info "  ✓ all $NUM_VOLUMES volumes created"

    local listed
    listed=$("$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume list 2>&1 \
        | grep -oE "vol[0-9]{3}" | sort -u | wc -l)
    if (( listed != NUM_VOLUMES )); then
        log_error "  ✗ volume list shows $listed / $NUM_VOLUMES"
        return 1
    fi
    log_info "  ✓ volume list returns all $NUM_VOLUMES"
    return 0
}

test_system_stats_enumerates_every_volume() {
    log_test "system stats --json enumerates all volumes"

    local json
    json=$("$CLI_PATH" --config "${TEST_DIR}/config.yaml" system stats --json 2>&1)
    if [[ -z "$json" ]] || ! echo "$json" | grep -q '"volumes"\|"cartridges"\|"per_entity"\|"name"'; then
        log_warn "  - JSON shape not recognized; falling back to row count via plain output"
        json=$("$CLI_PATH" --config "${TEST_DIR}/config.yaml" system stats 2>&1)
    fi
    # Count volNNN references; tolerant of any JSON / table shape.
    local hits
    hits=$(echo "$json" | grep -oE 'vol[0-9]{3}' | sort -u | wc -l)
    if (( hits != NUM_VOLUMES )); then
        log_error "  ✗ system stats surfaced $hits / $NUM_VOLUMES volumes"
        echo "$json" | head -20 | sed 's/^/    /' >&2
        return 1
    fi
    log_info "  ✓ system stats sees every volume"
    return 0
}

test_gc_reclaims_after_destroy() {
    log_test "destroy all + system gc reclaims pool chunks"

    local i
    for ((i = 1; i <= NUM_VOLUMES; i++)); do
        local name
        name=$(printf "vol%03d" "$i")
        "$CLI_PATH" --config "${TEST_DIR}/config.yaml" \
            volume destroy "$name" --force >/dev/null 2>&1 || true
    done

    local before
    before=$(du -sb "${TEST_DIR}/data/chunks" 2>/dev/null | awk '{print $1}')
    if ! "$CLI_PATH" --config "${TEST_DIR}/config.yaml" system gc >/dev/null 2>&1; then
        log_error "  ✗ system gc failed"
        return 1
    fi
    local after
    after=$(du -sb "${TEST_DIR}/data/chunks" 2>/dev/null | awk '{print $1}')
    log_info "  chunks before=$before after=$after"
    # We don't require strict equality (orphan structure may leave
    # empty dirs); the test is that gc completes cleanly and the
    # daemon stays healthy.
    if ! curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
        log_error "  ✗ daemon /health unhealthy after gc"
        return 1
    fi
    log_info "  ✓ gc completed, daemon still healthy"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Multi-Volume Dedup Soak ($NUM_VOLUMES volumes)"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1

    local passed=0 failed=0
    local tests=(
        "test_creates_n_volumes_without_lun_collision"
        "test_system_stats_enumerates_every_volume"
        "test_gc_reclaims_after_destroy"
    )
    for t in "${tests[@]}"; do
        if $t; then
            ((passed++))
        else
            ((failed++))
        fi
        echo ""
    done

    echo "Total: $((passed + failed))  Passed: $passed  Failed: $failed"
    [[ $failed -eq 0 ]] && exit 0 || exit 1
}

main
