#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Storage Failure-Path Tests
#
# Drives the data-path smoke through `thurvsad --test` with the
# `LocalBackend` failure-injection env var (THUR_STORAGE_INJECT_FAIL) set
# per sub-test, then greps the daemon log for the expected error-class
# strings. CI-friendly: no real storage credentials, no sudo, no kernel iSCSI
# initiator.
#
# Covers two scenarios. The third — "partial-upload resume" — would
# require chunk-level resume logic that the daemon doesn't currently
# implement (whole-page retry only). Tracked in ROADMAP.md.
#
#   1. Auth failure        — inject auth@*; assert
#                            "failed with permanent error (AUTH)" lands
#                            in the daemon log.
#   2. Network timeout     — inject timeout@* with a small retry
#                            budget; assert at least one
#                            "retrying in" line and the final
#                            "failed after N attempts" give-up line.
#
# Other --test sub-tests (data-path round trip, SYNCHRONIZE CACHE,
# sparse-page read) will also hit the injection and emit errors —
# that's expected. This script scores pass/fail on log-content
# patterns, not on the daemon's exit code.
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-fs-storage-failures.sh [OPTIONS]
#
# Options:
#   --debug         Use ./target/debug/ binaries (default is ./target/release/)
#   --keep-data     Don't clean up the test data dir on exit
#

# Not using `set -e` — sub-tests must run independently of each other.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/test-fs-storage-failures-$$"

init_common_daemon_args
parse_common_daemon_args "$@"

: "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"

cleanup() {
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    if [[ ! -x "$DAEMON_PATH" ]]; then
        log_error "Daemon not found at: $DAEMON_PATH (build with: cargo build [--release])"
        exit 1
    fi
}

# Fresh data dir + config for each sub-test. VSA --test mode runs its
# data-path smoke against an in-process LocalBackend created inside a
# private tempdir; the only thing we configure here is data_dir + a
# missing license file to force Community mode.
prepare_fixture() {
    local sub="$1"
    local fixture="${TEST_DIR}/${sub}"
    local data_dir="${fixture}/data"
    local config="${fixture}/config.yaml"

    mkdir -p "$data_dir"

    cat > "$config" <<EOFCONFIG
data_dir: "$data_dir"
license:
  file: "${fixture}/no-such.lic"
EOFCONFIG

    echo "$fixture"
}

# Run `thurvsad --test` with the given THUR_STORAGE_INJECT_FAIL
# value. Captures stderr+stdout to ${fixture}/daemon.log. Returns the
# daemon's exit code (may be non-zero — sub-tests grep the log).
run_under_injection() {
    local fixture="$1"
    local inject="$2"
    local log="${fixture}/daemon.log"

    THUR_STORAGE_INJECT_FAIL="$inject" \
        RUST_LOG=info \
        "$DAEMON_PATH" --test --config "${fixture}/config.yaml" \
        > "$log" 2>&1
    echo "$log"
}

# Sub-test 1: auth failure — assert permanent error landed in log.
test_auth_failure() {
    log_test "auth failure (THUR_STORAGE_INJECT_FAIL=auth@*)"

    local fixture
    fixture=$(prepare_fixture auth)
    local log
    log=$(run_under_injection "$fixture" "auth@*")

    if grep -Eq 'failed with permanent error \(AUTH\)' "$log"; then
        log_info "  ✓ daemon log contains 'failed with permanent error (AUTH)'"
    else
        log_error "  ✗ expected 'failed with permanent error (AUTH)' in daemon log"
        log_error "    last 30 lines of log:"
        tail -30 "$log" >&2
        return 1
    fi

    return 0
}

# Sub-test 2: network timeout with retry budget — assert retry log
# lines AND the final give-up line both appear.
test_network_timeout_with_retry() {
    log_test "network timeout (THUR_STORAGE_INJECT_FAIL=timeout@*)"

    local fixture
    fixture=$(prepare_fixture timeout)
    local log
    log=$(run_under_injection "$fixture" "timeout@*")

    if grep -Eq 'failed \(attempt [0-9]+/[0-9]+\):.*retrying in' "$log"; then
        log_info "  ✓ daemon log contains at least one retry line"
    else
        log_error "  ✗ expected 'failed (attempt N/M): ... retrying in' in daemon log"
        log_error "    last 30 lines of log:"
        tail -30 "$log" >&2
        return 1
    fi

    if grep -Eq 'failed after [0-9]+ attempts' "$log"; then
        log_info "  ✓ daemon log contains the 'failed after N attempts' give-up line"
    else
        log_error "  ✗ expected 'failed after N attempts' give-up line in daemon log"
        log_error "    last 30 lines of log:"
        tail -30 "$log" >&2
        return 1
    fi

    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Storage Failure-Path Tests"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"

    local passed=0
    local failed=0
    local tests=(
        "test_auth_failure"
        "test_network_timeout_with_retry"
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
        log_info "✓ All failure-path tests passed"
        exit 0
    else
        log_error "✗ $failed sub-test(s) failed"
        echo "Debug: test artifacts kept under $TEST_DIR (re-run with --keep-data)"
        exit 1
    fi
}

main
