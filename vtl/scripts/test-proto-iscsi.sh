#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL iSCSI Conformance Test
#
# Verifies the iSCSI protocol layer (login, CmdSN/StatSN bookkeeping, header
# digests) using libiscsi's userspace client. Userspace libiscsi is stricter
# than the Linux kernel iSCSI initiator about RFC 3720/7143 sequencing — bugs
# the kernel quietly works around (e.g. an off-by-one in the Login Response's
# ExpCmdSN) cause libiscsi to silently drop subsequent SCSI PDUs. So this
# script is the early-warning regression net for protocol-layer changes.
#
# This is NOT a SCSI-command conformance suite. iscsi-test-cu's tests are
# almost all SBC (block device); they don't apply to tape (SSC) or changer
# (SMC), so we don't run them. For SCSI command coverage see
# test-scsi-conformance.sh (sg3_utils-based, sudo).
#
# Prerequisites:
#   - libiscsi-bin    (Debian/Ubuntu: sudo apt-get install libiscsi-bin)
#                     Provides the iscsi-inq and iscsi-ls binaries.
#   - thurvtld and thurvtl (built or on PATH)
#
# No sudo / no kernel iSCSI initiator required.
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-proto-iscsi.sh [OPTIONS]
#
# Options:
#   --debug               Use ./target/debug/ binaries (default: ./target/release/)
#   --daemon-path PATH    Override path to thurvtld binary
#   --cli-path PATH       Override path to thurvtl binary
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

# Note: We don't use 'set -e' because we want to run all tests even if some fail.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

# Configuration
TEST_DIR="/tmp/test-proto-iscsi-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"

init_common_daemon_args
parse_common_daemon_args "$@"

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    standard_cleanup
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=()
    local hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvtld >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvtld)
        else
            missing+=("thurvtld")
            hints+=("  - thurvtld: $build_cmd (or pass --daemon-path PATH)")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvtl >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvtl)
        else
            missing+=("thurvtl")
            hints+=("  - thurvtl: $build_cmd (or pass --cli-path PATH)")
        fi
    fi

    if ! command -v iscsi-inq >/dev/null 2>&1; then
        missing+=("iscsi-inq")
        hints+=("  - iscsi-inq: sudo apt-get install libiscsi-bin")
    fi
    if ! command -v iscsi-ls >/dev/null 2>&1; then
        missing+=("iscsi-ls")
        hints+=("  - iscsi-ls: sudo apt-get install libiscsi-bin")
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
    mkdir -p "$TEST_DIR"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

$(yaml_vtl_library 40 2 8)

# Force Community mode so a host-installed license at /etc/thurvtl/license.lic
# can't influence test behavior. Pointing license.file at a nonexistent path
# disables the default search paths and triggers the Missing-license fallback
# to Community.
license:
  file: "$TEST_DIR/no-such.lic"

http:
  listen: "127.0.0.1:$HTTP_PORT"

$(yaml_iscsi "$TARGET_IQN")
$(yaml_local_backend)

EOFCONFIG
    mkdir -p "$TEST_DIR/data"
}

start_daemon() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    start_thur_daemon
}

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

PASSED=0
FAILED=0

# iscsi-inq performs login + INQUIRY; success exercises the full login state
# machine including CmdSN/StatSN bookkeeping. A regression in the daemon's
# Login Response (e.g. wrong ExpCmdSN, wrong Status-Class, missing transit bit)
# will surface here as a hang or non-zero exit.
test_inquiry() {
    local lun="$1"
    local expected_pdt="$2"
    local logfile="$3"
    if ! timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/$lun" \
            > "$logfile" 2>&1; then
        log_error "iscsi-inq LUN $lun failed (see $logfile)"
        return 1
    fi
    if ! grep -q "Peripheral Device Type:$expected_pdt" "$logfile"; then
        log_error "iscsi-inq LUN $lun: did not report $expected_pdt (see $logfile)"
        return 1
    fi
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
    echo "Thur VTL iSCSI Conformance Test"
    echo "========================================"
    echo "Verifying iSCSI protocol layer (login + CmdSN/StatSN bookkeeping)"
    echo "via libiscsi's strict userspace client."
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon

    echo ""
    run_test "iscsi-inq INQUIRY (changer LUN 0 -> MEDIA_CHANGER)" \
        test_inquiry 0 "MEDIA_CHANGER" "${TEST_DIR}/inquiry-changer.log"
    run_test "iscsi-inq INQUIRY (drive LUN 1 -> SEQUENTIAL_ACCESS)" \
        test_inquiry 1 "SEQUENTIAL_ACCESS" "${TEST_DIR}/inquiry-drive-1.log"

    # --- Multi-portal phase ---------------------------------------------
    # `iscsi.listen` accepts a list; the daemon binds one TCP listener
    # per entry and SendTargets advertises every entry. Restart with a
    # second loopback portal and verify both bind + SendTargets returns
    # both portals.
    log_info "Switching to multi-portal config (two loopback listeners)..."
    stop_thur_daemon
    ISCSI_PORT2=$(pick_free_port)
    log_info "Second iSCSI portal: 127.0.0.1:$ISCSI_PORT2"
    cat > "$TEST_CONFIG" <<EOFCONFIG2
data_dir: "$TEST_DIR/data"

$(yaml_vtl_library 40 2 8)

license:
  file: "$TEST_DIR/no-such.lic"

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen:
    - "127.0.0.1:$ISCSI_PORT"
    - "127.0.0.1:$ISCSI_PORT2"
  target_iqn: "$TARGET_IQN"

$(yaml_local_backend)

EOFCONFIG2
    DAEMON_LOG_MODE=append start_thur_daemon

    run_test "iscsi-inq INQUIRY through portal 1 (LUN 0)" \
        test_inquiry 0 "MEDIA_CHANGER" "${TEST_DIR}/inquiry-mp-portal1.log"

    # Second portal: same test, against the second port.
    iscsi_port_save=$ISCSI_PORT
    ISCSI_PORT=$ISCSI_PORT2
    run_test "iscsi-inq INQUIRY through portal 2 (LUN 0)" \
        test_inquiry 0 "MEDIA_CHANGER" "${TEST_DIR}/inquiry-mp-portal2.log"
    ISCSI_PORT=$iscsi_port_save

    # SendTargets wire check via iscsi-ls: discovery Login +
    # SendTargets + Logout against either portal must enumerate both
    # advertised portals and exit cleanly. Regression for issue #41
    # (Logout / NOP-Out against discovery sessions used to hang).
    log_test "iscsi-ls discovery enumerates both portals (issue #41)"
    if iscsi_ls_out=$(timeout 10 iscsi-ls "iscsi://127.0.0.1:$ISCSI_PORT" 2>&1); then
        if grep -q "127.0.0.1:$ISCSI_PORT," <<<"$iscsi_ls_out" \
            && grep -q "127.0.0.1:$ISCSI_PORT2," <<<"$iscsi_ls_out"; then
            log_pass "iscsi-ls listed both portals"
            PASSED=$((PASSED + 1))
        else
            log_fail "iscsi-ls output missing portal(s): $iscsi_ls_out"
            FAILED=$((FAILED + 1))
        fi
    else
        log_fail "iscsi-ls hung or exited non-zero: $iscsi_ls_out"
        FAILED=$((FAILED + 1))
    fi
    echo ""

    # /sessions HTTP endpoint must report the list of listen addresses.
    log_test "/sessions JSON carries listen_addresses array"
    if curl -sf "http://127.0.0.1:$HTTP_PORT/sessions" > "${TEST_DIR}/sessions.json" 2>&1; then
        addrs_len=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("listen_addresses", [])))' "${TEST_DIR}/sessions.json")
        if [[ "$addrs_len" -eq 2 ]]; then
            log_pass "/sessions: listen_addresses has 2 entries"
            PASSED=$((PASSED + 1))
        else
            log_fail "/sessions: listen_addresses len = $addrs_len, expected 2"
            FAILED=$((FAILED + 1))
        fi
    else
        log_fail "/sessions fetch failed"
        FAILED=$((FAILED + 1))
    fi
    echo ""

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
