#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Cartridge Tiering CLI Smoke (plan / run-now / status)
#
# Exercises `thurvtl system tiering {plan,run-now,status}` at the CLI
# surface against two local backends ("hot" and "cold") and a single
# barcode-prefix tiering policy. Mirrors the local-only, zero-chunk
# style of test-lifecycle-cartridge-migrate.sh: the cartridges carry no
# sealed data, which is enough surface for the policy-engine + migrate-
# primitive wiring these tests assert. End-to-end tiering with real
# chunks + legal-hold exclusion against a storage backend lives in
# test-tiering-legal-hold-interaction.sh.
#
# This script asserts:
#   - the `thurvtl` binary parses `system tiering {plan,run-now,status}`
#     and the daemon-routed job dispatcher accepts each kind
#   - plan (read-only) lists exactly the cartridges a policy matches,
#     with the right from_backend -> to_backend, and excludes
#     non-matching cartridges; plan does not mutate manifest.backend
#   - status summarizes pending_moves before and after a run
#   - run-now flips manifest.backend for matched cartridges, leaves
#     non-matching cartridges untouched, and audits each as
#     `cartridge.tiered`
#   - a second plan after a successful run is a no-op (already on
#     target — first-match policy still claims it, but no move)
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-tiering-plan-and-run.sh [OPTIONS]
#
# Options:
#   --debug               Use ./target/debug/ binaries (default: release)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --keep-data           Don't clean up test data directory
#

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/test-tiering-plan-and-run-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    standard_cleanup
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    require_daemon_binaries thurvtl
    if ! command -v jq &> /dev/null; then
        log_error "jq not found (needed to read tiering JSON + manifest.backend)"
        exit 1
    fi
    log_info "All prerequisites met"
}

# Writes a daemon config with two named local backends ("hot" and
# "cold") in separate bucket dirs, plus a single tiering policy that
# migrates any cartridge whose barcode starts with "ARCH" to "cold".
write_config() {
    local data_dir="$1"
    local config_path="$2"
    local hot_bucket="$3"
    local cold_bucket="$4"
    mkdir -p "$data_dir" "$hot_bucket" "$cold_bucket"
    cat > "$config_path" <<EOFCONFIG
data_dir: "$data_dir"

library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8

license:
  file: "$TEST_DIR/no-such.lic"

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"

storage:
  backends:
    hot:
      type: local
      root_dir: "$hot_bucket"
    cold:
      type: local
      root_dir: "$cold_bucket"

tiering:
  policies:
    - predicates:
        barcode_prefix: "ARCH"
      migrate_to: cold
EOFCONFIG
}

start_daemon() {
    local config_path="$1"
    local data_dir="$2"
    export THURVTL_ADMIN_SOCKET="${data_dir}/admin.sock"
    TEST_CONFIG="$config_path" DAEMON_LOG="${data_dir}/daemon.log" RUST_LOG=warn start_thur_daemon
}

stop_daemon() {
    stop_thur_daemon
}

# Bring a fresh data dir up: write config, start daemon, create the
# given cartridges bound to "hot". No iSCSI writes — zero-chunk
# cartridges are enough surface for the tiering CLI tests.
seed_fixture() {
    local data_dir="$1"
    local config="$2"
    local hot_bucket="$3"
    local cold_bucket="$4"
    shift 4

    write_config "$data_dir" "$config" "$hot_bucket" "$cold_bucket"

    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi

    local bc
    for bc in "$@"; do
        if ! "$CLI_PATH" --config "$config" cartridge create \
                "$bc" --backend hot --lto-generation 8 > /dev/null 2>&1; then
            log_error "cartridge create $bc failed"
            tail -20 "${data_dir}/daemon.log" >&2
            return 1
        fi
    done
}

manifest_backend_for() {
    local data_dir="$1"
    local barcode="$2"
    jq -r '.backend' "$data_dir/tapes/$barcode/manifest.json"
}

# --- Tests ----------------------------------------------------------

# plan lists exactly the policy-matched cartridge (ARCH001 hot -> cold)
# and excludes the non-matching one (KEEP001). cartridges_scanned and
# policies counts are reported.
test_plan_lists_matching_only() {
    log_test "tiering plan lists matched cartridge, excludes non-matching..."
    local data_dir="$TEST_DIR/plan"
    local config="$TEST_DIR/plan.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/p-hot" "$TEST_DIR/p-cold" "ARCH001" "KEEP001"; then
        return 1
    fi

    local plan
    plan=$("$CLI_PATH" --config "$config" system tiering plan --json 2>/dev/null)
    local rc=$?
    stop_daemon
    if [[ $rc -ne 0 ]]; then
        log_error "tiering plan exited $rc. Output: $plan"
        return 1
    fi

    if ! echo "$plan" | jq -e \
        '.moves[] | select(.barcode=="ARCH001" and .from_backend=="hot" and .to_backend=="cold")' \
        > /dev/null; then
        log_error "plan did not propose ARCH001 hot -> cold:"
        echo "$plan" >&2
        return 1
    fi
    if echo "$plan" | jq -e '.moves[] | select(.barcode=="KEEP001")' > /dev/null; then
        log_error "plan wrongly proposed a move for non-matching KEEP001:"
        echo "$plan" >&2
        return 1
    fi
    if ! echo "$plan" | jq -e '.cartridges_scanned==2 and .policies==1' > /dev/null; then
        log_error "plan counts wrong (expected cartridges_scanned=2, policies=1):"
        echo "$plan" >&2
        return 1
    fi
    log_info "OK — plan matched ARCH001 only, KEEP001 left in place"
    return 0
}

# plan is read-only: it must not flip manifest.backend.
test_plan_is_read_only() {
    log_test "tiering plan does not mutate manifest.backend..."
    local data_dir="$TEST_DIR/ro"
    local config="$TEST_DIR/ro.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/ro-hot" "$TEST_DIR/ro-cold" "ARCH001"; then
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" system tiering plan --json > /dev/null 2>&1; then
        log_error "tiering plan failed"
        stop_daemon
        return 1
    fi
    local after
    after=$(manifest_backend_for "$data_dir" "ARCH001")
    stop_daemon
    if [[ "$after" != "hot" ]]; then
        log_error "plan flipped manifest.backend: expected 'hot', got '$after'"
        return 1
    fi
    log_info "OK — plan kept manifest.backend='hot'"
    return 0
}

# status reports pending_moves and under_legal_hold counts.
test_status_reports_pending() {
    log_test "tiering status reports pending_moves count..."
    local data_dir="$TEST_DIR/status"
    local config="$TEST_DIR/status.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/st-hot" "$TEST_DIR/st-cold" "ARCH001" "KEEP001"; then
        return 1
    fi

    local status
    status=$("$CLI_PATH" --config "$config" system tiering status --json 2>/dev/null)
    local rc=$?
    stop_daemon
    if [[ $rc -ne 0 ]]; then
        log_error "tiering status exited $rc. Output: $status"
        return 1
    fi
    if ! echo "$status" | jq -e '.pending_moves==1 and .under_legal_hold==0' > /dev/null; then
        log_error "status counts wrong (expected pending_moves=1, under_legal_hold=0):"
        echo "$status" >&2
        return 1
    fi
    log_info "OK — status reports 1 pending move"
    return 0
}

# run-now migrates the matched cartridge (manifest.backend flips to
# cold), leaves the non-matching one on hot, and audits cartridge.tiered.
test_run_now_migrates_and_audits() {
    log_test "tiering run-now flips matched backend + audits, leaves others..."
    local data_dir="$TEST_DIR/run"
    local config="$TEST_DIR/run.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/r-hot" "$TEST_DIR/r-cold" "ARCH001" "KEEP001"; then
        return 1
    fi

    local run
    run=$("$CLI_PATH" --config "$config" system tiering run-now --json 2>/dev/null)
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        log_error "tiering run-now exited $rc. Output: $run"
        tail -20 "${data_dir}/daemon.log" >&2
        stop_daemon
        return 1
    fi
    if ! echo "$run" | jq -e \
        '.migrated[] | select(.barcode=="ARCH001" and .from_backend=="hot" and .to_backend=="cold")' \
        > /dev/null; then
        log_error "run-now did not migrate ARCH001 hot -> cold:"
        echo "$run" >&2
        stop_daemon
        return 1
    fi

    local arch_backend keep_backend
    arch_backend=$(manifest_backend_for "$data_dir" "ARCH001")
    keep_backend=$(manifest_backend_for "$data_dir" "KEEP001")
    if [[ "$arch_backend" != "cold" ]]; then
        log_error "ARCH001 manifest.backend not flipped: expected 'cold', got '$arch_backend'"
        stop_daemon
        return 1
    fi
    if [[ "$keep_backend" != "hot" ]]; then
        log_error "KEEP001 wrongly moved: expected 'hot', got '$keep_backend'"
        stop_daemon
        return 1
    fi

    # Stop the daemon so today's audit file flushes, then grep the chain.
    stop_daemon
    if ! find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null \
            | grep -q '"op":"cartridge.tiered"'; then
        log_error "cartridge.tiered did not appear in the audit chain"
        find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>&1 >&2 || true
        return 1
    fi
    log_info "OK — run-now moved ARCH001 to cold, kept KEEP001 on hot, audited"
    return 0
}

# A second plan after a successful run is a no-op: ARCH001 is already on
# its target backend, so the policy claims it but proposes no move.
test_plan_after_run_is_noop() {
    log_test "tiering plan after run-now proposes no further moves..."
    local data_dir="$TEST_DIR/noop"
    local config="$TEST_DIR/noop.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/n-hot" "$TEST_DIR/n-cold" "ARCH001"; then
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" system tiering run-now --json > /dev/null 2>&1; then
        log_error "first run-now failed"
        tail -20 "${data_dir}/daemon.log" >&2
        stop_daemon
        return 1
    fi

    local status
    status=$("$CLI_PATH" --config "$config" system tiering status --json 2>/dev/null)
    stop_daemon
    if ! echo "$status" | jq -e '.pending_moves==0' > /dev/null; then
        log_error "expected pending_moves=0 after run-now, got:"
        echo "$status" >&2
        return 1
    fi
    log_info "OK — no pending moves after the cartridge reached its target"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Cartridge Tiering CLI Smoke"
    echo "========================================"
    echo ""
    echo "(End-to-end tiering with real chunks + legal-hold exclusion"
    echo " lives in test-tiering-legal-hold-interaction.sh)"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    assign_ports

    local passed=0
    local failed=0
    local tests=(
        test_plan_lists_matching_only
        test_plan_is_read_only
        test_status_reports_pending
        test_run_now_migrates_and_audits
        test_plan_after_run_is_noop
    )

    for t in "${tests[@]}"; do
        if "$t"; then ((passed++)); else ((failed++)); fi
        echo ""
    done

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""
    if [[ $failed -eq 0 ]]; then
        log_info "All tests passed"
        exit 0
    else
        log_error "$failed test(s) failed"
        echo "Test data: $TEST_DIR"
        exit 1
    fi
}

main
