#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Crash-During-Page-Flush Durability Test
#
# Drives real host writes through ext4, fences with sync + umount,
# then SIGKILLs the daemon (no SIGTERM, no graceful flush) —
# emulating power loss / OOM kill / panic. On restart the daemon must
# serve back the exact same bytes that survived the umount fence.
#
# The data-path invariant proved: when the host has umounted a
# filesystem (so every WRITE + SYNCHRONIZE CACHE has been acked by
# the daemon), the page-cache layer in `core-block` has committed
# every page to the on-disk index + chunk pool. A kill -9 between
# umount and any later flush_all() must not lose data.
#
# Workflow:
#   1. Create a fresh volume.
#   2. iSCSI login, identify /dev/sdX and /dev/sgN.
#   3. mkfs.ext4 /dev/sdX, mount, tar-extract a ~4 MiB fixture tree,
#      sync, umount (so the kernel flushes every dirty page through
#      WRITE + SYNCHRONIZE CACHE to the daemon).
#   4. sg_dd snapshot the volume into snap-pre.bin (bypasses kernel
#      page cache via SG_IO — the load-bearing gate).
#   5. iSCSI logout, SIGKILL daemon, restart, iSCSI login.
#   6. sg_dd snapshot into snap-post.bin.
#   7. `cmp snap-pre snap-post` — every fsync'd byte must survive.
#
# Why ext4 + tar and not raw `dd` to /dev/sdX: ext4 mirrors the
# production workload and is the same pattern
# `test-fs-iscsi.sh` uses to assert the daemon's reads
# round-trip through ordinary host I/O. The raw-IO multi-sector
# round-trip path is covered separately by
# `test-iscsi-multi-pdu-readin.sh` (sg_dd bs=4096 bpt=1024 → 4 MiB
# READ-16 commands, exercising the iSCSI transport's Data-In
# chunking against the initiator's MaxRecvDataSegmentLength).
#
# Prerequisites:
#   - open-iscsi, sg3-utils, lsscsi, e2fsprogs (mkfs.ext4 / fsck.ext4),
#     util-linux (mount, umount), tar (sudo apt-get install ...)
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

TEST_DIR="/tmp/thurvsa-test-crash-page-flush-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
ISCSI_CONNECTED=0
VOLUME_NAME="vol-crash"
VOLUME_SIZE_MIB=64
FIXTURE_MIB=4
MOUNT_POINT=""
FIXTURE_DIR=""
FIXTURE_TAR=""
RW_DEVICE=""
RW_SG_DEVICE=""

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    if [[ -n "$MOUNT_POINT" ]] && mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi
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
    for t in iscsiadm sg_dd lsscsi cmp curl tar mkfs.ext4 mount umount systemctl; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service not running"
        exit 1
    fi
    require_daemon_binaries thurvsa
}

prepare_fixture() {
    local data_dir="${TEST_DIR}/data"
    local local_root="${TEST_DIR}/local-backend"
    MOUNT_POINT="${TEST_DIR}/mnt"
    FIXTURE_DIR="${TEST_DIR}/fixture"
    FIXTURE_TAR="${TEST_DIR}/fixture.tar"
    : "${HTTP_PORT:=$(pick_free_port)}"
    : "${ISCSI_PORT:=$(pick_free_port)}"

    mkdir -p "$data_dir" "$local_root" "$MOUNT_POINT" "$FIXTURE_DIR"

    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "$data_dir"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
storage:
  backends:
    local:
      type: local
      root_dir: "$local_root"
EOFCONFIG

    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"

    # Generate a ~4 MiB tree of mixed text + random bytes (mirrors
    # `test-fs-iscsi.sh`'s fixture so the data exercised by
    # the workload looks like real backup content).
    for i in $(seq 1 8); do
        dd if=/dev/urandom of="${FIXTURE_DIR}/blob-$i.bin" bs=512K count=1 status=none
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
        iscsi_logout_and_delete
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
    log_test "ext4 tar + sync + umount + kill -9 → restart → reads identical"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "$VOLUME_NAME" --size "${VOLUME_SIZE_MIB}M" >/dev/null || return 1

    login_iscsi || return 1

    # mkfs + mount + tar-extract, then sync + umount so every dirty
    # page is flushed to the daemon via WRITE + SYNCHRONIZE CACHE.
    # umount is the load-bearing fence: by the time it returns, the
    # daemon has acked every page.
    if ! mkfs.ext4 -q -F "$RW_DEVICE" >/dev/null 2>&1; then
        log_error "mkfs.ext4 $RW_DEVICE failed"
        return 1
    fi
    if ! mount "$RW_DEVICE" "$MOUNT_POINT"; then
        log_error "mount $RW_DEVICE failed"
        return 1
    fi
    if ! tar -xf "$FIXTURE_TAR" -C "$MOUNT_POINT"; then
        log_error "tar -xf failed"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    sync
    umount "$MOUNT_POINT" || { log_error "umount failed"; return 1; }
    log_info "  wrote ${FIXTURE_MIB} MiB via ext4 + tar; sync + umount fenced"

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

    # Extra correctness gate: re-mount the filesystem post-restart
    # and verify fsck passes + every fixture file is intact. This
    # catches the bug class where snap-pre/snap-post both contain
    # the same wrong data (e.g. daemon serves stale cache on read).
    if ! fsck.ext4 -fn "$RW_DEVICE" >/dev/null 2>&1; then
        log_error "  ✗ fsck.ext4 failed on the post-restart filesystem"
        return 1
    fi
    mount "$RW_DEVICE" "$MOUNT_POINT" || return 1
    local missing=0 mismatch=0 total=0
    for f in "$FIXTURE_DIR"/*.bin; do
        total=$((total + 1))
        local base
        base=$(basename "$f")
        if [[ ! -f "$MOUNT_POINT/$base" ]]; then
            missing=$((missing + 1))
        elif ! cmp -s "$f" "$MOUNT_POINT/$base"; then
            mismatch=$((mismatch + 1))
        fi
    done
    umount "$MOUNT_POINT" || true
    if (( missing > 0 || mismatch > 0 )); then
        log_error "  ✗ post-restart filesystem: ${missing}/${total} missing, ${mismatch}/${total} mismatch"
        return 1
    fi
    log_info "  ✓ post-restart filesystem: all $total fixture files intact"

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
