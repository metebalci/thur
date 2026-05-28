#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Many-Cartridge Lifecycle Soak
#
# Stresses the cartridge directory, library inventory, audit chain
# (one row per create), and chunk-pool book-keeping by creating /
# listing / destroying N cartridges in sequence. Without sudo /
# kernel initiator the host-write side stays empty; the goal here is
# to exercise the *metadata* lifecycle at scale and confirm:
#
#   1. N cartridges create cleanly without slot exhaustion.
#   2. `system audit verify` still validates the chain after the
#      burst.
#   3. Library inventory still reads cleanly after the operations.
#
# Gated behind `THURVTL_SOAK=1` — run-on-demand only.
#
# Usage (invoke from repo root):
#   THURVTL_SOAK=1 ./vtl/scripts/test-lifecycle-many-cartridges.sh [--debug]
#

set -u

if [[ "${THURVTL_SOAK:-0}" != "1" ]]; then
    echo "[INFO] gated behind THURVTL_SOAK=1. Re-run with THURVTL_SOAK=1 to execute."
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvtl-many-tape-$$"
NUM_CARTRIDGES=30

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --num-cartridges) NUM_CARTRIDGES="$2"; shift 2 ;;
        *)
            if parse_common_daemon_arg "$@"; then
                shift "$_CONSUMED_ARGS"
            else
                echo "Unknown option: $1" >&2
                exit 1
            fi
            ;;
    esac
done

cleanup() {
    standard_cleanup
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    require_daemon_binaries thurvtl
    command -v curl >/dev/null || { log_error "curl required"; exit 1; }
}

start_daemon() {
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)
    local slots=$(( NUM_CARTRIDGES + 4 ))
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "${TEST_DIR}/data"
library:
  num_slots: $slots
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
      root_dir: "${TEST_DIR}/local-backend"
EOFCONFIG
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    TEST_CONFIG="${TEST_DIR}/config.yaml" RUST_LOG=warn start_thur_daemon
}

test_creates_n_cartridges() {
    log_test "create $NUM_CARTRIDGES cartridges — library + audit scale"

    local i created=0
    for ((i = 1; i <= NUM_CARTRIDGES; i++)); do
        local name
        name=$(printf "TAPE%03d" "$i")
        if "$CLI_PATH" --config "${TEST_DIR}/config.yaml" \
                cartridge create "$name" >/dev/null 2>&1; then
            ((created++))
        else
            log_error "  ✗ cartridge create '$name' failed at $created"
            return 1
        fi
    done
    log_info "  ✓ all $NUM_CARTRIDGES cartridges created"

    local listed
    listed=$("$CLI_PATH" --config "${TEST_DIR}/config.yaml" cartridge list 2>&1 \
        | grep -oE "TAPE[0-9]{3}" | sort -u | wc -l)
    if (( listed != NUM_CARTRIDGES )); then
        log_error "  ✗ cartridge list shows $listed / $NUM_CARTRIDGES"
        return 1
    fi
    log_info "  ✓ cartridge list enumerates all"
    return 0
}

test_audit_chain_still_validates_after_burst() {
    log_test "audit verify clean after $NUM_CARTRIDGES creates"
    if ! "$CLI_PATH" --config "${TEST_DIR}/config.yaml" system audit verify >/dev/null 2>&1; then
        log_error "  ✗ audit verify failed"
        return 1
    fi
    log_info "  ✓ audit chain valid"
    return 0
}

test_system_stats_after_burst() {
    log_test "system stats traverses all cartridges without error"
    if ! "$CLI_PATH" --config "${TEST_DIR}/config.yaml" system stats >/dev/null 2>&1; then
        log_error "  ✗ system stats failed"
        return 1
    fi
    log_info "  ✓ system stats clean"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL Many-Cartridge Lifecycle ($NUM_CARTRIDGES)"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1

    local passed=0 failed=0
    local tests=(
        "test_creates_n_cartridges"
        "test_audit_chain_still_validates_after_burst"
        "test_system_stats_after_burst"
    )
    for t in "${tests[@]}"; do
        if $t; then ((passed++)); else ((failed++)); fi
        echo ""
    done

    echo "Total: $((passed + failed))  Passed: $passed  Failed: $failed"
    [[ $failed -eq 0 ]] && exit 0 || exit 1
}

main
