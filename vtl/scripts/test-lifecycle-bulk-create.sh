#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Bulk Cartridge-Create Lifecycle Test (issue #279)
#
# Exercises the `cartridge create --multi N` batch path through the admin
# socket. The handler creates each cartridge's on-disk dir off the library
# lock (spawn_blocking), then seats the whole batch into storage slots under
# one short lock with a single inventory persist. This verifies the
# observable contract of that restructure:
#   - a --multi N batch seats all N cartridges atomically,
#   - an over-capacity batch clean-fails ("not enough free slots") with NO
#     partial seating and NO orphaned tape dirs left on disk (rollback),
#   - the audit chain stays valid across the mixed success/failure batch.
#
# No sudo / no kernel iSCSI required: pure admin-socket + CLI + filesystem
# assertions (the create path never touches the SCSI data path).
#
# Companions:
#   - test-lifecycle-many-cartridges.sh — single-create scale soak
#   - test-smoke.sh                      — management-surface smoke
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-lifecycle-bulk-create.sh [OPTIONS]
#
# Options:
#   --debug               Use ./target/debug/ binaries (default release)
#   --daemon-path PATH    Path to thurvtld binary (overrides default)
#   --cli-path PATH       Path to thurvtl binary (overrides default)
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

# Note: no 'set -e' — we want every assertion to run even if one fails.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvtl-bulk-create-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    standard_cleanup
}
trap cleanup EXIT INT TERM

FAIL=0

start_daemon() {
    require_daemon_binaries thurvtl
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "${TEST_CONFIG}" <<EOFCONFIG
data_dir: "${TEST_DIR}/data"
library:
  num_slots: 10
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
    RUST_LOG=warn start_thur_daemon
}

cli() { "$CLI_PATH" --config "${TEST_CONFIG}" "$@"; }

seated_count() {
    cli cartridge list 2>&1 | grep -oE "TAPE[0-9]{3}" | sort -u | wc -l
}

main() {
    echo "========================================"
    echo "Thur VTL Bulk Cartridge-Create Test (#279)"
    echo "========================================"
    start_daemon

    # 1. Bulk-create 8 into a 10-slot chassis — all seat atomically.
    log_test "cartridge create --multi 8 seats all 8"
    if cli cartridge create TAPE001 --multi 8 >/dev/null 2>&1; then
        log_info "  create --multi 8 returned ok"
    else
        log_error "  create --multi 8 failed"; FAIL=1
    fi
    n=$(seated_count)
    if (( n == 8 )); then log_info "  ✓ 8 cartridges seated"; else log_error "  ✗ seated $n / 8"; FAIL=1; fi

    # 2. Over-capacity batch (5 requested, 2 free) must clean-fail.
    log_test "over-capacity --multi 5 rejected with no partial seating"
    out=$(cli cartridge create TAPE100 --multi 5 2>&1); rc=$?
    if (( rc != 0 )) && grep -qi "not enough free slots" <<<"$out"; then
        log_info "  ✓ rejected: not enough free slots"
    else
        log_error "  ✗ expected rejection (rc=$rc): $out"; FAIL=1
    fi
    n=$(seated_count)
    if (( n == 8 )); then log_info "  ✓ inventory unchanged (still 8)"; else log_error "  ✗ partial seating: $n"; FAIL=1; fi

    # 3. No orphaned tape dirs for the rolled-back barcodes.
    log_test "rolled-back batch leaves no orphan tape dirs on disk"
    orphans=0
    for nn in 100 101 102 103 104; do
        [[ -e "${TEST_DIR}/data/tapes/TAPE$nn" ]] && orphans=$((orphans+1))
    done
    if (( orphans == 0 )); then log_info "  ✓ no orphan dirs"; else log_error "  ✗ $orphans orphan dir(s)"; FAIL=1; fi

    # 4. A second batch that exactly fills the remaining 2 slots succeeds.
    log_test "fill remaining 2 slots with --multi 2"
    if cli cartridge create TAPE200 --multi 2 >/dev/null 2>&1; then log_info "  create --multi 2 ok"; else log_error "  create --multi 2 failed"; FAIL=1; fi
    n=$(seated_count)
    if (( n == 10 )); then log_info "  ✓ chassis full (10 seated)"; else log_error "  ✗ seated $n / 10"; FAIL=1; fi

    # 5. Audit chain still valid after the mixed success/failure run.
    log_test "audit chain valid after mixed batch run"
    if cli system audit verify >/dev/null 2>&1; then log_info "  ✓ audit chain valid"; else log_error "  ✗ audit verify failed"; FAIL=1; fi

    echo "========================================"
    if (( FAIL == 0 )); then
        log_info "All bulk-create tests passed"
    else
        log_error "Bulk-create tests FAILED"
    fi
    return $FAIL
}

main
exit $FAIL
