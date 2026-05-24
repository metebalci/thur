#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Crash-During-Page-Flush Durability Test
#
# Drives real host writes through ext4/sg_dd, fences with SYNC, then
# SIGKILLs the daemon (no SIGTERM, no graceful flush) — emulating
# power loss / OOM kill / panic. On restart the daemon must serve back
# the exact same bytes we wrote pre-crash.
#
# The data-path invariant proved: when a host write returns OK
# (post-fsync / post-SYNCHRONIZE CACHE), the page-cache layer in
# `core-block` has committed the page to the on-disk index + chunk
# pool. A kill -9 between commit and any later flush_all() must not
# lose data.
#
# Workflow:
#   1. Create a fresh volume.
#   2. iSCSI login, identify /dev/sdX and /dev/sgN.
#   3. Write 8 MiB of random bytes via `dd oflag=direct conv=fsync`
#      (forces SYNCHRONIZE CACHE on close).
#   4. sg_dd snapshot the volume into snap-pre.bin (bypasses kernel
#      page cache via SG_IO).
#   5. iSCSI logout, SIGKILL daemon, restart, iSCSI login.
#   6. sg_dd snapshot into snap-post.bin.
#   7. `cmp snap-pre snap-post` — the load-bearing durability gate.
#
# Prerequisites:
#   - open-iscsi, sg3-utils, lsscsi (sudo apt-get install ...)
#   - iscsid running (sudo systemctl enable --now iscsid)
#   - Root / sudo NOPASSWD
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-crash-page-flush.sh [--release] [--keep-data]
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
TEST_DIR="/tmp/thurvsa-test-crash-page-flush-$$"
KEEP_DATA=0
DAEMON_PATH=""
CLI_PATH=""
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
DAEMON_PID=""
ISCSI_CONNECTED=0
VOLUME_NAME="vol-crash"
VOLUME_SIZE_MIB=64
PAYLOAD_MIB=8
RW_DEVICE=""
RW_SG_DEVICE=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)   BUILD_PROFILE="release"; shift ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

: "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
: "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

cleanup() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
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
    for t in iscsiadm sg_dd lsscsi cmp curl dd systemctl; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service not running"
        exit 1
    fi
    [[ -x "$DAEMON_PATH" ]] || { log_error "Daemon missing at $DAEMON_PATH"; exit 1; }
    [[ -x "$CLI_PATH" ]] || { log_error "CLI missing at $CLI_PATH"; exit 1; }
}

prepare_fixture() {
    local data_dir="${TEST_DIR}/data"
    local local_root="${TEST_DIR}/local-backend"
    : "${HTTP_PORT:=$(pick_free_port)}"
    : "${ISCSI_PORT:=$(pick_free_port)}"

    mkdir -p "$data_dir" "$local_root"

    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "$data_dir"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
cloud:
  backends:
    local:
      type: local
      root_dir: "$local_root"
EOFCONFIG

    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
}

start_daemon() {
    RUST_LOG=info "$DAEMON_PATH" --config "${TEST_DIR}/config.yaml" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    return 1
}

kill_daemon_hard() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    DAEMON_PID=""
}

login_iscsi() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    sleep 3
    local row
    row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
    [[ -n "$row" ]] || { log_error "No THUR VSA device found"; lsscsi -g; return 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    RW_SG_DEVICE=$(echo "$row" | awk '{print $NF}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE not a block device"; return 1; }
    log_info "  iSCSI LUN -> $RW_DEVICE (sg: $RW_SG_DEVICE)"
}

logout_iscsi() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete >/dev/null 2>&1 || true
        ISCSI_CONNECTED=0
        sleep 1
    fi
}

snapshot_volume() {
    local out_file="$1"
    local block_bytes=131072
    local total_blocks=$(( VOLUME_SIZE_MIB * 1024 * 1024 / block_bytes ))
    sg_dd "if=$RW_SG_DEVICE" "of=$out_file" "bs=$block_bytes" "count=$total_blocks" 2>&1 | tail -2 | sed 's/^/    /'
    local actual
    actual=$(stat -c%s "$out_file")
    (( actual == VOLUME_SIZE_MIB * 1024 * 1024 )) || {
        log_error "snapshot size $actual != $((VOLUME_SIZE_MIB * 1024 * 1024))"
        return 1
    }
}

test_kill_after_sync_preserves_data() {
    log_test "host write + fsync + kill -9 → restart → reads identical"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "$VOLUME_NAME" --size "${VOLUME_SIZE_MIB}M" >/dev/null || return 1

    login_iscsi || return 1

    # Write a known pattern at the head of the volume, fenced by fsync.
    # oflag=direct bypasses the kernel page cache; conv=fsync issues
    # SYNCHRONIZE CACHE on close. After dd returns the daemon MUST
    # have committed every page.
    local payload="${TEST_DIR}/payload.bin"
    dd if=/dev/urandom of="$payload" bs=1M count="$PAYLOAD_MIB" status=none
    if ! dd if="$payload" of="$RW_DEVICE" bs=1M count="$PAYLOAD_MIB" \
            oflag=direct conv=fsync status=none; then
        log_error "dd to $RW_DEVICE failed"
        return 1
    fi
    log_info "  wrote ${PAYLOAD_MIB} MiB with oflag=direct conv=fsync"

    # Pre-crash snapshot through SG_IO (bypasses kernel block-cache).
    local snap_pre="${TEST_DIR}/snap-pre.bin"
    snapshot_volume "$snap_pre" || return 1
    log_info "  snap-pre $(stat -c%s "$snap_pre") bytes captured"

    # Cleanly remove the kernel session so post-restart login picks up
    # a fresh device; the daemon itself is *not* given a graceful stop.
    logout_iscsi
    kill_daemon_hard
    log_info "  daemon killed (SIGKILL — no graceful flush)"

    # Restart and re-attach.
    start_daemon || return 1
    login_iscsi || return 1

    local snap_post="${TEST_DIR}/snap-post.bin"
    snapshot_volume "$snap_post" || return 1
    log_info "  snap-post $(stat -c%s "$snap_post") bytes captured"

    if ! cmp -s "$snap_pre" "$snap_post"; then
        log_error "  ✗ snap-pre != snap-post — data lost across kill -9"
        cmp "$snap_pre" "$snap_post" 2>&1 | head -3 | sed 's/^/    /' >&2
        return 1
    fi
    log_info "  ✓ snap-pre == snap-post: every fsync'd byte survived kill -9"

    # Extra correctness gate: the head of the volume must match the
    # original payload exactly.
    if ! cmp -s -n $((PAYLOAD_MIB * 1024 * 1024)) "$payload" "$snap_post"; then
        log_error "  ✗ payload bytes diverged from what was written"
        return 1
    fi
    log_info "  ✓ payload bytes match original at head of volume"

    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Crash-During-Page-Flush"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    prepare_fixture

    start_daemon || exit 1

    local passed=0 failed=0
    if test_kill_after_sync_preserves_data; then
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
        log_info "All crash-page-flush tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
