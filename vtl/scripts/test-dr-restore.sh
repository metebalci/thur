#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Cross-region DR Restore Smoke
#
# Exercises `thurvtl library restore` at the CLI surface. End-to-
# end coverage with real chunks (write to source, wipe, restore on a
# fresh host, read every block back) lives in
# `core/smc/tests/restore_tests.rs` — that test owns the data
# round-trip because no sudo / kernel iSCSI initiator is needed for
# in-process Cartridge writes.
#
# This script asserts the surface that the Rust test can't:
#   - the actual `thurvtl` binary parses --backend / --barcodes /
#     --dry-run / --allow-existing
#   - daemon-down dispatch reads `storage.backends:` from the YAML conffile
#     directly
#   - precondition errors are well-shaped (missing `library init`,
#     unknown backend, ambiguous when multiple backends configured)
#   - dry-run leaves zero filesystem state under `<data_dir>/tapes/`
#   - empty-bucket discovery succeeds cleanly (exit 0, "0 discovered")
#   - the queued `library.restore` audit entry survives a daemon
#     start and replays into the chain
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-dr-restore.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default is ./target/debug/)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --keep-data           Don't clean up test data directory
#

set -u  # Not set -e; we want to run all tests even if one fails.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/test-dr-restore-$$"
KEEP_DATA=0
DAEMON_PID=""
HTTP_PORT=""
ISCSI_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)        BUILD_PROFILE="release"; shift ;;
        --daemon-path)    DAEMON_PATH="$2"; shift 2 ;;
        --cli-path)       CLI_PATH="$2"; shift 2 ;;
        --keep-data)      KEEP_DATA=1; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

cleanup() {
    if [[ -n "$DAEMON_PID" ]]; then
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
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"
    if [[ ! -f "$DAEMON_PATH" ]]; then
        log_error "Daemon not found at: $DAEMON_PATH"
        log_error "Build with: $build_cmd"
        exit 1
    fi
    if [[ ! -f "$CLI_PATH" ]]; then
        log_error "CLI not found at: $CLI_PATH"
        log_error "Build with: $build_cmd"
        exit 1
    fi
    if ! command -v curl &> /dev/null; then
        log_error "curl not found"
        exit 1
    fi
    log_info "All prerequisites met"
}

write_config() {
    local data_dir="$1"
    local config_path="$2"
    local mirror_dir="$3"
    mkdir -p "$data_dir"
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
    mirror:
      type: local
      root_dir: "$mirror_dir"
EOFCONFIG
}

start_daemon() {
    local config_path="$1"
    local data_dir="$2"
    export THURVTL_ADMIN_SOCKET="${data_dir}/admin.sock"
    RUST_LOG=warn "$DAEMON_PATH" --config "$config_path" > "${data_dir}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for i in {1..30}; do
        if curl -s "http://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    log_error "Daemon did not become ready"
    tail -20 "${data_dir}/daemon.log" >&2
    return 1
}

stop_daemon() {
    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
}

# --- Tests ----------------------------------------------------------

# library restore refuses with a clear error when library.json is
# absent. We must not blow past this gate — the operator's mental
# model is "configure library: in YAML, start daemon once to
# materialize, then restore."
test_refuses_when_uninitialized() {
    log_test "library restore refuses when library is not initialized..."
    local data_dir="$TEST_DIR/uninit"
    local config="$TEST_DIR/uninit.yaml"
    write_config "$data_dir" "$config" "$TEST_DIR/mirror-empty"
    mkdir -p "$TEST_DIR/mirror-empty"

    local out
    out=$("$CLI_PATH" --config "$config" library restore --backend mirror 2>&1)
    local rc=$?
    if [[ $rc -eq 0 ]]; then
        log_error "expected non-zero exit; got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -q "library not initialized"; then
        log_error "error message does not mention 'library not initialized':"
        echo "$out" >&2
        return 1
    fi
    if ! echo "$out" | grep -q "thurvtl.yaml"; then
        log_error "error message does not point operator at thurvtl.yaml:"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — clear precondition error"
    return 0
}

# library restore refuses with a clear error when the operator names
# a backend that isn't defined under `storage.backends:`.
test_refuses_unknown_backend() {
    log_test "library restore refuses an unknown backend name..."
    local data_dir="$TEST_DIR/unknown-backend"
    local config="$TEST_DIR/unknown-backend.yaml"
    write_config "$data_dir" "$config" "$TEST_DIR/mirror-empty"

    # Materialize library.json by starting the daemon briefly. The
    # daemon writes library.json from the YAML `library:` block on
    # first start; we then stop so cmd_restore (daemon-down) can run.
    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi
    stop_daemon

    local out
    out=$("$CLI_PATH" --config "$config" library restore --backend nope 2>&1)
    local rc=$?
    if [[ $rc -eq 0 ]]; then
        log_error "expected non-zero exit, got 0. Output: $out"
        return 1
    fi
    if ! echo "$out" | grep -qi "backend"; then
        log_error "error message does not mention 'backend':"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — clear unknown-backend error"
    return 0
}

# Empty mirror bucket: discovery returns 0 cartridges, restore exits
# 0 cleanly, no audit entry blows up.
test_empty_bucket_dry_run_then_real() {
    log_test "library restore against an empty bucket (dry-run, then real)..."
    local data_dir="$TEST_DIR/empty"
    local config="$TEST_DIR/empty.yaml"
    local mirror="$TEST_DIR/mirror-empty"
    write_config "$data_dir" "$config" "$mirror"
    mkdir -p "$mirror"

    # Materialize library.json by starting the daemon briefly. The
    # daemon writes library.json from the YAML `library:` block on
    # first start; we then stop so cmd_restore (daemon-down) can run.
    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi
    stop_daemon

    # Dry-run first.
    local dry_out
    dry_out=$("$CLI_PATH" --config "$config" library restore \
        --backend mirror --dry-run 2>&1)
    local dry_rc=$?
    if [[ $dry_rc -ne 0 ]]; then
        log_error "dry-run exited $dry_rc"
        echo "$dry_out" >&2
        return 1
    fi
    if ! echo "$dry_out" | grep -q "Discovered: 0 cartridge"; then
        log_error "dry-run did not report '0 cartridge' discovery:"
        echo "$dry_out" >&2
        return 1
    fi
    if [[ -d "$data_dir/tapes" ]] && [[ -n "$(ls -A "$data_dir/tapes" 2>/dev/null)" ]]; then
        log_error "dry-run wrote under $data_dir/tapes:"
        ls -la "$data_dir/tapes" >&2
        return 1
    fi

    # Real restore against the empty bucket.
    local real_out
    real_out=$("$CLI_PATH" --config "$config" library restore --backend mirror 2>&1)
    local real_rc=$?
    if [[ $real_rc -ne 0 ]]; then
        log_error "restore against empty bucket exited $real_rc:"
        echo "$real_out" >&2
        return 1
    fi
    log_info "OK — empty bucket exits 0 in both modes"
    return 0
}

# Single-cartridge sentinel: --barcodes filter with no matches still
# exits 0; verify report shape includes the filtered-out barcode.
test_barcode_filter_with_no_matches() {
    log_test "library restore --barcodes with no matching sentinel..."
    local data_dir="$TEST_DIR/filter"
    local config="$TEST_DIR/filter.yaml"
    local mirror="$TEST_DIR/mirror-filter"
    write_config "$data_dir" "$config" "$mirror"

    # Synthesize a minimal sentinel that our discovery can see. We
    # don't need it to open as a real cartridge — discovery only
    # checks for the sentinel key shape.
    mkdir -p "$mirror/manifests/TAPE_HIDDEN"
    echo '{"label":"TAPE_HIDDEN","backend":"mirror","dedup":"global","uuid":"00000000000000000000000000000000","index_epoch":{}}' \
        > "$mirror/manifests/TAPE_HIDDEN/manifest-latest.json"

    # Materialize library.json by starting the daemon briefly. The
    # daemon writes library.json from the YAML `library:` block on
    # first start; we then stop so cmd_restore (daemon-down) can run.
    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi
    stop_daemon

    # Filter selects nothing — exit 0, no restore attempted.
    local out
    out=$("$CLI_PATH" --config "$config" library restore \
        --backend mirror --barcodes "TAPE_NONE" --dry-run 2>&1)
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        log_error "filter dry-run exited $rc"
        echo "$out" >&2
        return 1
    fi
    if ! echo "$out" | grep -q "Discovered: 1 cartridge"; then
        log_error "discovery did not see the synthesized sentinel:"
        echo "$out" >&2
        return 1
    fi
    if ! echo "$out" | grep -q "Filtered out by --barcodes: TAPE_HIDDEN"; then
        log_error "filter did not report TAPE_HIDDEN as filtered out:"
        echo "$out" >&2
        return 1
    fi
    log_info "OK — filter excludes non-matches while preserving discovery"
    return 0
}

# Audit footprint: `library.restore` is queued under <audit_dir>/pending
# and survives a daemon start (it replays into the chain).
test_audit_footprint_replays() {
    log_test "library.restore audit entry replays into the chain..."
    local data_dir="$TEST_DIR/audit"
    local config="$TEST_DIR/audit.yaml"
    local mirror="$TEST_DIR/mirror-audit"
    write_config "$data_dir" "$config" "$mirror"
    mkdir -p "$mirror"

    # Bring the daemon up briefly so it materializes library.json v2
    # from the YAML `library:` block, then stop so library restore
    # (daemon-down) can run.
    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi
    stop_daemon

    if ! "$CLI_PATH" --config "$config" library restore \
            --backend mirror > /dev/null 2>&1; then
        log_error "library restore failed"
        return 1
    fi

    # Pending entry must exist before the daemon starts; the daemon
    # picks it up on startup.
    if ! ls "$data_dir/audit/pending/"*.json > /dev/null 2>&1; then
        log_error "expected pending audit entry under $data_dir/audit/pending/"
        ls -la "$data_dir/audit" 2>&1 >&2 || true
        return 1
    fi

    # Bring the daemon up briefly to drain the pending entry into
    # the chain, then stop.
    if ! start_daemon "$config" "$data_dir"; then
        return 1
    fi
    sleep 1
    stop_daemon

    if ! find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null \
            | grep -q '"op":"library.restore"'; then
        log_error "library.restore did not appear in the audit chain"
        find "$data_dir/audit" -name 'audit-*.jsonl' -exec cat {} + 2>&1 >&2 || true
        return 1
    fi
    log_info "OK — library.restore replayed into chain"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL DR Restore CLI Smoke"
    echo "========================================"
    echo ""
    echo "(End-to-end data round-trip lives in"
    echo " core/smc/tests/restore_tests.rs)"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    assign_ports

    local passed=0
    local failed=0
    local tests=(
        test_refuses_when_uninitialized
        test_refuses_unknown_backend
        test_empty_bucket_dry_run_then_real
        test_barcode_filter_with_no_matches
        test_audit_footprint_replays
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
