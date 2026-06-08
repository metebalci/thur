#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Legal Hold Lifecycle (cloud-native)
#
# End-to-end CLI coverage of `thurvtl cartridge legal-hold
# {set,clear,status}` and the migrate gate that refuses a held
# cartridge. Legal hold is cloud-native — the provider's per-object
# hold primitive (S3 PutObjectLegalHold / GCS eventBasedHold / Azure
# legal hold) is the only source of truth, so this test REQUIRES a
# storage backend whose bucket/container has Object Lock (S3) or the
# equivalent legal-hold capability enabled. It cannot run against a
# `local` backend and skips cleanly when one is selected.
#
# Flow (one data-bearing cartridge):
#   write data via iSCSI -> wait for the manifest sentinel to land
#   -> status (not held) -> set hold -> status (HELD)
#   -> migrate --dry-run (refused: under legal hold)
#   -> clear hold -> status (not held)
#   -> migrate --dry-run (now permitted)
#
# The migrate gate fires before the dry-run short-circuits, so the
# refusal/permit assertions need no real cross-backend data movement.
#
# Selection: set THURVTL_TEST_BACKEND to the name of an entry under
# `storage.backends:` in $THURVTL_SOURCE_BACKENDS (default
# private/storage-backends.yaml). That entry MUST:
#   - not be `type: local`            (legal hold is cloud-only)
#   - have `retention_mode: none`     (so the daemon starts and the
#                                       test can clear its own holds)
#   - point at a bucket with Object Lock ENABLED (PutObjectLegalHold
#     fails otherwise — the test reports that as a clear error)
# A bucket with Object Lock enabled but NO default retention rule is
# ideal: legal hold works and the test can delete its objects after
# clearing the hold. If the bucket carries a default retention rule,
# written objects may linger past the run (no BypassGovernanceRetention
# in the minimal IAM policy) — cleanup is best-effort and warns.
#
# Prerequisites: same as test-backup-storage.sh (mtx, mt-st,
# open-iscsi, tar, lsscsi, curl, yq, the backend CLI matching the
# type) + root/sudo for iSCSI. Do NOT prefix with sudo — the script
# self-elevates and forwards backend-credential env vars.
#
# Usage (invoke from repo root, WITHOUT sudo):
#   THURVTL_TEST_BACKEND=governance ./vtl/scripts/test-legal-hold-lifecycle.sh [OPTIONS]
#
# Options:
#   --debug          Use ./target/debug/ binaries (default: release)
#   --keep-data      Don't clean up local test data directory
#   --keep-iscsi     Don't disconnect iSCSI session after tests
#   --keep-storage   Don't purge the test sub-prefix from the bucket
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials before self-elevation
# so `sudo KEY=VAL ...` can forward them (see test-backup-storage.sh).
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

if [[ $EUID -ne 0 ]]; then
    forward=()
    for v in $(compgen -A variable); do
        case "$v" in
            AWS_*|GOOGLE_*|GCS_*|AZURE_*|AISTOR_*|WASABI_*|MINIO_*|THURVTL_*)
                [[ -n "${!v}" ]] && forward+=("$v=${!v}")
                ;;
        esac
    done
    echo "[INFO] Re-executing under sudo with ${#forward[@]} env vars forwarded..."
    exec sudo "${forward[@]}" "$0" "$@"
fi

source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
TEST_DIR="/tmp/test-legal-hold-lifecycle-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
BARCODE="HOLD01L8"
FIXTURE_MB="${THURVTL_FIXTURE_MB:-8}"
if (( FIXTURE_MB < 8 )); then FIXTURE_MB=8; fi
MANIFEST_WAIT_SECS=$((120 + (FIXTURE_MB > 8 ? FIXTURE_MB - 8 : 0)))
if (( MANIFEST_WAIT_SECS > 600 )); then MANIFEST_WAIT_SECS=600; fi

KEEP_ISCSI=0
KEEP_STORAGE=0
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
TAPE_DEVICE=""
NOREWIND_DEVICE=""

BACKEND_TYPE=""
BACKEND_BUCKET=""
BACKEND_ENDPOINT=""
BACKEND_REGION=""
BACKEND_ACCOUNT=""
BACKEND_CONTAINER=""
BACKEND_AUTH_AKID_ENV=""
BACKEND_AUTH_SECRET_ENV=""
ORIG_PREFIX=""
TEST_PREFIX=""
RUN_ID=""

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --keep-storage) KEEP_STORAGE=1; shift ;;
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

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    # Best-effort: drop any hold we may have left on (so objects become
    # deletable). Needs the daemon up; ignore failures.
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold clear "$BARCODE" \
            --reason test-cleanup >/dev/null 2>&1 || true
    fi

    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsi_logout_and_delete
    fi
    stop_thur_daemon

    if [[ $KEEP_STORAGE -eq 0 && -n "$BACKEND_TYPE" && -n "$TEST_PREFIX" ]]; then
        log_info "Purging storage test prefix: ${BACKEND_BUCKET:-?}/${TEST_PREFIX} (best-effort; locked objects may linger)"
        storage_purge_test_prefix || log_warn "purge incomplete — bucket retention may be holding objects"
    elif [[ $KEEP_STORAGE -eq 1 && -n "$TEST_PREFIX" ]]; then
        log_warn "Keeping storage test prefix: ${BACKEND_BUCKET:-?}/${TEST_PREFIX}"
    fi

    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

resolve_backend() {
    if [[ -z "${THURVTL_TEST_BACKEND:-}" ]]; then
        log_error "THURVTL_TEST_BACKEND is not set."
        echo "Legal hold is cloud-native; set it to a non-local, Object-Lock-enabled backend"
        echo "defined in $SOURCE_BACKENDS. Example:"
        echo "  THURVTL_TEST_BACKEND=governance $0"
        exit 1
    fi
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Cannot read source backends file: $SOURCE_BACKENDS"
        exit 1
    fi
    if ! command -v yq >/dev/null 2>&1; then
        log_error "yq is required to parse $SOURCE_BACKENDS"
        exit 1
    fi

    local exists
    exists=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" || "$exists" == "null" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' not found in $SOURCE_BACKENDS"
        yq -r '.storage.backends | keys | .[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi

    BACKEND_TYPE=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
    BACKEND_BUCKET=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
    ORIG_PREFIX=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_AKID_ENV=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".auth | select(.type == \"env\") | .access_key_id_env // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".auth | select(.type == \"env\") | .secret_access_key_env // \"\"" "$SOURCE_BACKENDS")
    local retention
    retention=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")

    if [[ "$BACKEND_TYPE" == "local" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' is type 'local' — legal hold is cloud-native and cannot be tested locally."
        echo "Select a storage backend whose bucket has Object Lock / legal-hold enabled."
        exit 1
    fi
    # We declare retention_mode: none in the test config regardless (the
    # daemon's startup lock-state check only runs for governance/
    # compliance). A source entry already carrying a non-none mode would
    # auto-apply retention to our writes and block cleanup — refuse it,
    # same as test-backup-storage.sh.
    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' has retention_mode='$retention' — refusing."
        echo "Use an Object-Lock-enabled bucket declared with retention_mode: none so legal"
        echo "hold works while the test can still delete its own (un-held) objects."
        exit 1
    fi

    RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
    local prefix_clean="${ORIG_PREFIX%/}"
    if [[ -n "$prefix_clean" ]]; then
        TEST_PREFIX="${prefix_clean}/test-runs/${RUN_ID}/"
    else
        TEST_PREFIX="test-runs/${RUN_ID}/"
    fi

    log_info "Backend:          $THURVTL_TEST_BACKEND (type=$BACKEND_TYPE)"
    log_info "Bucket/container: ${BACKEND_BUCKET}${BACKEND_CONTAINER}"
    log_info "Test sub-prefix:  $TEST_PREFIX"
}

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=()
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"
    [[ -x "$DAEMON_PATH" ]] || { command -v thurvtld >/dev/null 2>&1 && DAEMON_PATH=$(command -v thurvtld) || missing+=("thurvtld"); }
    [[ -x "$CLI_PATH" ]] || { command -v thurvtl >/dev/null 2>&1 && CLI_PATH=$(command -v thurvtl) || missing+=("thurvtl"); }
    for tool in mtx mt iscsiadm tar lsscsi curl yq jq; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    local cli; cli=$(storage_cli_for_type)
    if [[ -n "$cli" ]] && ! command -v "$cli" >/dev/null 2>&1; then
        missing+=("$cli")
    fi
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        exit 1
    fi
    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running. Start it: sudo systemctl enable --now iscsid open-iscsi"
            exit 1
        fi
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

# Test config: the chosen storage backend cloned in as `testbackend`
# (retention_mode forced to none, prefix scoped to the test run), plus
# a throwaway local `migsink` used only as the migrate --dry-run target
# (the gate reads the SOURCE sentinel, so no real movement happens).
create_test_config() {
    log_info "Creating test configuration (storage backend cloned from $SOURCE_BACKENDS)..."
    mkdir -p "$TEST_DIR/data" "$TEST_DIR/migsink"
    if [[ -n "${SUDO_USER:-}" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local backend_json
    backend_json=$(yq -c \
        ".storage.backends.\"$THURVTL_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\", retention_mode: \"none\" }" \
        "$SOURCE_BACKENDS")
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"
$(yaml_vtl_library 4 1 8)
http:
  listen: "127.0.0.1:$HTTP_PORT"
$(yaml_iscsi "$TARGET_IQN")
disk_cache:
  disk_free_min_gb: 0
storage:
  backends:
    testbackend: $backend_json
    migsink:
      type: local
      root_dir: "$TEST_DIR/migsink"
EOFCONFIG
}

start_daemon() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    TEST_CONFIG="$TEST_CONFIG" DAEMON_LOG="${TEST_DIR}/daemon.log" RUST_LOG=warn start_thur_daemon
}

connect_iscsi() {
    iscsi_discover_and_login
    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; exit 1; }
    TAPE_DEVICE=$(lsscsi | awk '/tape/{print $NF}' | head -1)
    [[ -n "$TAPE_DEVICE" ]] || { log_error "Tape device not found"; lsscsi; exit 1; }
    NOREWIND_DEVICE=$(echo "$TAPE_DEVICE" | sed 's|/dev/st|/dev/nst|')
    log_info "Changer: $CHANGER_DEVICE   Tape: $TAPE_DEVICE (no-rewind: $NOREWIND_DEVICE)"
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
}

make_fixture() {
    local dir="$1"
    mkdir -p "$dir"
    local bytes=$((FIXTURE_MB * 1024 * 1024))
    openssl enc -aes-256-ctr -pass "pass:legal-hold" -nosalt \
        -in <(head -c "$bytes" /dev/zero) -out "$dir/seeded.bin" 2>/dev/null
    echo "legal-hold fixture mb=$FIXTURE_MB" > "$dir/marker.txt"
}

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

PASSED=0
FAILED=0
run_test() {
    local name="$1"; shift
    log_test "$name"
    if "$@"; then
        log_pass "$name"; PASSED=$((PASSED + 1))
    else
        log_fail "$name"; FAILED=$((FAILED + 1))
        echo "----- last 30 daemon log lines -----"
        tail -30 "${TEST_DIR}/daemon.log" 2>/dev/null | sed 's/^/  /'
    fi
    echo ""
}

# Write a data-bearing cartridge and wait for the storage sentinel
# (manifests/<bc>/manifest-latest.json) — legal hold has nothing to act
# on until objects exist on the backend.
test_write_and_seal() {
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || return 1
    make_fixture "$TEST_DIR/fixture"
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$TEST_DIR/fixture" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || return 1
    log_info "Waiting for storage sentinel (up to ${MANIFEST_WAIT_SECS}s)..."
    storage_wait_for_key "manifests/${BARCODE}/manifest-latest.json" "$MANIFEST_WAIT_SECS"
}

test_status_not_held_initially() {
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold status "$BARCODE" 2>&1)
    echo "$out" | grep -qi "not held" || { log_error "expected 'not held', got: $out"; return 1; }
}

test_set_hold() {
    local out rc
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold set "$BARCODE" \
        --id case-2026-test --reason litigation-hold 2>&1); rc=$?
    if [[ $rc -ne 0 ]]; then
        log_error "set hold failed (rc=$rc): $out"
        echo "$out" | grep -qi "object lock\|not enabled\|InvalidRequest" && \
            log_error "HINT: the backend bucket likely does NOT have Object Lock enabled."
        return 1
    fi
    echo "$out" | grep -qi "succeeded" || { log_error "set output unexpected: $out"; return 1; }
}

test_status_held() {
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold status "$BARCODE" 2>&1)
    echo "$out" | grep -q "HELD" || { log_error "expected 'HELD', got: $out"; return 1; }
}

# Migrate gate refuses a held cartridge — even on --dry-run, the hold
# check fires first. Target is the throwaway local migsink; no data
# moves because the gate refuses before the plan.
test_migrate_refused_while_held() {
    local out rc
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge migrate "$BARCODE" \
        --target-backend migsink --mode move --dry-run 2>&1); rc=$?
    if [[ $rc -eq 0 ]]; then
        log_error "expected migrate to be refused while held; got exit 0. Output: $out"
        return 1
    fi
    echo "$out" | grep -qi "legal hold" || { log_error "refusal did not mention legal hold: $out"; return 1; }
}

test_clear_hold() {
    local out rc
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold clear "$BARCODE" \
        --id case-2026-test --reason litigation-settled 2>&1); rc=$?
    [[ $rc -eq 0 ]] || { log_error "clear hold failed (rc=$rc): $out"; return 1; }
    echo "$out" | grep -qi "succeeded" || { log_error "clear output unexpected: $out"; return 1; }
}

test_status_not_held_after_clear() {
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold status "$BARCODE" 2>&1)
    echo "$out" | grep -qi "not held" || { log_error "expected 'not held' after clear, got: $out"; return 1; }
}

# With the hold cleared, the gate no longer blocks. We assert the
# refusal is gone (output no longer cites legal hold) rather than a
# clean exit 0 — a storage->local dry-run may still decline for unrelated
# reasons, but it must not be a legal-hold refusal. Cross-backend move
# mechanics are covered by test-lifecycle-cartridge-migrate.sh.
test_migrate_permitted_after_clear() {
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge migrate "$BARCODE" \
        --target-backend migsink --mode move --dry-run 2>&1)
    if echo "$out" | grep -qi "legal hold"; then
        log_error "migrate still cites legal hold after clear: $out"
        return 1
    fi
}

main() {
    echo "================================================"
    echo "Thur VTL Legal Hold Lifecycle (cloud-native)"
    echo "================================================"
    echo ""

    resolve_backend
    check_prerequisites
    verify_storage_creds || {
        echo "Set storage credentials in your user shell (or private/thur.env), then re-run (no sudo prefix)."
        exit 1
    }
    assign_ports
    create_test_config
    start_daemon
    if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$BARCODE" \
            --lto-generation 8 --backend testbackend --dedup local >/dev/null 2>&1; then
        log_error "cartridge create failed"; tail -20 "${TEST_DIR}/daemon.log" >&2; exit 1
    fi
    connect_iscsi

    echo ""
    echo "Running legal-hold lifecycle tests..."
    echo "-------------------------------------"
    echo ""

    run_test "write data + seal storage sentinel"          test_write_and_seal
    run_test "status: not held initially"                test_status_not_held_initially
    run_test "set legal hold"                            test_set_hold
    run_test "status: HELD"                              test_status_held
    run_test "migrate refused while held (dry-run)"      test_migrate_refused_while_held
    run_test "clear legal hold"                          test_clear_hold
    run_test "status: not held after clear"              test_status_not_held_after_clear
    run_test "migrate permitted after clear (dry-run)"   test_migrate_permitted_after_clear

    echo "================================================"
    echo "Test Summary"
    echo "================================================"
    echo "Backend:     $THURVTL_TEST_BACKEND ($BACKEND_TYPE)"
    echo "Test prefix: $TEST_PREFIX"
    echo "Passed:      $PASSED"
    echo "Failed:      $FAILED"
    echo ""
    if [[ $FAILED -eq 0 ]]; then
        log_pass "All legal-hold lifecycle tests passed"
        exit 0
    else
        log_fail "$FAILED test(s) failed"
        echo "Debug: ${TEST_DIR}/daemon.log"
        exit 1
    fi
}

main
