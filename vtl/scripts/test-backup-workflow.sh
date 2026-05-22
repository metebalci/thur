#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL End-to-End Backup Workflow Test
#
# Exercises the real-world backup workflow that turns Thur VTL into a
# usable tape target: write -> swap cartridge -> write to second tape ->
# swap back -> read first tape -> verify contents byte-for-byte.
#
# This catches gaps that synthetic SCSI tests miss:
#   - Variable-block tape WRITE/READ via /dev/stN (tar's wire format)
#   - Filemarks at end of tar streams
#   - mt rewind / mtx load / mtx unload sequencing
#   - Per-cartridge data persistence across load/unload cycles
#
# Prerequisites:
#   - mtx package        (sudo apt-get install mtx)
#   - mt-st              (sudo apt-get install mt-st)
#   - open-iscsi         (sudo apt-get install open-iscsi)
#   - tar                (always present)
#   - lsscsi             (sudo apt-get install lsscsi)
#   - Root/sudo access (required for iSCSI + /dev/stN)
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-backup-workflow.sh [OPTIONS]
#
# The script self-elevates via sudo (NOPASSWD sudoers entry required); no
# need to prefix with sudo yourself.
#
# Options:
#   --release             Use ./target/release/ binaries (default is ./target/debug/)
#   --daemon-path PATH    Path to thurvtld binary (overrides default)
#   --cli-path PATH       Path to thurvtl binary (overrides default)
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect iSCSI session after tests
#

# Self-elevate via sudo so the user can invoke without a `sudo` prefix.
# Requires a NOPASSWD sudoers entry for this script.
if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

# Configuration
BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/test-backup-workflow-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
KEEP_ISCSI=0
DAEMON_PID=""
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
TAPE_DEVICE=""  # /dev/stN (rewind on close)
NOREWIND_DEVICE=""  # /dev/nstN

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        log_info "Disconnecting iSCSI session..."
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
    fi

    if [[ -n "$DAEMON_PID" ]]; then
        log_info "Stopping daemon (PID: $DAEMON_PID)"
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi

    if [[ $KEEP_DATA -eq 0 ]]; then
        log_info "Removing test directory: $TEST_DIR"
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi

    exit $rc
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=()
    local hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    # Resolve thurvtl binaries: explicit --daemon-path/--cli-path > target/$BUILD_PROFILE > $PATH
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvtld >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvtld)
        else
            missing+=("thurvtld")
            hints+=("  - thurvtld: $build_cmd (or pass --daemon-path PATH)")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvtl >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvtl)
        else
            missing+=("thurvtl")
            hints+=("  - thurvtl: $build_cmd (or pass --cli-path PATH)")
        fi
    fi

    # External tools with install hints
    declare -A HINTS=(
        [mtx]="sudo apt-get install mtx"
        [mt]="sudo apt-get install mt-st"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [tar]="(install via your package manager — should be present by default)"
        [lsscsi]="sudo apt-get install lsscsi"
        [curl]="sudo apt-get install curl"
    )
    for tool in mtx mt iscsiadm tar lsscsi curl; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        echo "Install hints:"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running."
            echo "Start it with:"
            echo "  sudo systemctl enable --now iscsid open-iscsi"
            exit 1
        fi
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration..."
    # `library init` refuses to create data_dir itself (operator
    # responsibility on a packaged install — chowned to the daemon
    # user). Pre-create here so the daemon-down init succeeds.
    mkdir -p "$TEST_DIR/data"
    # We're running as root post-self-elevation. The CLI's privdrop
    # will setuid to $SUDO_USER (see init_library), so the data_dir
    # has to be writable by that user — otherwise the audit-log
    # writer hits EACCES. Match ownership to the privdrop target.
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"

# Test data_dir is under /tmp, which is commonly a small tmpfs on dev
# boxes — the production-default 5 GB free-floor would block every
# chunk-seal that calls try_reserve. Disable the floor for tests.
disk_cache:
  disk_free_min_gb: 0

cloud:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

keystore:
  backends:
    local: { type: local }

EOFCONFIG}

# `library init` is daemon-down: must run BEFORE start_daemon.
# `--user "${SUDO_USER:-root}"`: the script self-elevates via sudo,
# so euid=0 here and the CLI's privdrop tries to setuid to the daemon
# service user (default `thurvtl`). On a dev box that user typically
# isn't provisioned; pass the invoking user (or root if invoked
# directly as root) so privdrop has a real target.
init_library() {
    log_info "Initializing library (10 slots, 2 drives, LTO-8)..."
    if ! "$CLI_PATH" --config "$TEST_CONFIG" --user "${SUDO_USER:-root}" \
            library init --slots 10 --drives 2 --lto-generation 8 >/dev/null; then
        log_error "library init failed"
        exit 1
    fi
}

# `cartridge create` is daemon-routed (admin socket): must run AFTER
# start_daemon so THURVTL_ADMIN_SOCKET is in scope.
#
# TAPE01L8: plaintext (legacy path).
# TAPE02L8: appliance-side at-rest encrypted against the `local`
#           keystore. Exercises the seal-time encrypt seam + the
#           read-time decrypt seam through the real iSCSI + /dev/stN
#           write/read path below, plus the boot-time DEK pre-unwrap
#           on every daemon restart.
create_cartridges() {
    log_info "Creating cartridges TAPE01L8 (plaintext) / TAPE02L8 (at-rest local)..."
    if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create TAPE01L8 --lto-generation 8 >/dev/null; then
        log_error "cartridge create TAPE01L8 failed"
        exit 1
    fi
    if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create TAPE02L8 \
        --lto-generation 8 --encrypt --keystore local >/dev/null; then
        log_error "cartridge create TAPE02L8 (--encrypt --keystore local) failed"
        exit 1
    fi
}

start_daemon() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting daemon..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null && { log_info "Daemon ready"; return 0; }
        sleep 1
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

connect_iscsi() {
    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    sleep 3  # let kernel settle and create /dev/stN nodes

    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; exit 1; }
    log_info "Changer device: $CHANGER_DEVICE"

    # Tape device — pick the tape that came from our iSCSI session
    TAPE_DEVICE=$(lsscsi | awk '/tape/{print $NF}' | head -1)
    [[ -n "$TAPE_DEVICE" ]] || { log_error "Tape device not found"; lsscsi; exit 1; }
    NOREWIND_DEVICE=$(echo "$TAPE_DEVICE" | sed 's|/dev/st|/dev/nst|')
    log_info "Tape device: $TAPE_DEVICE (no-rewind: $NOREWIND_DEVICE)"

    # Warm up: clear any pending Unit Attention from iSCSI login
    # (mtx load can fail with I/O error if the first SCSI op hits POWER-ON UA)
    log_info "Warming up SCSI path with mtx status..."
    if ! mtx -f "$CHANGER_DEVICE" status >"${TEST_DIR}/mtx-initial-status.txt" 2>&1; then
        log_warn "Initial mtx status failed (continuing anyway):"
        cat "${TEST_DIR}/mtx-initial-status.txt"
    fi
    if ! mt -f "$NOREWIND_DEVICE" status >"${TEST_DIR}/mt-initial-status.txt" 2>&1; then
        log_warn "Initial mt status failed (continuing anyway):"
        cat "${TEST_DIR}/mt-initial-status.txt"
    fi
}

dump_diagnostics() {
    echo ""
    echo "----- Diagnostics -----"
    echo "Changer status:"
    mtx -f "$CHANGER_DEVICE" status 2>&1 | sed 's/^/  /' | head -40
    echo ""
    echo "Tape status:"
    mt -f "$NOREWIND_DEVICE" status 2>&1 | sed 's/^/  /'
    echo ""
    echo "Last 30 lines of daemon log:"
    tail -30 "${TEST_DIR}/daemon.log" 2>/dev/null | sed 's/^/  /'
    echo "-----------------------"
    echo ""
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
        log_pass "$name"
        PASSED=$((PASSED + 1))
    else
        log_fail "$name"
        FAILED=$((FAILED + 1))
        dump_diagnostics
    fi
    echo ""
}

# Generate a fixture directory full of varied content
make_fixture() {
    local dir="$1"
    local seed="$2"
    mkdir -p "$dir"
    echo "fixture seed=$seed" > "$dir/marker.txt"
    head -c 1048576 /dev/urandom > "$dir/random-1mb.bin"
    seq 1 10000 > "$dir/sequential.txt"
    mkdir -p "$dir/nested/deep"
    echo "deep file $seed" > "$dir/nested/deep/info.txt"
}

test_load_first_tape() {
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || return 1
    mtx -f "$CHANGER_DEVICE" status 2>&1 | grep -q "Data Transfer Element 0:Full" || return 1
}

test_write_first_tape() {
    local fixture="$TEST_DIR/fixture1"
    make_fixture "$fixture" "tape1"
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$fixture" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" weof 2>/dev/null || true  # tar already writes EOF marks
    mt -f "$NOREWIND_DEVICE" rewind || return 1
}

test_unload_first_tape() {
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || return 1
    mtx -f "$CHANGER_DEVICE" status 2>&1 | grep -q "Data Transfer Element 0:Empty" || return 1
}

test_load_second_tape() {
    mtx -f "$CHANGER_DEVICE" load 2 0 >/dev/null 2>&1 || return 1
}

test_write_second_tape() {
    local fixture="$TEST_DIR/fixture2"
    make_fixture "$fixture" "tape2-different"
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$fixture" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
}

test_swap_back_to_first_tape() {
    mtx -f "$CHANGER_DEVICE" unload 2 0 >/dev/null 2>&1 || return 1
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || return 1
}

test_read_first_tape_lists_match() {
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local listing="$TEST_DIR/listing1.txt"
    tar -tf "$NOREWIND_DEVICE" 2>/dev/null | sort > "$listing"
    # Compare against fixture contents
    local expected="$TEST_DIR/expected1.txt"
    (cd "$TEST_DIR/fixture1" && find . -type f | sort) > "$expected"
    # tar prepends "./"; both sides will have it
    diff -u "$expected" <(grep -v '/$' "$listing")
}

test_extract_first_tape_matches_byte_for_byte() {
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local out="$TEST_DIR/extracted1"
    mkdir -p "$out"
    tar -C "$out" -xf "$NOREWIND_DEVICE" || return 1
    diff -r "$TEST_DIR/fixture1" "$out"
}

# At-rest encryption correctness: TAPE01 (plaintext) and TAPE02
# (--keystore local) have both been written with `tar -c` of a
# fixture containing a known marker. Walk the chunk pool and grep
# the raw bytes for the marker. The plaintext cartridge's chunks
# must contain it; the encrypted cartridge's chunks must NOT.
#
# Pool layout: <data_dir>/chunks/<backend>/<aa>/<bb>/<hash>.dat
# (global-dedup), or .../<backend>/<barcode>/... (local-dedup).
# Both cartridges here use the default `global` scope on the
# `local` backend.
#
# We must run this BEFORE the second tape is unloaded — flush_and_seal
# at unload ships the trailing chunk into the pool. The fixture
# writes above wrote enough bytes (1 MiB random + sequential.txt
# + marker.txt) to force at least one chunk-roll mid-write, so the
# marker is already in a sealed chunk by the time write_second_tape
# returns. The unloaded-flush still happens by the time we get here
# because `test_swap_back_to_first_tape` ran the unload between the
# write and us.
test_pool_chunks_for_plaintext_carry_marker() {
    local pool_root="${TEST_DIR}/data/chunks/local"
    if [[ ! -d "$pool_root" ]]; then
        log_error "chunk pool root missing at $pool_root"
        return 1
    fi
    # Tape 1 marker: literal `tape1` (set in make_fixture). One of
    # tape 1's pool chunks must contain it. We grep the binary union
    # of every .dat file under the pool.
    if ! find "$pool_root" -type f -name '*.dat' -print0 \
            | xargs -0 grep -l --binary-files=text 'fixture seed=tape1' >/dev/null 2>&1; then
        log_error "marker 'fixture seed=tape1' NOT found in plaintext-cartridge pool chunks"
        log_error "  → either pool layout changed, or the plaintext path silently encrypted"
        return 1
    fi
    return 0
}

test_pool_chunks_for_encrypted_cartridge_are_ciphertext() {
    local pool_root="${TEST_DIR}/data/chunks/local"
    if [[ ! -d "$pool_root" ]]; then
        log_error "chunk pool root missing at $pool_root"
        return 1
    fi
    # Tape 2 marker: literal `tape2-different`. The plaintext fixture
    # contained it; if the seal-time encrypt seam works, the on-disk
    # ciphertext must NOT contain it. Same grep, opposite expectation.
    if find "$pool_root" -type f -name '*.dat' -print0 \
            | xargs -0 grep -l --binary-files=text 'fixture seed=tape2-different' >/dev/null 2>&1; then
        log_error "marker 'fixture seed=tape2-different' FOUND in encrypted-cartridge pool chunks"
        log_error "  → seal-time encrypt seam regressed; on-disk chunks are plaintext"
        return 1
    fi
    # Sanity belt: the encrypted cartridge's chunks must still
    # exist (we only know the marker is absent — that could also mean
    # zero chunks were written). Walk tape 2's chunk_index and confirm
    # at least one sealed chunk exists.
    local chunks_idx="${TEST_DIR}/data/tapes/TAPE02L8/chunks.idx"
    if [[ ! -s "$chunks_idx" ]]; then
        log_error "TAPE02L8 chunks.idx missing or empty — write apparently never sealed"
        return 1
    fi
    return 0
}

test_second_tape_data_independent() {
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || return 1
    mtx -f "$CHANGER_DEVICE" load 2 0 >/dev/null 2>&1 || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local out="$TEST_DIR/extracted2"
    mkdir -p "$out"
    tar -C "$out" -xf "$NOREWIND_DEVICE" || return 1
    # Tape 2's marker.txt must contain "tape2-different" not "tape1"
    grep -q "tape2-different" "$out/marker.txt" || return 1
    # And it must NOT match tape 1's fixture
    if diff -r "$TEST_DIR/fixture1" "$out" >/dev/null 2>&1; then
        log_error "Tape 2 contents identical to tape 1 — load/unload not isolating cartridges"
        return 1
    fi
    return 0
}

main() {
    echo "========================================"
    echo "Thur VTL End-to-End Backup Workflow Test"
    echo "========================================"
    echo ""
    echo "Workflow: tar -> tape, swap cartridges, swap back, verify byte-for-byte"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    init_library               # daemon-down: library init writes library.json/inventory.json
    start_daemon               # exports THURVTL_ADMIN_SOCKET; required before any cartridge op
    create_cartridges          # daemon-routed: cartridge create auto-places in first free slot
    connect_iscsi

    echo ""
    echo "Running backup-workflow tests..."
    echo "---------------------------------"
    echo ""

    run_test "load tape 1 from slot 1 to drive 0" test_load_first_tape
    run_test "tar archive fixture to tape 1"      test_write_first_tape
    run_test "unload tape 1"                       test_unload_first_tape
    run_test "load tape 2"                         test_load_second_tape
    run_test "tar archive fixture to tape 2"      test_write_second_tape
    run_test "swap back to tape 1"                test_swap_back_to_first_tape
    run_test "plaintext pool chunks carry the marker"  test_pool_chunks_for_plaintext_carry_marker
    run_test "encrypted pool chunks are ciphertext"    test_pool_chunks_for_encrypted_cartridge_are_ciphertext
    run_test "tar -t lists tape 1 contents"       test_read_first_tape_lists_match
    run_test "tar -x tape 1 matches fixture"      test_extract_first_tape_matches_byte_for_byte
    run_test "tape 2 contents are independent"    test_second_tape_data_independent

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total tests: $((PASSED + FAILED))"
    echo "Passed: $PASSED"
    echo "Failed: $FAILED"
    echo ""

    if [[ $FAILED -eq 0 ]]; then
        log_pass "All backup-workflow tests passed"
        exit 0
    else
        log_fail "$FAILED test(s) failed"
        echo ""
        echo "Debug:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data:  ${TEST_DIR}"
        echo "  - Changer:    $CHANGER_DEVICE"
        echo "  - Tape:       $TAPE_DEVICE"
        exit 1
    fi
}

main
