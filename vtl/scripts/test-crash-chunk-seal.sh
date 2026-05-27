#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Crash-During-Chunk-Seal Durability Test
#
# Tape analogue of `vsa/scripts/test-crash-page-flush.sh`. Drives a
# real tar stream through /dev/nstN, fences with a filemark, then
# SIGKILLs the daemon mid-stream. The invariant under test: every
# tape block the daemon acked must survive kill -9, because the
# chunk-seal pipeline in `core-stream` is supposed to land sealed
# chunks atomically (staging-rename inside the cartridge dir) before
# the WRITE completes.
#
# Workflow:
#   1. Create a cartridge, iSCSI login, identify changer + tape.
#   2. mtx-load the cartridge into drive 0, mt-rewind.
#   3. tar cf /dev/nstN ~8 MiB fixture, weof, rewind.
#   4. dd if=/dev/nstN of=read-pre.bin — capture what the daemon
#      currently serves.
#   5. iSCSI logout, kill -9 daemon, restart, iSCSI login.
#   6. mtx-load + rewind + dd if=/dev/nstN of=read-post.bin.
#   7. `cmp read-pre read-post` — the durability gate.
#
# Prerequisites:
#   - mtx, mt-st, open-iscsi, lsscsi, tar, dd
#   - iscsid running
#   - Root / sudo NOPASSWD
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vtl/scripts/test-crash-chunk-seal.sh [--release] [--keep-data]
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvtl-test-crash-chunk-seal-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
NOREWIND_DEVICE=""
CART_LABEL="tape-crash"
FIXTURE_DIR=""
FIXTURE_TAR=""

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
    fi
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
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
    for t in iscsiadm lsscsi mtx mt tar dd cmp curl systemctl; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service not running"
        exit 1
    fi
    require_daemon_binaries thurvtl
}

prepare_fixture() {
    local data_dir="${TEST_DIR}/data"
    local local_root="${TEST_DIR}/local-backend"
    : "${HTTP_PORT:=$(pick_free_port)}"
    : "${ISCSI_PORT:=$(pick_free_port)}"

    mkdir -p "$data_dir" "$local_root"
    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "$data_dir"
library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "iqn.2025-10.com.metebalci:thurvtl"
storage:
  backends:
    local:
      type: local
      root_dir: "$local_root"
EOFCONFIG

    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"

    FIXTURE_DIR="${TEST_DIR}/fixture"
    FIXTURE_TAR="${TEST_DIR}/fixture.tar"
    mkdir -p "$FIXTURE_DIR"
    for i in $(seq 1 8); do
        dd if=/dev/urandom of="${FIXTURE_DIR}/blob-$i.bin" bs=1M count=1 status=none
    done
    tar -cf "$FIXTURE_TAR" -C "$FIXTURE_DIR" .
}

start_daemon() {
    TEST_CONFIG="${TEST_DIR}/config.yaml" DAEMON_LOG_MODE=append start_thur_daemon
}

kill_daemon_hard() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    DAEMON_PID=""
}

login_iscsi() {
    iscsi_discover_and_login
    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "No changer device"; lsscsi -g; return 1; }
    local tape_dev
    tape_dev=$(lsscsi | awk '/tape/{print $NF}' | head -1)
    [[ -n "$tape_dev" ]] || { log_error "No tape device"; lsscsi; return 1; }
    NOREWIND_DEVICE=$(echo "$tape_dev" | sed 's|/dev/st|/dev/nst|')
    log_info "  changer=$CHANGER_DEVICE tape=$NOREWIND_DEVICE"
    # Warm up — first op after login can hit POWER-ON UA.
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
    mt -f "$NOREWIND_DEVICE" status >/dev/null 2>&1 || true
}

logout_iscsi() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
        sleep 1
    fi
}

load_cart_in_drive() {
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || {
        log_error "mtx load 1 0 failed"
        mtx -f "$CHANGER_DEVICE" status >&2
        return 1
    }
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
}

unload_cart() {
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || true
}

dump_tape() {
    local out="$1"
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    # Read up to 32 MiB — the fixture is ~8 MiB. dd stops at EOF /
    # filemark naturally.
    dd if="$NOREWIND_DEVICE" of="$out" bs=64K count=512 status=none 2>&1 || true
    [[ -s "$out" ]]
}

test_kill_mid_stream_preserves_acked_blocks() {
    log_test "tape stream + filemark + kill -9 → restart → reads identical"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" cartridge create "$CART_LABEL" >/dev/null || return 1

    login_iscsi || return 1
    load_cart_in_drive || return 1

    log_info "  writing fixture to /dev/nstN via tar..."
    if ! tar -cf "$NOREWIND_DEVICE" -C "$FIXTURE_DIR" . 2>&1 | tail -3; then
        log_error "tar -cf failed"
        return 1
    fi
    mt -f "$NOREWIND_DEVICE" weof >/dev/null 2>&1 || true
    log_info "  wrote tar + filemark"

    local read_pre="${TEST_DIR}/read-pre.bin"
    dump_tape "$read_pre" || { log_error "pre-crash dump empty"; return 1; }
    log_info "  read-pre $(stat -c%s "$read_pre") bytes"

    unload_cart
    logout_iscsi
    kill_daemon_hard
    log_info "  daemon killed (SIGKILL)"

    start_daemon || return 1
    login_iscsi || return 1
    load_cart_in_drive || return 1

    local read_post="${TEST_DIR}/read-post.bin"
    dump_tape "$read_post" || { log_error "post-crash dump empty"; return 1; }
    log_info "  read-post $(stat -c%s "$read_post") bytes"

    if ! cmp -s "$read_pre" "$read_post"; then
        log_error "  ✗ read-pre != read-post — chunk-seal lost acked blocks"
        cmp "$read_pre" "$read_post" 2>&1 | head -3 | sed 's/^/    /' >&2
        return 1
    fi
    log_info "  ✓ tape blocks survived kill -9"

    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Crash-During-Chunk-Seal"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    prepare_fixture

    start_daemon || exit 1

    local passed=0 failed=0
    if test_kill_mid_stream_preserves_acked_blocks; then
        ((passed++))
    else
        ((failed++))
    fi
    echo ""

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total: $((passed + failed))"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""

    if [[ $failed -eq 0 ]]; then
        log_info "All crash-chunk-seal tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
