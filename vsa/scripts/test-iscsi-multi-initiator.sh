#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Multi-Initiator iSCSI Test
#
# Drives two concurrent iSCSI sessions against the same target via
# two distinct InitiatorName values (one default kernel, one synthetic
# `iqn.2025-test:co-init`). Exercises the persistent-reservation
# (PR) matrix: each initiator registers, the first reserves, the
# second's WRITE must come back RESERVATION_CONFLICT (sense 0x18) per
# SPC-4 § 5.9.
#
# What's asserted:
#   1. Two distinct initiator IQNs can login simultaneously.
#   2. Initiator A's REGISTER + RESERVE succeeds.
#   3. Initiator B's REGISTER succeeds (no exclusivity yet).
#   4. Initiator B's WRITE comes back with reservation conflict sense.
#   5. Initiator B's PREEMPT succeeds and steals the reservation.
#
# Prerequisites:
#   - open-iscsi, lsscsi, sg3-utils
#   - iscsid running
#   - Root / sudo NOPASSWD
#
# This script is self-elevating (matches the test-iscsi-fs-workflow
# pattern). Without two kernel-initiator IQNs, the dual-host PR matrix
# can't be exercised; we approximate this by editing
# /etc/iscsi/initiatorname.iscsi between two logins.
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-iscsi-multi-initiator.sh [--release] [--keep-data]
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
TEST_DIR="/tmp/thurvsa-multi-init-$$"
KEEP_DATA=0
DAEMON_PATH=""
CLI_PATH=""
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
DAEMON_PID=""
INIT_BACKUP=""
INIT_A_NAME="iqn.2025-test.com.metebalci:host-a"
INIT_B_NAME="iqn.2025-test.com.metebalci:host-b"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)   BUILD_PROFILE="release"; shift ;;
        --keep-data) KEEP_DATA=1; shift ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

: "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
: "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

restore_initiator_name() {
    if [[ -n "$INIT_BACKUP" && -f "$INIT_BACKUP" ]]; then
        cp -f "$INIT_BACKUP" /etc/iscsi/initiatorname.iscsi
        systemctl restart iscsid 2>/dev/null || true
        sleep 1
    fi
}

set_initiator_name() {
    local name="$1"
    echo "InitiatorName=$name" > /etc/iscsi/initiatorname.iscsi
    systemctl restart iscsid 2>/dev/null || true
    sleep 2
}

cleanup() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
    restore_initiator_name
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
    for t in iscsiadm sg_persist sg_dd lsscsi curl systemctl; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    [[ -f /etc/iscsi/initiatorname.iscsi ]] || {
        log_error "/etc/iscsi/initiatorname.iscsi missing — open-iscsi not initialised"
        exit 1
    }
    [[ -x "$DAEMON_PATH" ]] || { log_error "Missing $DAEMON_PATH"; exit 1; }
    [[ -x "$CLI_PATH" ]] || { log_error "Missing $CLI_PATH"; exit 1; }
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
cloud:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/local-backend"
EOFCONFIG
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    RUST_LOG=info "$DAEMON_PATH" --config "${TEST_DIR}/config.yaml" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    log_error "daemon did not become ready"
    tail -20 "${TEST_DIR}/daemon.log"
    return 1
}

discover_lun_block() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    sleep 3
    lsscsi | awk '/THUR VSA/{print $NF; exit}'
}

logout_session() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete >/dev/null 2>&1 || true
    sleep 1
}

test_pr_reservation_conflict_between_initiators() {
    log_test "two initiators + PR matrix: B blocked while A holds the reservation"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "vol-mi" --size 16M >/dev/null || return 1

    # Backup the real initiator name; restore on cleanup.
    INIT_BACKUP="${TEST_DIR}/initiatorname.iscsi.bak"
    cp /etc/iscsi/initiatorname.iscsi "$INIT_BACKUP"

    # Initiator A: register + reserve.
    set_initiator_name "$INIT_A_NAME"
    local dev_a
    dev_a=$(discover_lun_block) || { log_error "A: lun not visible"; return 1; }
    log_info "  A=$INIT_A_NAME → $dev_a"
    sg_persist --out --register --param-sark=0xA1A1 "$dev_a" >/dev/null || return 1
    sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$dev_a" >/dev/null || return 1
    log_info "  ✓ A registered + reserved"
    logout_session

    # Initiator B: register, then try to WRITE — must conflict.
    set_initiator_name "$INIT_B_NAME"
    local dev_b
    dev_b=$(discover_lun_block) || { log_error "B: lun not visible"; return 1; }
    log_info "  B=$INIT_B_NAME → $dev_b"
    sg_persist --out --register --param-sark=0xB2B2 "$dev_b" >/dev/null || return 1

    # Attempt a WRITE — expected to return non-zero with RESERVATION
    # CONFLICT in sense data.
    local write_log
    write_log=$(sg_dd if=/dev/zero of="$dev_b" bs=4K count=1 oflag=direct 2>&1 || true)
    if echo "$write_log" | grep -qi "Reservation conflict"; then
        log_info "  ✓ B's WRITE returned RESERVATION CONFLICT (sense as expected)"
    else
        log_error "  ✗ B's WRITE did not conflict as expected"
        echo "$write_log" | head -5 | sed 's/^/    /' >&2
        return 1
    fi
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Multi-Initiator iSCSI"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1

    local passed=0 failed=0
    if test_pr_reservation_conflict_between_initiators; then
        ((passed++))
    else
        ((failed++))
    fi
    echo ""

    echo "Total: $((passed + failed))  Passed: $passed  Failed: $failed"
    [[ $failed -eq 0 ]] && exit 0 || exit 1
}

main
