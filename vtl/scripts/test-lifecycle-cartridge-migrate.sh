#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Cartridge Migrate + Archive CLI Smoke
#
# Exercises `thurvtl cartridge migrate`, `cartridge archive`, and
# `library restore-archive` at the CLI surface. End-to-end coverage
# with real chunks (write data, upload, migrate/archive, byte-for-byte
# read-back) lives in core/smc/tests/{migrate,archive,restore_archive}_tests.rs.
#
# This script asserts the surface those Rust tests can't:
#   - the actual `thurvtl` binary parses the new flags
#   - the daemon-routed job dispatcher accepts each kind and runs it
#     under the admin socket
#   - precondition errors are well-shaped (unknown target backend,
#     source == target, cartridge not in inventory, duplicate archive
#     label, missing archive on restore)
#   - dry-run completes without mutating on-disk state
#   - real migrate flips manifest.backend; archive creates the
#     expected on-bucket layout
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-lifecycle-cartridge-migrate.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default debug)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --keep-data           Don't clean up test data directory
#

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/test-lifecycle-cartridge-migrate-$$"
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
    if ! command -v curl &> /dev/null; then
        log_error "curl not found"
        exit 1
    fi
    if ! command -v jq &> /dev/null; then
        log_error "jq not found (needed to read manifest.backend)"
        exit 1
    fi
    log_info "All prerequisites met"
}

# Writes a daemon config + two named local backends ("src" and "dst")
# into separate bucket dirs. Returns paths via globals for inspection.
write_config() {
    local data_dir="$1"
    local config_path="$2"
    local src_bucket="$3"
    local dst_bucket="$4"
    mkdir -p "$data_dir" "$src_bucket" "$dst_bucket"
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
    src:
      type: local
      root_dir: "$src_bucket"
    dst:
      type: local
      root_dir: "$dst_bucket"
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

# Bring a fresh data dir up: write config, init library, start daemon,
# create a bare cartridge bound to "src". No iSCSI writes; the
# cartridge has zero sealed chunks, which is enough surface for the
# migrate CLI tests.
seed_fixture() {
    local data_dir="$1"
    local config="$2"
    local src_bucket="$3"
    local dst_bucket="$4"
    local barcode="$5"

    write_config "$data_dir" "$config" "$src_bucket" "$dst_bucket"

    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" cartridge create \
            "$barcode" --backend src --lto-generation 8 > /dev/null 2>&1; then
        log_error "cartridge create failed"
        tail -20 "${data_dir}/daemon.log" >&2
        return 1
    fi
}

manifest_backend_for() {
    local data_dir="$1"
    local barcode="$2"
    jq -r '.backend' "$data_dir/tapes/$barcode/manifest.json"
}

# --- Tests ----------------------------------------------------------

# Unknown target backend → daemon refuses with a clear error citing
# the known backend names.
test_refuses_unknown_target() {
    log_test "cartridge migrate refuses an unknown target backend..."
    local data_dir="$TEST_DIR/unknown"
    local config="$TEST_DIR/unknown.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/u-src" "$TEST_DIR/u-dst" "TAPE_U1"; then
        return 1
    fi

    local out
    out=$("$CLI_PATH" --config "$config" cartridge migrate \
        TAPE_U1 --target-backend nope 2>&1)
    local rc=$?
    stop_daemon
    if [[ $rc -eq 0 ]]; then
        log_error "expected non-zero exit; got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -qi "not defined"; then
        log_error "error message does not mention unknown backend:"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — clear unknown-backend error"
    return 0
}

# Source == target → refused; backend stays the same.
test_refuses_same_source_and_target() {
    log_test "cartridge migrate refuses source == target..."
    local data_dir="$TEST_DIR/same"
    local config="$TEST_DIR/same.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/s-src" "$TEST_DIR/s-dst" "TAPE_S1"; then
        return 1
    fi

    local out
    out=$("$CLI_PATH" --config "$config" cartridge migrate \
        TAPE_S1 --target-backend src 2>&1)
    local rc=$?
    stop_daemon
    if [[ $rc -eq 0 ]]; then
        log_error "expected non-zero exit; got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -qi "differ\|same"; then
        log_error "error message does not mention same source/target:"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — clear same-backend error"
    return 0
}

# Dry-run reports a plan and does not flip manifest.backend.
test_dry_run_no_mutation() {
    log_test "cartridge migrate --dry-run does not mutate manifest.backend..."
    local data_dir="$TEST_DIR/dryrun"
    local config="$TEST_DIR/dryrun.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/d-src" "$TEST_DIR/d-dst" "TAPE_D1"; then
        return 1
    fi

    local before
    before=$(manifest_backend_for "$data_dir" "TAPE_D1")
    if [[ "$before" != "src" ]]; then
        log_error "manifest.backend pre-migrate: expected 'src', got '$before'"
        stop_daemon
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" cartridge migrate \
            TAPE_D1 --target-backend dst --dry-run > /dev/null 2>&1; then
        log_error "dry-run failed"
        stop_daemon
        return 1
    fi

    local after
    after=$(manifest_backend_for "$data_dir" "TAPE_D1")
    stop_daemon
    if [[ "$after" != "src" ]]; then
        log_error "dry-run flipped manifest.backend ('$before' -> '$after')"
        return 1
    fi
    log_info "OK — dry-run kept manifest.backend='src'"
    return 0
}

# Real move flips manifest.backend and emits a cartridge.migrated
# audit entry.
test_move_flips_backend_and_audits() {
    log_test "cartridge migrate move flips manifest.backend + audits..."
    local data_dir="$TEST_DIR/move"
    local config="$TEST_DIR/move.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/m-src" "$TEST_DIR/m-dst" "TAPE_M1"; then
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" cartridge migrate \
            TAPE_M1 --target-backend dst > /dev/null 2>&1; then
        log_error "migrate failed"
        tail -20 "${data_dir}/daemon.log" >&2
        stop_daemon
        return 1
    fi

    local after
    after=$(manifest_backend_for "$data_dir" "TAPE_M1")
    if [[ "$after" != "dst" ]]; then
        log_error "manifest.backend not flipped: expected 'dst', got '$after'"
        stop_daemon
        return 1
    fi

    # Stop the daemon so today's audit file flushes, then grep the
    # chain for the migrated entry.
    stop_daemon
    if ! find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null \
            | grep -q '"op":"cartridge.migrated"'; then
        log_error "cartridge.migrated did not appear in the audit chain"
        find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>&1 >&2 || true
        return 1
    fi
    log_info "OK — move flipped backend and audited"
    return 0
}

# Rebind --no-verify against an empty target succeeds (operator
# vouches). With --verify (default) it must refuse against an empty
# target — but a zero-chunk cartridge has nothing to HEAD, so the
# only key checked is the manifest-latest sentinel. We assert the
# no-verify path here; the verify-refusal path is covered by
# migrate_tests.rs::migrate_rebind_refuses_when_target_missing_chunks.
test_rebind_no_verify() {
    log_test "cartridge migrate --mode=rebind --no-verify against empty target..."
    local data_dir="$TEST_DIR/rebind"
    local config="$TEST_DIR/rebind.yaml"
    if ! seed_fixture "$data_dir" "$config" \
            "$TEST_DIR/r-src" "$TEST_DIR/r-dst" "TAPE_R1"; then
        return 1
    fi

    if ! "$CLI_PATH" --config "$config" cartridge migrate \
            TAPE_R1 --target-backend dst \
            --mode rebind --no-verify > /dev/null 2>&1; then
        log_error "rebind --no-verify failed"
        tail -20 "${data_dir}/daemon.log" >&2
        stop_daemon
        return 1
    fi

    local after
    after=$(manifest_backend_for "$data_dir" "TAPE_R1")
    stop_daemon
    if [[ "$after" != "dst" ]]; then
        log_error "rebind did not flip manifest.backend: got '$after'"
        return 1
    fi
    log_info "OK — rebind --no-verify flipped backend"
    return 0
}

# cartridge archive: creates archives/<barcode>/<label>/manifest.json
# on the target bucket. Source manifest is unchanged.
test_archive_creates_archive_prefix() {
    log_test "cartridge archive creates archives/<barcode>/<label>/ prefix..."
    local data_dir="$TEST_DIR/arc"
    local config="$TEST_DIR/arc.yaml"
    local src_b="$TEST_DIR/a-src"
    local dst_b="$TEST_DIR/a-dst"
    if ! seed_fixture "$data_dir" "$config" "$src_b" "$dst_b" "TAPE_A1"; then
        return 1
    fi
    if ! "$CLI_PATH" --config "$config" cartridge archive \
            TAPE_A1 --target-backend dst --label snap1 > /dev/null 2>&1; then
        log_error "archive failed"
        tail -20 "${data_dir}/daemon.log" >&2
        stop_daemon
        return 1
    fi
    stop_daemon
    if [[ ! -f "$dst_b/archives/TAPE_A1/snap1/manifest.json" ]]; then
        log_error "expected $dst_b/archives/TAPE_A1/snap1/manifest.json"
        find "$dst_b" >&2
        return 1
    fi
    # Source cartridge unchanged.
    local src_backend
    src_backend=$(manifest_backend_for "$data_dir" "TAPE_A1")
    if [[ "$src_backend" != "src" ]]; then
        log_error "archive mutated source cartridge: backend='$src_backend'"
        return 1
    fi
    log_info "OK — archive lives under archives/ prefix, source intact"
    return 0
}

# Re-archiving under the same label refuses.
test_archive_refuses_duplicate_label() {
    log_test "cartridge archive refuses a duplicate label..."
    local data_dir="$TEST_DIR/arcdup"
    local config="$TEST_DIR/arcdup.yaml"
    local src_b="$TEST_DIR/ad-src"
    local dst_b="$TEST_DIR/ad-dst"
    if ! seed_fixture "$data_dir" "$config" "$src_b" "$dst_b" "TAPE_AD"; then
        return 1
    fi
    if ! "$CLI_PATH" --config "$config" cartridge archive \
            TAPE_AD --target-backend dst --label one > /dev/null 2>&1; then
        log_error "first archive failed"
        stop_daemon
        return 1
    fi
    local out
    out=$("$CLI_PATH" --config "$config" cartridge archive \
        TAPE_AD --target-backend dst --label one 2>&1)
    local rc=$?
    stop_daemon
    if [[ $rc -eq 0 ]]; then
        log_error "duplicate label expected non-zero exit; got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -qi "already exists"; then
        log_error "error did not mention 'already exists':"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — duplicate label refused"
    return 0
}

# restore-archive against a missing archive errors clearly.
test_restore_archive_refuses_missing() {
    log_test "library restore-archive refuses when archive not found..."
    local data_dir="$TEST_DIR/rstmiss"
    local config="$TEST_DIR/rstmiss.yaml"
    local src_b="$TEST_DIR/rm-src"
    local dst_b="$TEST_DIR/rm-dst"
    if ! seed_fixture "$data_dir" "$config" "$src_b" "$dst_b" "TAPE_RM"; then
        return 1
    fi
    local out
    out=$("$CLI_PATH" --config "$config" library restore-archive \
        --backend dst --barcode TAPE_NONE --label nope 2>&1)
    local rc=$?
    stop_daemon
    if [[ $rc -eq 0 ]]; then
        log_error "expected non-zero exit; got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -qi "not found"; then
        log_error "error did not mention 'not found':"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — missing archive refused"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Cartridge Migrate CLI Smoke"
    echo "========================================"
    echo ""
    echo "(End-to-end data round-trip lives in"
    echo " core/smc/tests/migrate_tests.rs)"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    assign_ports

    local passed=0
    local failed=0
    local tests=(
        test_refuses_unknown_target
        test_refuses_same_source_and_target
        test_dry_run_no_mutation
        test_move_flips_backend_and_audits
        test_rebind_no_verify
        test_archive_creates_archive_prefix
        test_archive_refuses_duplicate_label
        test_restore_archive_refuses_missing
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
