#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA iSCSI Multi-PDU Data-In Test
#
# Regression guard for the iSCSI Data-In chunking path. A single
# Data-In PDU larger than the initiator-declared MaxRecvDataSegmentLength
# violates RFC 7143 §11.7; the Linux iSCSI initiator drops the link on
# the spec violation, retries, drops again — sg_read sees mass short
# transfers and the kernel logs EPIPE storms. This script issues a
# single SCSI READ-16 large enough to force the daemon onto the
# multi-PDU path (each PDU carrying sequential DataSN / monotonic
# BufferOffset / F-bit | S-bit only on the final PDU); a control
# sub-test repeats at bpt=1 so a green control + red multi-sector
# unambiguously fingers the chunking code.
#
# Filesystem-level harnesses (monte-carlo, fs-iscsi) don't exercise
# this — the kernel block layer splits filesystem I/O into ≤max_sectors_kb
# SCSI commands well below MRDSL. Only a raw sg_dd READ-16 with a
# large bpt triggers the multi-PDU response shape.
#
# Workflow:
#   1. Create a fresh volume.
#   2. iSCSI login, identify /dev/sdX and /dev/sgN.
#   3. Generate 8 MiB of random bytes into a host-side payload file.
#   4. Write the payload to /dev/sdX via `dd oflag=direct conv=fsync`
#      (bypasses the kernel page cache; conv=fsync issues
#      SYNCHRONIZE CACHE on close).
#   5. Read the same range back via `sg_dd` with `bs=4096` (matches
#      the device's declared LBA size) and a large `bpt`
#      (blocks-per-transfer). Each SG_IO is one READ-16 with
#      TRANSFER LENGTH = bpt sectors, producing a Data-In response
#      far above the Linux iSCSI initiator's default
#      MaxRecvDataSegmentLength (262144). Pre-fix the daemon emits
#      one PDU of `bpt * 4096` bytes, the initiator drops the
#      connection on the spec violation, and sg_dd captures zeros
#      or short-transfers.
#   6. cmp the two — they MUST match byte-for-byte.
#
# Why `sg_dd bs=4096` is the right reproducer: sg_dd places `bpt`
# directly into the READ-16 TRANSFER LENGTH field; with `bs` set to
# the device's actual sector size, sg_dd's internal buffer
# accounting also lines up (bs * bpt bytes per SG_IO). Using
# `bs=64K bpt=128` confuses sg_dd — it'd ask for 128 sectors
# (512 KiB at 4 KiB sectors) while expecting 8 MiB. The fix is
# `bs=4096 bpt=1024 count=2048` for an 8 MiB transfer split into 2
# READ-16(1024)s of 4 MiB each.
# Why not `dd iflag=direct`: this system ships uutils dd (Rust
# reimplementation of coreutils dd) which rejects `iflag=direct`
# outright. sg_dd avoids the dd-flavor dependency.
#
# A control sub-test repeats the round-trip at bpt=1 (single-sector
# READs) so a green control + red multi-sector unambiguously fingers
# the chunking path.
#
# Prerequisites:
#   - open-iscsi, sg3-utils, lsscsi (sudo apt-get install ...)
#   - iscsid running (sudo systemctl enable --now iscsid)
#   - Root / sudo NOPASSWD
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-iscsi-multi-pdu-readin.sh [--release] [--keep-data]
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-test-iscsi-multi-pdu-readin-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
ISCSI_CONNECTED=0
VOLUME_NAME="vol-multi"
VOLUME_SIZE_MIB=64
PAYLOAD_MIB=8
RW_DEVICE=""
RW_SG_DEVICE=""

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
    fi
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    for t in iscsiadm lsscsi cmp curl dd sg_dd systemctl; do
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
    : "${HTTP_PORT:=$(pick_free_port)}"
    : "${ISCSI_PORT:=$(pick_free_port)}"

    mkdir -p "$data_dir" "$local_root"

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
}

start_daemon() {
    TEST_CONFIG="${TEST_DIR}/config.yaml" DAEMON_LOG_MODE=append start_thur_daemon
}

login_iscsi() {
    iscsi_discover_and_login
    local row
    row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
    [[ -n "$row" ]] || { log_error "No THUR VSA device found"; lsscsi -g; return 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    RW_SG_DEVICE=$(echo "$row" | awk '{print $NF}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; return 1; }
    log_info "  iSCSI LUN -> $RW_DEVICE (sg passthrough: $RW_SG_DEVICE)"
}

# Round-trip $PAYLOAD_MIB MiB of random data via raw dd write, read
# back via sg_read with the requested blocks-per-transfer, and
# compare. Returns 0 on byte-for-byte match, 1 otherwise.
roundtrip_at_bpt() {
    local bpt="$1"             # sectors per SG_IO READ command
    local label="$2"           # human-readable label for log lines
    local payload="${TEST_DIR}/payload-bpt${bpt}.bin"
    local readback="${TEST_DIR}/readback-bpt${bpt}.bin"
    local payload_bytes=$(( PAYLOAD_MIB * 1024 * 1024 ))
    # sg_read processes (count * bpt * bs) bytes total. With bs=4096
    # (device LBA size) and total = 8 MiB, count = 2048 / bpt.
    local total_sectors=$(( payload_bytes / 4096 ))
    if (( total_sectors % bpt != 0 )); then
        log_error "  [${label}] total_sectors $total_sectors not a multiple of bpt $bpt"
        return 1
    fi
    local cmd_count=$(( total_sectors / bpt ))

    dd if=/dev/urandom of="$payload" bs=1M count="$PAYLOAD_MIB" status=none

    # Raw WRITE: oflag=direct + conv=fsync forces SYNCHRONIZE CACHE
    # on close. After dd returns the daemon's chunk pool MUST contain
    # every byte (test-crash-page-flush.sh proves this invariant —
    # the WRITE path is not the bug).
    if ! dd if="$payload" of="$RW_DEVICE" bs=1M count="$PAYLOAD_MIB" \
            oflag=direct conv=fsync status=none; then
        log_error "  [${label}] dd write to $RW_DEVICE failed"
        return 1
    fi

    # Multi-sector READ via SG_IO. Each command's TRANSFER LENGTH =
    # bpt sectors → bpt * 4096 bytes of Data-In. With bpt = 1024
    # that's a 4 MiB Data-In response — well above the Linux iSCSI
    # initiator's default MaxRecvDataSegmentLength (262144), so the
    # daemon MUST chunk per RFC 7143 §11.7 for the read to land.
    local sg_log="${TEST_DIR}/sg_dd-bpt${bpt}.log"
    if ! sg_dd "if=$RW_SG_DEVICE" "of=$readback" \
                bs=4096 "bpt=$bpt" "count=$total_sectors" \
                > "$sg_log" 2>&1; then
        log_error "  [${label}] sg_dd failed"
        sed 's/^/      /' "$sg_log" >&2
        return 1
    fi

    local readback_bytes
    readback_bytes=$(stat -c%s "$readback")
    if (( readback_bytes != payload_bytes )); then
        log_error "  [${label}] sg_dd produced $readback_bytes bytes, expected $payload_bytes"
        sed 's/^/      /' "$sg_log" >&2
        return 1
    fi

    if grep -q "Non-zero sum of residual counts" "$sg_log"; then
        log_error "  [${label}] sg_dd reported residual underflow — Data-In chunking bug"
        sed 's/^/      /' "$sg_log" >&2
        return 1
    fi

    if ! cmp -s "$payload" "$readback"; then
        log_error "  [${label}] readback != payload — multi-sector short-transfer bug"
        echo "    first diff:" >&2
        cmp "$payload" "$readback" 2>&1 | head -1 | sed 's/^/      /' >&2
        return 1
    fi

    log_info "  [${label}] ${PAYLOAD_MIB} MiB round-trip via sg_dd bs=4096 bpt=${bpt} matches"
    return 0
}

test_multi_sector_roundtrip() {
    log_test "raw dd write + sg_dd bs=4096 bpt=1024 (4 MiB READ-16) round-trip"
    roundtrip_at_bpt 1024 "bpt=1024/4MiB"
}

test_single_sector_roundtrip() {
    log_test "raw dd write + sg_dd bs=4096 bpt=1 (1-sector READ-16) control round-trip"
    roundtrip_at_bpt 1 "bpt=1/1-sector"
}

main() {
    echo "========================================"
    echo "Thur VSA SBC Multi-Sector R/W"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    prepare_fixture

    start_daemon || exit 1

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "$VOLUME_NAME" --size "${VOLUME_SIZE_MIB}M" >/dev/null || exit 1
    login_iscsi || exit 1

    local passed=0 failed=0
    # Multi-sector is the bug; run it first so its output is at the
    # top of the log. The control case follows so a green control +
    # red multi-sector is the unambiguous signature.
    if test_multi_sector_roundtrip; then ((passed++)); else ((failed++)); fi
    echo ""
    if test_single_sector_roundtrip; then ((passed++)); else ((failed++)); fi
    echo ""

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total: $((passed + failed))"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""

    if [[ $failed -eq 0 ]]; then
        log_info "All SBC multi-sector R/W tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
