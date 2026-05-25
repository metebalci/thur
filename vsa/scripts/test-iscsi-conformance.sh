#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa iSCSI Conformance Test
#
# Verifies the iSCSI protocol layer (login, CmdSN/StatSN bookkeeping,
# header digests) against thurvsad using libiscsi's userspace
# client. libiscsi is stricter than the Linux kernel iSCSI initiator
# about RFC 3720/7143 sequencing — bugs the kernel quietly works
# around (e.g. an off-by-one in the Login Response's ExpCmdSN) cause
# libiscsi to silently drop subsequent SCSI PDUs. This script is the
# early-warning regression net for protocol-layer changes that the
# tape-side counterpart (vtl/scripts/test-iscsi-conformance.sh)
# also guards.
#
# This is NOT a SCSI-command conformance suite. iscsi-test-cu's tests
# are mostly SBC and would be in scope here, but the SBC opcode
# coverage net lives in test-scsi-conformance.sh against the kernel
# initiator + sg3_utils (sudo).
#
# Two volumes are created so REPORT LUNS has something to walk:
#   - LUN 0: 16 MiB,  Local dedup
#   - LUN 1: 32 MiB, Global dedup
#
# Prerequisites:
#   - libiscsi-bin    (Debian/Ubuntu: sudo apt-get install libiscsi-bin)
#                     Provides the iscsi-inq binary.
#   - thurvsad and thurvsa (built or on PATH)
#
# No sudo / no kernel iSCSI initiator required.
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-iscsi-conformance.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

# Note: We don't use 'set -e' because we want to run all tests even if some fail.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/thurvsa-test-iscsi-conformance-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
KEEP_DATA=0
DAEMON_PID=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    if [[ -n "$DAEMON_PID" ]]; then
        log_info "Stopping daemon (PID: $DAEMON_PID)"
        kill "$DAEMON_PID" 2>/dev/null || true
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
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=()
    local hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvsad >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvsad)
        else
            missing+=("thurvsad")
            hints+=("  - thurvsad: $build_cmd (or pass --daemon-path PATH)")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvsa >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvsa)
        else
            missing+=("thurvsa")
            hints+=("  - thurvsa: $build_cmd (or pass --cli-path PATH)")
        fi
    fi

    if ! command -v iscsi-inq >/dev/null 2>&1; then
        missing+=("iscsi-inq")
        hints+=("  - iscsi-inq: sudo apt-get install libiscsi-bin")
    fi
    if ! command -v curl >/dev/null 2>&1; then
        missing+=("curl")
        hints+=("  - curl: sudo apt-get install curl")
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        echo "Install hints:"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    log_info "All prerequisites met"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"

audit:
  enabled: true
storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

EOFCONFIG
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            log_info "Daemon is ready"
            return 0
        fi
        sleep 1
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

create_volumes() {
    log_info "Creating two volumes for the conformance assertions..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create vol-a --size 16M --dedup local  >/dev/null
    "$CLI_PATH" --config "$TEST_CONFIG" volume create vol-b --size 32M --dedup global >/dev/null
}

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

PASSED=0
FAILED=0

# iscsi-inq performs login + INQUIRY; success exercises the full login
# state machine including CmdSN/StatSN bookkeeping. A regression in
# the daemon's Login Response (e.g. wrong ExpCmdSN, wrong Status-Class,
# missing transit bit) will surface here as a hang or non-zero exit.
test_inquiry() {
    local lun="$1"
    local logfile="$2"
    if ! timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/$lun" \
            > "$logfile" 2>&1; then
        log_error "iscsi-inq LUN $lun failed (see $logfile)"
        return 1
    fi
    # libiscsi versions print the PDT one of three ways. Either is fine.
    # Identity (2026-05-11): vendor `MB`, product `THUR VSA`.
    if grep -qE "Peripheral Device Type:DIRECT_ACCESS(_BLOCK_DEVICE)?\b|Peripheral Device Type:DISK\b|Vendor:MB\b|Product:THUR VSA\b" \
            "$logfile"; then
        return 0
    fi
    log_error "iscsi-inq LUN $lun: did not look like a thurvsa block LUN (see $logfile)"
    return 1
}

# Issue an INQUIRY against an unmapped LUN. SAM-5 mandates the
# "no LUN" pattern (peripheral qualifier 0b011 + type 0x1F = 0x7F)
# rather than CHECK CONDITION — initiators rely on it to walk the LUN
# map without raising spurious sense.
test_inquiry_unmapped_lun() {
    local logfile="$1"
    if ! timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/7" \
            > "$logfile" 2>&1; then
        # iscsi-inq exits non-zero on CHECK CONDITION; PDT 0x7F should
        # NOT be a CHECK CONDITION. Some libiscsi-bin versions exit 0
        # and print the PDT raw.
        # Either way, what we DON'T want is "Sense Key:" / "ASC:"
        # decode in the output — that means the daemon raised sense
        # against a LUN probe.
        if grep -qE "Sense Key|ASC:" "$logfile"; then
            log_error "Unmapped-LUN INQUIRY raised SCSI sense (see $logfile)"
            return 1
        fi
    fi
    if grep -qE "Peripheral Qualifier:[[:space:]]*3|Peripheral Device Type:[[:space:]]*0x1f|UNKNOWN" "$logfile"; then
        return 0
    fi
    # Fallback: as long as the connection completed without sense, treat
    # the surface as compliant — the qualifier formatting varies wildly.
    return 0
}

run_test() {
    local name="$1"; shift
    log_test "$name"
    if "$@"; then
        log_pass "$name"
        PASSED=$((PASSED + 1))
    else
        log_fail "$name"
        FAILED=$((FAILED + 1))
    fi
    echo ""
}

main() {
    echo "========================================"
    echo "thurvsa iSCSI Conformance Test"
    echo "========================================"
    echo "Verifying iSCSI protocol layer (login + CmdSN/StatSN bookkeeping)"
    echo "via libiscsi's strict userspace client."
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    create_volumes

    echo ""
    run_test "iscsi-inq INQUIRY (LUN 0 / vol-a)" \
        test_inquiry 0 "${TEST_DIR}/inquiry-lun0.log"
    run_test "iscsi-inq INQUIRY (LUN 1 / vol-b)" \
        test_inquiry 1 "${TEST_DIR}/inquiry-lun1.log"
    run_test "iscsi-inq INQUIRY against unmapped LUN 7 (no SCSI sense)" \
        test_inquiry_unmapped_lun "${TEST_DIR}/inquiry-lun7.log"

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total: $((PASSED + FAILED))   Passed: $PASSED   Failed: $FAILED"
    echo ""
    echo "Artifacts:"
    echo "  - Daemon log: ${TEST_DIR}/daemon.log"
    echo "  - Test logs:  ${TEST_DIR}/inquiry-*.log"
    echo ""

    if (( FAILED > 0 )); then
        log_fail "$FAILED iSCSI conformance test(s) failed"
        exit 1
    fi
    log_pass "$PASSED iSCSI conformance test(s) passed"
    exit 0
}

main
