#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL + Thur VSA Co-Residency Smoke Test
#
# Both daemons are designed to share a single host: distinct system
# users, conffile dirs, data dirs, admin sockets, and unit names — but
# the default iSCSI / HTTP ports overlap (3260 / 9090). The
# documented install pattern is "operator overrides one in YAML";
# this test exercises exactly that flow + proves the two products
# don't trip each other's:
#
#   1. iSCSI listener (each daemon binds its own ephemeral port).
#   2. HTTP admin server (each daemon binds its own ephemeral port,
#      `/health` returns the right `daemon` field).
#   3. Admin Unix socket (per-product path under the test tmp dir).
#   4. Audit log dir (per-product `<data_dir>/audit/`).
#   5. CLI dispatch (thurvtl talks only to thurvtld; thurvsa to
#      thurvsad — no cross-talk).
#
# No sudo, no iSCSI initiator. Both daemons run with --config under
# /tmp; admin sockets are per-test paths exported via the
# `THURVTL_ADMIN_SOCKET` / `THURVSA_ADMIN_SOCKET` env vars that both
# the daemon and CLI honor.
#
# Usage (invoke from repo root):
#   ./scripts/test-coresident-smoke.sh [--release] [--keep-data]
#

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/test-helpers.sh"

BUILD_PROFILE="debug"
TEST_DIR="/tmp/thur-coresident-$$"
KEEP_DATA=0
VTL_DAEMON=""
VTL_CLI=""
VSA_DAEMON=""
VSA_CLI=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)   BUILD_PROFILE="release"; shift ;;
        --keep-data) KEEP_DATA=1; shift ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

: "${VTL_DAEMON:=./target/$BUILD_PROFILE/thurvtld}"
: "${VTL_CLI:=./target/$BUILD_PROFILE/thurvtl}"
: "${VSA_DAEMON:=./target/$BUILD_PROFILE/thurvsad}"
: "${VSA_CLI:=./target/$BUILD_PROFILE/thurvsa}"

VTL_PID=""
VSA_PID=""

cleanup() {
    for pid in "$VTL_PID" "$VSA_PID"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    for path in "$VTL_DAEMON" "$VTL_CLI" "$VSA_DAEMON" "$VSA_CLI"; do
        if [[ ! -x "$path" ]]; then
            log_error "Missing binary: $path (build with cargo build)"
            exit 1
        fi
    done
}

VTL_ISCSI_PORT=""
VTL_HTTP_PORT=""
VSA_ISCSI_PORT=""
VSA_HTTP_PORT=""

prepare_fixtures() {
    # Exports must happen in the *current* shell, not a subshell. We
    # set the port globals directly rather than echoing them through
    # $(prepare_fixtures), which would lose the env exports.
    mkdir -p "$TEST_DIR"/{vtl-data,vsa-data,vtl-local,vsa-local}

    VTL_ISCSI_PORT=$(pick_free_port)
    VTL_HTTP_PORT=$(pick_free_port)
    VSA_ISCSI_PORT=$(pick_free_port)
    VSA_HTTP_PORT=$(pick_free_port)

    cat > "${TEST_DIR}/thurvtl.yaml" <<EOFVTL
data_dir: "${TEST_DIR}/vtl-data"
library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8
http:
  listen: "127.0.0.1:$VTL_HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$VTL_ISCSI_PORT"
  target_iqn: "iqn.2025-10.com.metebalci:thurvtl"
cloud:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/vtl-local"
EOFVTL

    cat > "${TEST_DIR}/thurvsa.yaml" <<EOFVSA
data_dir: "${TEST_DIR}/vsa-data"
http:
  listen: "127.0.0.1:$VSA_HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$VSA_ISCSI_PORT"
cloud:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/vsa-local"
EOFVSA

    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/thurvtl-admin.sock"
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/thurvsa-admin.sock"
}

start_both() {
    RUST_LOG=info "$VTL_DAEMON" --config "${TEST_DIR}/thurvtl.yaml" > "${TEST_DIR}/vtl.log" 2>&1 &
    VTL_PID=$!
    RUST_LOG=info "$VSA_DAEMON" --config "${TEST_DIR}/thurvsa.yaml" > "${TEST_DIR}/vsa.log" 2>&1 &
    VSA_PID=$!
}

wait_for_health() {
    local label="$1" port="$2" expected_daemon="$3"
    for _ in {1..30}; do
        local body
        body=$(curl -sf "http://127.0.0.1:$port/health" 2>/dev/null || true)
        if [[ -n "$body" ]] && echo "$body" | grep -q "\"daemon\":\"$expected_daemon\""; then
            log_info "  ✓ $label /health responding ($expected_daemon)"
            return 0
        fi
        sleep 0.5
    done
    log_error "  ✗ $label /health did not surface daemon=$expected_daemon on port $port"
    tail -20 "${TEST_DIR}/${label}.log" >&2
    return 1
}

test_both_health_endpoints_distinguish_daemon() {
    log_test "both daemons live on disjoint HTTP ports, identifying themselves"

    prepare_fixtures
    start_both

    wait_for_health "vtl" "$VTL_HTTP_PORT" "thurvtl" || return 1
    wait_for_health "vsa" "$VSA_HTTP_PORT" "thurvsa" || return 1
    return 0
}

test_admin_sockets_route_to_correct_daemon() {
    log_test "each CLI routes only to its own daemon's admin socket"

    # Create a cartridge via thurvtl: should land in VTL's data dir,
    # never in VSA's.
    if ! "$VTL_CLI" --config "${TEST_DIR}/thurvtl.yaml" cartridge create "co-tape" >/dev/null 2>&1; then
        log_error "  ✗ thurvtl cartridge create failed"
        return 1
    fi
    log_info "  ✓ VTL cartridge create succeeded against thurvtl-admin.sock"

    # Create a volume via thurvsa: should land in VSA's data dir.
    if ! "$VSA_CLI" --config "${TEST_DIR}/thurvsa.yaml" volume create "co-vol" --size 16M >/dev/null 2>&1; then
        log_error "  ✗ thurvsa volume create failed"
        return 1
    fi
    log_info "  ✓ VSA volume create succeeded against thurvsa-admin.sock"

    # Cross-talk check: VTL data dir must NOT contain any volumes/
    # subdirectory, and VSA data dir must NOT contain any tapes/
    # subdirectory.
    if [[ -d "${TEST_DIR}/vtl-data/volumes" ]] \
        && [[ -n "$(ls -A "${TEST_DIR}/vtl-data/volumes" 2>/dev/null)" ]]; then
        log_error "  ✗ VTL data dir leaked a volumes/ subdir — cross-talk?"
        return 1
    fi
    if [[ -d "${TEST_DIR}/vsa-data/tapes" ]] \
        && [[ -n "$(ls -A "${TEST_DIR}/vsa-data/tapes" 2>/dev/null)" ]]; then
        log_error "  ✗ VSA data dir leaked a tapes/ subdir — cross-talk?"
        return 1
    fi
    log_info "  ✓ no cross-talk between admin sockets / data dirs"

    return 0
}

test_audit_logs_are_disjoint() {
    log_test "each daemon writes audit rows to its own data dir only"

    local vtl_audit="${TEST_DIR}/vtl-data/audit"
    local vsa_audit="${TEST_DIR}/vsa-data/audit"
    if ! compgen -G "${vtl_audit}/audit-*.jsonl" >/dev/null; then
        log_error "  ✗ no VTL audit files"
        return 1
    fi
    if ! compgen -G "${vsa_audit}/audit-*.jsonl" >/dev/null; then
        log_error "  ✗ no VSA audit files"
        return 1
    fi

    # The cartridge.create row must appear under VTL only.
    if ! grep -lq '"op":"cartridge.create"' "${vtl_audit}"/audit-*.jsonl; then
        log_error "  ✗ VTL audit missing cartridge.create row"
        return 1
    fi
    if grep -lq '"op":"cartridge.create"' "${vsa_audit}"/audit-*.jsonl 2>/dev/null; then
        log_error "  ✗ VSA audit leaked a cartridge.create row"
        return 1
    fi
    if ! grep -lq '"op":"volume.create"' "${vsa_audit}"/audit-*.jsonl; then
        log_error "  ✗ VSA audit missing volume.create row"
        return 1
    fi
    if grep -lq '"op":"volume.create"' "${vtl_audit}"/audit-*.jsonl 2>/dev/null; then
        log_error "  ✗ VTL audit leaked a volume.create row"
        return 1
    fi

    log_info "  ✓ audit rows are partitioned by product"
    return 0
}

test_audit_verify_on_both_chains() {
    log_test "audit verify succeeds for both chains independently"

    if ! "$VTL_CLI" --config "${TEST_DIR}/thurvtl.yaml" system audit verify >/dev/null 2>&1; then
        log_error "  ✗ VTL audit verify failed"
        return 1
    fi
    if ! "$VSA_CLI" --config "${TEST_DIR}/thurvsa.yaml" system audit verify >/dev/null 2>&1; then
        log_error "  ✗ VSA audit verify failed"
        return 1
    fi
    log_info "  ✓ both chains valid"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL + VSA Co-Residency Smoke"
    echo "========================================"
    echo ""

    check_prerequisites

    local passed=0 failed=0
    local tests=(
        "test_both_health_endpoints_distinguish_daemon"
        "test_admin_sockets_route_to_correct_daemon"
        "test_audit_logs_are_disjoint"
        "test_audit_verify_on_both_chains"
    )
    for t in "${tests[@]}"; do
        if $t; then
            ((passed++))
        else
            ((failed++))
        fi
        echo ""
    done

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total: $((passed + failed))"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""

    if [[ $failed -eq 0 ]]; then
        log_info "All co-residency smoke tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
