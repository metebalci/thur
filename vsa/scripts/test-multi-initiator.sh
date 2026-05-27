#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Multi-Initiator iSCSI Test
#
# Drives two distinct iSCSI initiators against the same target via
# open-iscsi `iface` records (each iface carries its own
# `iface.initiatorname`), then exercises the persistent-reservation
# (PR) matrix: A registers + reserves, B registers, B's WRITE must
# come back RESERVATION_CONFLICT (sense 0x18) per SPC-4 § 5.9.
#
# What's asserted:
#   1. Two distinct initiator IQNs can each login.
#   2. Initiator A's REGISTER + RESERVE succeeds.
#   3. Initiator B's REGISTER succeeds (no exclusivity yet).
#   4. Initiator B's WRITE comes back with reservation conflict sense.
#
# Prerequisites:
#   - open-iscsi, lsscsi, sg3-utils
#   - iscsid running
#   - Root / sudo NOPASSWD
#
# Why iface and not /etc/iscsi/initiatorname.iscsi swap: open-iscsi
# tracks per-iface `iface.initiatorname` independently of the global
# `initiatorname.iscsi`. Using two ifaces lets us run two SESSIONS
# with distinct I_T nexus IDs without restarting iscsid, which was
# the failure mode of the initial swap-based approach (the kernel
# would re-attach the prior session under the cached InitiatorName,
# so the daemon saw both "initiators" as one I_T nexus and PR
# exclusivity was a no-op).
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-multi-initiator.sh [--release] [--keep-data]
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
IFACE_A="thurvsa-test-host-a-$$"
IFACE_B="thurvsa-test-host-b-$$"
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

# Build a per-test iface record bound to a specific InitiatorName.
# open-iscsi stores these under /var/lib/iscsi/ifaces/<name> and
# evaluates `iface.initiatorname` at login, so two sessions opened
# via two ifaces present distinct I_T nexus IDs to the target.
create_iface() {
    local iface="$1" name="$2"
    iscsiadm -m iface -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$iface" --op update -n iface.initiatorname -v "$name" >/dev/null 2>&1
}

delete_iface() {
    iscsiadm -m iface -I "$1" -o delete >/dev/null 2>&1 || true
}

cleanup() {
    # Per-iface logout (login records are keyed on iface, so the
    # generic --logout would miss them).
    for iface in "$IFACE_A" "$IFACE_B"; do
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --op delete 2>/dev/null || true
        delete_iface "$iface"
    done
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
storage:
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

# Login through a specific iface and return the resulting /dev/sgN
# device for the thurvsa LUN. We disambiguate by snapshotting the
# pre-login sg device set, doing the login, and reading off the new
# entry — `lsscsi` ordering depends on session age and isn't safe
# to filter on alone.
login_via_iface() {
    local iface="$1" before_file="$2" after_file="$3"
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" -I "$iface" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --login >/dev/null
    sleep 3
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$after_file"
    comm -13 "$before_file" "$after_file" | head -1
}

logout_via_iface() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$1" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$1" --op delete >/dev/null 2>&1 || true
    sleep 1
}

test_pr_reservation_conflict_between_initiators() {
    log_test "two initiators + PR matrix: B blocked while A holds the reservation"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "vol-mi" --size 16M >/dev/null || return 1

    # Stage two ifaces with distinct iface.initiatorname; login each
    # independently so both sessions are live concurrently.
    create_iface "$IFACE_A" "$INIT_A_NAME"
    create_iface "$IFACE_B" "$INIT_B_NAME"

    local before="${TEST_DIR}/sg-before.txt"
    local after_a="${TEST_DIR}/sg-after-a.txt"
    local after_b="${TEST_DIR}/sg-after-b.txt"
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$before"

    local dev_a dev_b
    dev_a=$(login_via_iface "$IFACE_A" "$before" "$after_a")
    [[ -n "$dev_a" ]] || { log_error "A login: no new THUR VSA device"; lsscsi -g >&2; return 1; }
    log_info "  A=$INIT_A_NAME via $IFACE_A → $dev_a"

    dev_b=$(login_via_iface "$IFACE_B" "$after_a" "$after_b")
    [[ -n "$dev_b" ]] || { log_error "B login: no new THUR VSA device"; lsscsi -g >&2; return 1; }
    log_info "  B=$INIT_B_NAME via $IFACE_B → $dev_b"

    # A registers + takes Write Exclusive (type 1) reservation.
    if ! sg_persist --out --register --param-sark=0xA1A1 "$dev_a" >/dev/null 2>&1; then
        log_error "A register failed"
        return 1
    fi
    if ! sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$dev_a" >/dev/null 2>&1; then
        log_error "A reserve failed"
        return 1
    fi
    log_info "  ✓ A registered + reserved (Write Exclusive)"

    # B registers (allowed even when A holds the reservation).
    if ! sg_persist --out --register --param-sark=0xB2B2 "$dev_b" >/dev/null 2>&1; then
        log_error "B register failed"
        return 1
    fi
    log_info "  ✓ B registered (no exclusivity claim)"

    # Confirm both registrants show up in the daemon's PR table.
    # sg_persist's read-keys output lists each registered key on its
    # own indented line (`    0xa1a1`), not as `Key: <hex>`. We grep
    # the literal hex words rather than parse the layout.
    local pr_keys
    pr_keys=$(sg_persist --in --read-keys "$dev_a" 2>&1 | grep -oE '0x[0-9a-fA-F]+' | tr '\n' ' ')
    if [[ "$pr_keys" != *"0xa1a1"* || "$pr_keys" != *"0xb2b2"* ]]; then
        log_error "PR key table missing one of A/B (got: $pr_keys)"
        return 1
    fi
    log_info "  ✓ PR key table shows both A=0xa1a1 and B=0xb2b2"

    # B's WRITE must return RESERVATION CONFLICT (sense 0x18).
    # Probe via sg_write_same which takes an sg device directly — no
    # /dev/sdX detour through the kernel block layer (which has its
    # own caching and would mask a sense response).
    local write_log
    write_log=$(sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" 2>&1 || true)
    if echo "$write_log" | grep -qiE "Reservation conflict|reservation_conflict|sense.*0x18"; then
        log_info "  ✓ B's WRITE returned RESERVATION CONFLICT"
    else
        log_error "  ✗ B's WRITE did not conflict (daemon may not enforce PR)"
        echo "$write_log" | head -5 | sed 's/^/    /' >&2
        return 1
    fi

    # Cleanup the sessions so a re-run starts clean.
    logout_via_iface "$IFACE_A"
    logout_via_iface "$IFACE_B"
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
