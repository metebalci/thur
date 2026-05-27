#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Audit-Chain Crash-Recovery Test
#
# Mirrors `vsa/scripts/test-crash-audit-append.sh` for the tape
# product. Exercises one durability invariant: the BLAKE3-chained
# `audit-NNNN.jsonl` re-validates after the daemon is killed with
# SIGKILL mid-operation. If `AuditChannel`'s single-writer task ever
# loses partial-line tail handling or fsync sequencing, this catches
# it.
#
# What's asserted:
#   1. After bring-up + N audit-emitting admin ops + SIGKILL + restart,
#      `system audit verify` exits 0.
#   2. The post-restart chain still carries the pre-crash entries.
#   3. Repeat crash + restart is idempotent (chain stays valid).
#
# No sudo, no iSCSI initiator. LocalBackend rooted in the test tmp
# dir; audit-generating ops are admin-socket cartridge create/destroy.
#
# Usage:
#   ./vtl/scripts/test-crash-audit-append.sh [--release] [--keep-data]
#

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvtl-test-crash-audit-$$"
TEST_CONFIG=""

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
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
    require_daemon_binaries thurvtl
}

prepare_fixture() {
    local data_dir="${TEST_DIR}/data"
    local local_root="${TEST_DIR}/local-backend"
    TEST_CONFIG="${TEST_DIR}/config.yaml"
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)

    mkdir -p "$data_dir" "$local_root"

    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$data_dir"
library:
  num_slots: 16
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
}

start_daemon() {
    DAEMON_LOG="${TEST_DIR}/daemon-$1.log" start_thur_daemon
}

kill_daemon_hard() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    DAEMON_PID=""
}

count_audit_lines() {
    local audit_dir="${TEST_DIR}/data/audit"
    if compgen -G "${audit_dir}/audit-*.jsonl" > /dev/null; then
        cat "${audit_dir}"/audit-*.jsonl 2>/dev/null | wc -l
    else
        echo 0
    fi
}

emit_audit_traffic() {
    for i in 1 2 3 4 5; do
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "tape-$1-$i" >/dev/null 2>&1 || true
    done
}

verify_chain_clean() {
    local out rc=0
    out=$("$CLI_PATH" --config "$TEST_CONFIG" system audit verify 2>&1) || rc=$?
    if (( rc != 0 )); then
        log_error "system audit verify exited $rc:"
        echo "$out" | sed 's/^/    /' >&2
        return 1
    fi
    return 0
}

test_kill_after_burst_keeps_chain_valid() {
    log_test "kill -9 after audit burst → restart → audit verify clean"

    start_daemon "boot1" || return 1
    emit_audit_traffic "burst1"
    local pre_lines
    pre_lines=$(count_audit_lines)
    log_info "  emitted audit burst, $pre_lines line(s) on disk pre-crash"

    kill_daemon_hard
    start_daemon "boot2" || return 1

    if ! verify_chain_clean; then
        log_error "  ✗ chain failed to validate after first crash"
        return 1
    fi
    log_info "  ✓ chain valid after first kill-restart cycle"

    local post_lines
    post_lines=$(count_audit_lines)
    if (( post_lines < pre_lines )); then
        log_error "  ✗ silent truncation: $pre_lines → $post_lines lines"
        return 1
    fi
    log_info "  ✓ pre-crash entries preserved ($pre_lines → $post_lines)"

    return 0
}

test_repeated_crash_idempotent() {
    log_test "second kill -9 → restart → audit verify still clean"

    emit_audit_traffic "burst2"
    kill_daemon_hard
    start_daemon "boot3" || return 1

    if ! verify_chain_clean; then
        log_error "  ✗ chain failed after second crash cycle"
        return 1
    fi
    log_info "  ✓ chain valid after repeated kill-restart"

    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Audit-Chain Crash Recovery"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    prepare_fixture

    local passed=0 failed=0
    local tests=(
        "test_kill_after_burst_keeps_chain_valid"
        "test_repeated_crash_idempotent"
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
        log_info "All audit-crash tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
