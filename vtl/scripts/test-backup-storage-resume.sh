#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Boot-Time Orphan Upload Recovery Test
#
# Smoke-level verification of the wire-up added in
# `vtl/daemon/src/upload_recovery.rs`: when the daemon starts, it must
# spawn a background task that walks every cartridge's `chunks.idx`,
# looks for sealed-but-not-uploaded chunks left behind by a previous
# kill-mid-PUT, and re-queues them through the existing UploadRequest
# mpsc. The deep recovery logic is covered by unit tests in
# `upload_recovery::tests`; this script proves the scan actually runs
# at boot and writes its two audit entries.
#
# What's asserted:
#   1. The audit log carries `storage.orphan_scan_started`.
#   2. The audit log carries `storage.orphan_scan_completed` with
#      `orphans_found: 0` (fresh dir, nothing to recover).
#
# No iSCSI, no sudo, no real storage backend. LocalBackend rooted in the test
# tmp dir. Daemon runs in normal mode (not `--test`) for a few seconds
# so the audit writer + boot scan get a chance to fire, then killed.
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-backup-storage-resume.sh [OPTIONS]
#
# Options:
#   --debug         Use ./target/debug/ binaries (default is ./target/release/)
#   --keep-data     Don't clean up the test data dir on exit
#

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/test-backup-storage-resume-$$"

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        # Give the daemon a moment to flush the audit chain on graceful stop.
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$DAEMON_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
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
    local fixture="${TEST_DIR}/fixture"
    local data_dir="${fixture}/data"
    local local_root="${fixture}/local-backend"
    local config="${fixture}/config.yaml"
    local iscsi_port http_port
    iscsi_port=$(pick_free_port)
    http_port=$(pick_free_port)

    mkdir -p "$data_dir" "$local_root"

    cat > "$config" <<EOFCONFIG
data_dir: "$data_dir"
library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8
iscsi:
  listen: "127.0.0.1:$iscsi_port"
  target_iqn: "iqn.2025-10.com.metebalci:thurvtl"
http:
  listen: "127.0.0.1:$http_port"
license:
  file: "${fixture}/no-such.lic"
storage:
  backends:
    primary:
      type: local
      root_dir: "$local_root"

EOFCONFIG

    echo "$fixture"
}

# Polls the daemon's audit log for an extended regex pattern. Same
# semantics as wait_for_log_pattern but glob-matches the daily audit
# file under <data_dir>/audit/.
wait_for_audit_pattern() {
    local audit_dir="$1"
    local pattern="$2"
    local timeout="${3:-15}"
    local deadline=$(( $(date +%s) + timeout ))
    while (( $(date +%s) < deadline )); do
        if compgen -G "${audit_dir}/audit-*.jsonl" > /dev/null \
            && grep -Eq "$pattern" "${audit_dir}"/audit-*.jsonl 2>/dev/null; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

test_boot_scan_emits_audit_events() {
    log_test "boot orphan-upload scan emits start + completed audit entries"

    local fixture
    fixture=$(prepare_fixture)
    local log="${fixture}/daemon.log"
    local audit_dir="${fixture}/data/audit"

    RUST_LOG=info "$DAEMON_PATH" --config "${fixture}/config.yaml" \
        > "$log" 2>&1 &
    DAEMON_PID=$!

    # Wait for the completed entry — implies started was already written.
    if ! wait_for_audit_pattern "$audit_dir" '"op":"storage\.orphan_scan_completed"' 15; then
        log_error "  ✗ never saw 'storage.orphan_scan_completed' in audit log within 15s"
        log_error "    last 30 lines of daemon log:"
        tail -30 "$log" >&2
        if compgen -G "${audit_dir}/audit-*.jsonl" > /dev/null; then
            log_error "    last 5 audit entries:"
            tail -5 "${audit_dir}"/audit-*.jsonl >&2
        fi
        return 1
    fi
    log_info "  ✓ 'storage.orphan_scan_completed' present"

    if ! grep -Eq '"op":"storage\.orphan_scan_started"' "${audit_dir}"/audit-*.jsonl; then
        log_error "  ✗ 'storage.orphan_scan_started' missing — scan reported completion without start"
        return 1
    fi
    log_info "  ✓ 'storage.orphan_scan_started' present"

    # Fresh fixture: no orphans expected.
    if ! grep -Eq '"orphans_found":0' "${audit_dir}"/audit-*.jsonl; then
        log_error "  ✗ expected 'orphans_found:0' on the fresh fixture's completed entry"
        log_error "    matching entries:"
        grep '"op":"storage\.orphan_scan_completed"' "${audit_dir}"/audit-*.jsonl >&2
        return 1
    fi
    log_info "  ✓ orphans_found: 0 on the fresh fixture (nothing to recover)"

    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Boot Orphan-Upload Recovery"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"

    local passed=0
    local failed=0
    local tests=(
        "test_boot_scan_emits_audit_events"
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
        log_info "All boot-scan smoke tests passed"
        exit 0
    else
        log_error "$failed sub-test(s) failed"
        echo "Debug: re-run with --keep-data to inspect $TEST_DIR"
        exit 1
    fi
}

main
