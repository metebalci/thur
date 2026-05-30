#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Tiering x Legal Hold Interaction (cloud-native)
#
# Asserts the safety-critical gate where cartridge tiering meets legal
# hold: a cartridge under legal hold is NEVER moved by tiering — it is
# excluded at plan time (TieringPlanReport.excluded_legal_hold) and
# never attempted by run-now. Clearing the hold lifts the exclusion.
#
# Legal hold is cloud-native (provider per-object hold is the only
# source of truth), so this REQUIRES a cloud backend with Object Lock /
# legal-hold enabled and cannot run against `local`. See
# test-legal-hold-lifecycle.sh for the backend/bucket requirements;
# selection is identical (THURVTL_TEST_BACKEND, retention_mode: none,
# Object Lock enabled on the bucket).
#
# Fixture: two data-bearing cartridges on the cloud backend (`hot`):
#   TIERHOLD1 — under legal hold
#   TIERMOVE1 — not held
# A single policy matches barcode prefix "TIER" -> local backend `cold`.
#
# Assertions (depend only on cloud upload + legal-hold reads + the plan
# engine — NOT on the cloud->local move actually completing, which is
# covered by test-lifecycle-cartridge-migrate.sh):
#   - plan: TIERHOLD1 in excluded_legal_hold, not in moves;
#           TIERMOVE1 in moves (hot -> cold); under_legal_hold == 1
#   - run-now: TIERHOLD1 in excluded_legal_hold; its manifest.backend
#              is unchanged (the held cartridge was never moved)
#   - after clearing the hold: plan now lists TIERHOLD1 in moves
#
# Prerequisites + sudo behavior: identical to test-backup-storage.sh /
# test-legal-hold-lifecycle.sh. Do NOT prefix with sudo — self-elevates.
#
# Usage (invoke from repo root, WITHOUT sudo):
#   THURVTL_TEST_BACKEND=governance ./vtl/scripts/test-tiering-legal-hold-interaction.sh [OPTIONS]
#
# Options:
#   --debug          Use ./target/debug/ binaries (default: release)
#   --keep-data      Don't clean up local test data directory
#   --keep-iscsi     Don't disconnect iSCSI session after tests
#   --keep-storage   Don't purge the test sub-prefix from the bucket
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

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
TEST_DIR="/tmp/test-tiering-legal-hold-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
HELD_BC="TIERHOLD1"
MOVE_BC="TIERMOVE1"
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
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold clear "$HELD_BC" \
            --reason test-cleanup >/dev/null 2>&1 || true
    fi
    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsi_logout_and_delete
    fi
    stop_thur_daemon
    if [[ $KEEP_STORAGE -eq 0 && -n "$BACKEND_TYPE" && -n "$TEST_PREFIX" ]]; then
        log_info "Purging storage test prefix (best-effort): ${BACKEND_BUCKET:-?}/${TEST_PREFIX}"
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
        log_error "THURVTL_TEST_BACKEND is not set (cloud, Object-Lock-enabled backend required)."
        echo "Example: THURVTL_TEST_BACKEND=governance $0"
        exit 1
    fi
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Cannot read source backends file: $SOURCE_BACKENDS"; exit 1
    fi
    if ! command -v yq >/dev/null 2>&1; then
        log_error "yq is required to parse $SOURCE_BACKENDS"; exit 1
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
        log_error "Backend '$THURVTL_TEST_BACKEND' is type 'local' — legal hold is cloud-native."
        exit 1
    fi
    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' has retention_mode='$retention' — refusing (cleanup would be blocked)."
        echo "Use an Object-Lock-enabled bucket declared with retention_mode: none."
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
    if [[ -n "$cli" ]] && ! command -v "$cli" >/dev/null 2>&1; then missing+=("$cli"); fi
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"; exit 1
    fi
    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) not running. Start: sudo systemctl enable --now iscsid open-iscsi"; exit 1
        fi
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

# Cloud backend as `hot` (retention_mode forced none, prefix scoped),
# local `cold` as the tiering target, and a barcode-prefix policy.
create_test_config() {
    log_info "Creating test configuration (cloud backend cloned from $SOURCE_BACKENDS)..."
    mkdir -p "$TEST_DIR/data" "$TEST_DIR/cold"
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
    hot: $backend_json
    cold:
      type: local
      root_dir: "$TEST_DIR/cold"
tiering:
  policies:
    - predicates:
        barcode_prefix: "TIER"
      migrate_to: cold
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

manifest_backend_for() {
    jq -r '.backend' "$TEST_DIR/data/tapes/$1/manifest.json"
}

# Load slot $1 to drive 0, tar a fresh fixture, unload. Each cartridge
# gets distinct bytes (seed = barcode) so they don't dedup to nothing.
write_slot() {
    local slot="$1"
    local seed="$2"
    local fix="$TEST_DIR/fixture-$seed"
    mkdir -p "$fix"
    openssl enc -aes-256-ctr -pass "pass:$seed" -nosalt \
        -in <(head -c "$((FIXTURE_MB * 1024 * 1024))" /dev/zero) -out "$fix/seeded.bin" 2>/dev/null
    echo "fixture $seed" > "$fix/marker.txt"
    mtx -f "$CHANGER_DEVICE" load "$slot" 0 >/dev/null 2>&1 || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$fix" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    mtx -f "$CHANGER_DEVICE" unload "$slot" 0 >/dev/null 2>&1 || return 1
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

# plan: held excluded (not a move), unheld is a move, under_legal_hold==1.
test_plan_excludes_held() {
    local plan rc
    plan=$("$CLI_PATH" --config "$TEST_CONFIG" system tiering plan --json 2>/dev/null); rc=$?
    if [[ $rc -ne 0 ]]; then
        log_error "tiering plan exited $rc: $plan"; return 1
    fi
    if ! echo "$plan" | jq -e --arg b "$HELD_BC" '.excluded_legal_hold | index($b)' >/dev/null; then
        log_error "$HELD_BC not in excluded_legal_hold:"; echo "$plan" >&2; return 1
    fi
    if echo "$plan" | jq -e --arg b "$HELD_BC" '.moves[] | select(.barcode==$b)' >/dev/null; then
        log_error "$HELD_BC wrongly appears in moves (held must never be a move candidate):"; echo "$plan" >&2; return 1
    fi
    if ! echo "$plan" | jq -e --arg b "$MOVE_BC" '.moves[] | select(.barcode==$b and .from_backend=="hot" and .to_backend=="cold")' >/dev/null; then
        log_error "$MOVE_BC not proposed hot -> cold:"; echo "$plan" >&2; return 1
    fi
    if ! echo "$plan" | jq -e '.under_legal_hold==1 or (.excluded_legal_hold | length)==1' >/dev/null; then
        log_error "expected exactly one cartridge under legal hold:"; echo "$plan" >&2; return 1
    fi
    return 0
}

# run-now: held cartridge excluded and NOT moved (manifest.backend stays
# hot). We assert on JSON content + manifest, not on run-now's exit code
# (the unheld cartridge's cloud->local move is exercised elsewhere).
test_run_now_never_moves_held() {
    local run
    run=$("$CLI_PATH" --config "$TEST_CONFIG" system tiering run-now --json 2>/dev/null)
    if ! echo "$run" | jq -e --arg b "$HELD_BC" '.excluded_legal_hold | index($b)' >/dev/null; then
        log_error "$HELD_BC not in run-now excluded_legal_hold:"; echo "$run" >&2; return 1
    fi
    if echo "$run" | jq -e --arg b "$HELD_BC" '.migrated[] | select(.barcode==$b)' >/dev/null; then
        log_error "$HELD_BC appears in run-now migrated — a held cartridge was moved!"; echo "$run" >&2; return 1
    fi
    local held_backend
    held_backend=$(manifest_backend_for "$HELD_BC")
    if [[ "$held_backend" != "hot" ]]; then
        log_error "$HELD_BC manifest.backend changed to '$held_backend' — held cartridge was moved!"
        return 1
    fi
    return 0
}

# Clearing the hold lifts the exclusion: the cartridge becomes a move
# candidate in a fresh plan.
test_cleared_hold_becomes_movable() {
    local out rc
    out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold clear "$HELD_BC" \
        --reason interaction-test 2>&1); rc=$?
    if [[ $rc -ne 0 ]]; then
        log_error "clear hold failed (rc=$rc): $out"; return 1
    fi
    local plan
    plan=$("$CLI_PATH" --config "$TEST_CONFIG" system tiering plan --json 2>/dev/null)
    if echo "$plan" | jq -e --arg b "$HELD_BC" '.excluded_legal_hold | index($b)' >/dev/null; then
        log_error "$HELD_BC still excluded after clearing the hold:"; echo "$plan" >&2; return 1
    fi
    if ! echo "$plan" | jq -e --arg b "$HELD_BC" '.moves[] | select(.barcode==$b)' >/dev/null; then
        log_error "$HELD_BC not a move candidate after clearing the hold:"; echo "$plan" >&2; return 1
    fi
    return 0
}

main() {
    echo "================================================"
    echo "Thur VTL Tiering x Legal Hold Interaction"
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

    # Create HELD first (slot 1), MOVE second (slot 2) — slot assignment
    # is sequential from 1, matching test-backup-storage.sh's convention.
    for bc in "$HELD_BC" "$MOVE_BC"; do
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$bc" \
                --lto-generation 8 --backend hot --dedup local >/dev/null 2>&1; then
            log_error "cartridge create $bc failed"; tail -20 "${TEST_DIR}/daemon.log" >&2; exit 1
        fi
    done
    connect_iscsi

    log_info "Writing data to both cartridges..."
    write_slot 1 "$HELD_BC" || { log_error "write to $HELD_BC failed"; exit 1; }
    write_slot 2 "$MOVE_BC" || { log_error "write to $MOVE_BC failed"; exit 1; }

    log_info "Waiting for both cloud sentinels (up to ${MANIFEST_WAIT_SECS}s each)..."
    storage_wait_for_key "manifests/${HELD_BC}/manifest-latest.json" "$MANIFEST_WAIT_SECS" \
        || { log_error "sentinel for $HELD_BC never landed"; exit 1; }
    storage_wait_for_key "manifests/${MOVE_BC}/manifest-latest.json" "$MANIFEST_WAIT_SECS" \
        || { log_error "sentinel for $MOVE_BC never landed"; exit 1; }

    log_info "Applying legal hold to $HELD_BC..."
    local set_out
    set_out=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge legal-hold set "$HELD_BC" \
        --id interaction-test --reason litigation-hold 2>&1)
    if [[ $? -ne 0 ]] || ! echo "$set_out" | grep -qi succeeded; then
        log_error "could not apply legal hold to $HELD_BC: $set_out"
        echo "$set_out" | grep -qi "object lock\|not enabled\|InvalidRequest" && \
            log_error "HINT: the backend bucket likely does NOT have Object Lock enabled."
        exit 1
    fi

    echo ""
    echo "Running tiering x legal-hold interaction tests..."
    echo "-------------------------------------------------"
    echo ""

    run_test "plan excludes the held cartridge, proposes the unheld one" test_plan_excludes_held
    run_test "run-now never moves the held cartridge"                    test_run_now_never_moves_held
    run_test "clearing the hold makes the cartridge movable again"       test_cleared_hold_becomes_movable

    echo "================================================"
    echo "Test Summary"
    echo "================================================"
    echo "Backend:     $THURVTL_TEST_BACKEND ($BACKEND_TYPE)"
    echo "Test prefix: $TEST_PREFIX"
    echo "Passed:      $PASSED"
    echo "Failed:      $FAILED"
    echo ""
    if [[ $FAILED -eq 0 ]]; then
        log_pass "All tiering x legal-hold interaction tests passed"
        exit 0
    else
        log_fail "$FAILED test(s) failed"
        echo "Debug: ${TEST_DIR}/daemon.log"
        exit 1
    fi
}

main
