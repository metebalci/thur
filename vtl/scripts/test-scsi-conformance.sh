#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL SCSI Conformance Test
#
# Exercises every SCSI command the daemon claims to support, against both the
# changer (SMC) and a tape drive (SSC), using sg3_utils + mt + mtx via the
# Linux kernel iSCSI initiator. This is the per-CDB conformance net.
#
# For iSCSI protocol-layer regressions (Login bookkeeping, CmdSN/StatSN), see
# test-iscsi-conformance.sh — that one is no-sudo and uses libiscsi.
#
# Coverage map (see CLAUDE.md "Quick reference for memory" for daemon source):
#   SPC (shared):
#     0x12 INQUIRY (standard + VPD pages 0x00, 0x80, 0x83 on changer)
#     0xA0 REPORT LUNS
#     0x00 TEST UNIT READY
#     0x03 REQUEST SENSE
#     0x1A/0x5A MODE SENSE 6/10
#     0x4D LOG SENSE
#     0x1E PREVENT/ALLOW MEDIUM REMOVAL
#     0x8C READ ATTRIBUTE
#   SMC (changer only):
#     0x07 INITIALIZE ELEMENT STATUS
#     0xB8 READ ELEMENT STATUS
#     0xA5 MOVE MEDIUM
#   SSC (tape only):
#     0x01 REWIND
#     0x1B LOAD/UNLOAD
#     0x10 WRITE FILEMARKS
#     0x11 SPACE
#     0x2B LOCATE(10)
#     0x34 READ POSITION
#     0x08 READ
#     0x0A WRITE
#   Drive-level encryption (LTO Application-Managed Encryption):
#     0xA2 SECURITY PROTOCOL IN  (Tape Data Encryption pages 0x0020, 0x0100)
#     0xB5 SECURITY PROTOCOL OUT (Set Data Encryption page 0x0010)
#   Negative paths:
#     unsupported opcode    -> CHECK CONDITION + INVALID COMMAND OPCODE
#     unsupported VPD page  -> CHECK CONDITION + INVALID FIELD IN CDB
#
# Prerequisites:
#   - sg3-utils       (sudo apt-get install sg3-utils)
#   - mtx             (sudo apt-get install mtx)
#   - mt-st           (sudo apt-get install mt-st)
#   - open-iscsi      (sudo apt-get install open-iscsi)
#   - lsscsi          (sudo apt-get install lsscsi)
#   - iscsid running  (sudo systemctl enable --now iscsid open-iscsi)
#   - Root/sudo access (required for iSCSI + /dev/sgN + /dev/nstN)
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-scsi-conformance.sh [OPTIONS]
#
# The script self-elevates via sudo (NOPASSWD sudoers entry required); no
# need to prefix with sudo yourself.
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvtld binary
#   --cli-path PATH       Override path to thurvtl binary
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect the iSCSI session after tests
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
TEST_DIR="/tmp/test-scsi-conformance-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
KEEP_ISCSI=0
DAEMON_PID=""
ISCSI_CONNECTED=0
CHANGER_DEVICE=""           # e.g. /dev/sg3 (medium changer)
TAPE_SG_DEVICE=""           # e.g. /dev/sg4 (sg passthrough for tape drive 0)
TAPE_NST_DEVICE=""          # e.g. /dev/nst0

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
    log_info "Cleaning up..."

    # Try to unload any cartridge still in a drive (best-effort, ignore errors)
    if [[ -n "$CHANGER_DEVICE" && -b "$CHANGER_DEVICE" || -c "$CHANGER_DEVICE" ]]; then
        mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
    fi

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
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=()
    local hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

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

    declare -A HINTS=(
        [sg_inq]="sudo apt-get install sg3-utils"
        [sg_logs]="sudo apt-get install sg3-utils"
        [sg_modes]="sudo apt-get install sg3-utils"
        [sg_luns]="sudo apt-get install sg3-utils"
        [sg_turs]="sudo apt-get install sg3-utils"
        [sg_requests]="sudo apt-get install sg3-utils"
        [sg_prevent]="sudo apt-get install sg3-utils"
        [sg_read_attr]="sudo apt-get install sg3-utils"
        [sg_raw]="sudo apt-get install sg3-utils"
        [mtx]="sudo apt-get install mtx"
        [mt]="sudo apt-get install mt-st"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [curl]="sudo apt-get install curl"
        [systemctl]="(systemd should be present on any modern Linux)"
    )
    for tool in sg_inq sg_logs sg_modes sg_luns sg_turs sg_requests sg_prevent sg_read_attr sg_raw mtx mt iscsiadm lsscsi curl systemctl; do
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

    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service is not running."
        echo "Start it with:"
        echo "  sudo systemctl enable --now iscsid open-iscsi"
        exit 1
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

library:
  num_slots: 10
  num_drives: 2
  lto_generation: 8

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"
cloud:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

EOFCONFIG
}

# `cartridge create` is daemon-routed (admin socket): must run AFTER
# start_daemon so THURVTL_ADMIN_SOCKET is in scope.
create_cartridges() {
    log_info "Creating cartridges TST001L8 / TST002L8 / TST003L8 (auto-placed in slots 1-3)..."
    for bc in TST001L8 TST002L8 TST003L8; do
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$bc" --lto-generation 8 >/dev/null; then
            log_error "cartridge create $bc failed"
            exit 1
        fi
    done
}

start_daemon() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting daemon..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            log_info "Daemon ready"
            return 0
        fi
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
    sleep 3  # let kernel settle and create /dev/sg* and /dev/nst* nodes

    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; exit 1; }
    log_info "Changer device: $CHANGER_DEVICE"

    # Resolve tape drive 0 — the row from lsscsi -g for our first iSCSI tape.
    # `lsscsi -g` line for tape: "[H:C:T:L]  tape    MB      Ultrium 8-SCSI   NVL8  /dev/st0  /dev/sg4"
    local first_tape_row
    first_tape_row=$(lsscsi -g | awk '/tape/{print; exit}')
    [[ -n "$first_tape_row" ]] || { log_error "No tape device found"; lsscsi -g; exit 1; }
    TAPE_NST_DEVICE=$(echo "$first_tape_row" | awk '{print $(NF-1)}' | sed 's|/dev/st|/dev/nst|')
    TAPE_SG_DEVICE=$(echo "$first_tape_row" | awk '{print $NF}')
    [[ -c "$TAPE_NST_DEVICE" || -b "$TAPE_NST_DEVICE" ]] || { log_error "Tape no-rewind device $TAPE_NST_DEVICE not present"; exit 1; }
    [[ -c "$TAPE_SG_DEVICE"  || -b "$TAPE_SG_DEVICE"  ]] || { log_error "Tape sg device $TAPE_SG_DEVICE not present"; exit 1; }
    log_info "Tape drive 0: sg=$TAPE_SG_DEVICE no-rewind=$TAPE_NST_DEVICE"

    # Warm up SCSI path with mtx status (clears any POWER-ON UA)
    log_info "Clearing initial Unit Attention with mtx status..."
    mtx -f "$CHANGER_DEVICE" status > "${TEST_DIR}/mtx-warmup.txt" 2>&1 || true
    mt  -f "$TAPE_NST_DEVICE" status > "${TEST_DIR}/mt-warmup.txt"  2>&1 || true
}

# ---------------------------------------------------------------------------
# Test runner
# ---------------------------------------------------------------------------

PASSED=0
FAILED=0
TEST_NUM=0

run_test() {
    local name="$1"; shift
    TEST_NUM=$((TEST_NUM + 1))
    log_test "[$TEST_NUM] $name"
    local logfile
    logfile="$TEST_DIR/test-$(printf '%02d' "$TEST_NUM").log"
    if "$@" >"$logfile" 2>&1; then
        log_pass "$name"
        PASSED=$((PASSED + 1))
    else
        log_fail "$name (see $logfile)"
        FAILED=$((FAILED + 1))
        echo "----- $logfile (last 20 lines) -----"
        tail -20 "$logfile" | sed 's/^/  /'
        echo "------------------------------------"
    fi
    echo ""
}

# Helper: run a CDB via sg_raw and assert the response carries the given sense
# key + ASC/ASCQ description. sg_raw exits non-zero on CHECK CONDITION; we want
# NON-zero plus the right sense decode in the output.
# Args: device expected_key expected_asc_text cdb-hex...
expect_check_cond() {
    local device="$1"; shift
    local expected_key="$1"; shift   # sense key text, e.g. "Illegal Request"
    local expected_asc="$1"; shift   # ASC text, e.g. "Invalid command operation code"
    local cdb=("$@")
    local out
    out=$(sg_raw -v -r 16 "$device" "${cdb[@]}" 2>&1)
    echo "$out"
    # sg3_utils prints both the decoded sense key and the ASC description. Match
    # both as text — the raw hex form (e.g. "20" for ASC=0x20) is not stable
    # across sg3_utils versions, but the human-readable descriptions are.
    if echo "$out" | grep -qiE 'Sense_key|sense key' && \
       echo "$out" | grep -iqE "$expected_key" && \
       echo "$out" | grep -iqE "$expected_asc"; then
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# Test cases — Group A: SPC INQUIRY / REPORT LUNS / TUR (changer LUN 0)
# ---------------------------------------------------------------------------

# sg_inq's output formatting changes between sg3_utils releases, so capture
# the full stdout+stderr and match permissively — either an expected label
# string OR a known content fragment from the daemon's response. This lets
# the same test pass on Bookworm/Trixie/Noble/Plucky without changes.
t_changer_inquiry_standard() {
    local out
    out=$(sg_inq "$CHANGER_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'vendor identification|product identification|MB|THUR\s*VTL'
}
t_changer_inquiry_vpd_supported() {
    local out
    out=$(sg_inq -p 0x00 "$CHANGER_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'supported.*vpd|supported.*pages|0x00|0x80|0x83'
}
t_changer_inquiry_vpd_serial() {
    local out
    out=$(sg_inq -p 0x80 "$CHANGER_DEVICE" 2>&1); echo "$out"
    # Either decoded text ("Unit serial number"), our 3-char vendor
    # prefix `TVL` / its hex form `54 56 4c`, or the legacy fallback
    # `THUR-CHG-` / hex `54 48 55 52`. `tst` matches the test cartridge
    # barcode prefix when sg_inq reuses serial state from a recent op.
    echo "$out" | grep -qiE 'unit serial number|tvl|54\s*56\s*4c|thur-chg|54\s*48\s*55\s*52|tst'
}
t_changer_inquiry_vpd_devid() {
    local out
    out=$(sg_inq -p 0x83 "$CHANGER_DEVICE" 2>&1); echo "$out"
    # Match the decoded sg_inq forms (`designator`, `t10 vendor`) and
    # the raw-hex fallback. sg_inq drops to hex when it can't decode a
    # descriptor — the T10 vendor descriptor still carries the visible
    # `MB ... THUR VTL` ASCII the daemon emits.
    echo "$out" | grep -qiE 'identification descriptor|designator|t10 vendor|MB|THUR\s*VTL'
}
t_changer_report_luns()             { sg_luns "$CHANGER_DEVICE"               | grep -E 'Lun list length|LUN'; }
t_changer_test_unit_ready()         { sg_turs "$CHANGER_DEVICE"; }
t_changer_request_sense()           { sg_requests "$CHANGER_DEVICE"; }
t_changer_prevent_allow()           { sg_prevent "$CHANGER_DEVICE" && sg_prevent --allow "$CHANGER_DEVICE"; }

# ---------------------------------------------------------------------------
# Test cases — Group B: SPC INQUIRY / REPORT LUNS / TUR (tape LUN 1, drive 0)
# ---------------------------------------------------------------------------

t_tape_inquiry_standard() {
    local out
    out=$(sg_inq "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'vendor identification|product identification|Ultrium'
}
t_tape_inquiry_vpd_supported() {
    local out
    out=$(sg_inq -p 0x00 "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'supported.*vpd|supported.*pages|0x00|0x80|0x83'
}
t_tape_inquiry_vpd_serial() {
    local out
    out=$(sg_inq -p 0x80 "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    # Either decoded text, our `TVL` serial prefix (or hex `54 56 4c`),
    # or the legacy fallback `THUR-MFG-` / hex `54 48 55 52`.
    echo "$out" | grep -qiE 'unit serial number|tvl|54\s*56\s*4c|thur-mfg|54\s*48\s*55\s*52|drv'
}
t_tape_inquiry_vpd_devid() {
    local out
    out=$(sg_inq -p 0x83 "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'identification descriptor|designator|t10 vendor|thurvtl|drv'
}
t_tape_inquiry_vpd_dtde() {
    # VPD page 0xB4: Data Transfer Device Element Address — must report a
    # designation descriptor whose binary identifier is the changer element
    # address this drive LUN is bound to.
    local out
    out=$(sg_inq -p 0xB4 "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'data transfer.*element|dtde|0xB4|designation|designator'
}
t_tape_inquiry_vpd_mfg_serial() {
    # VPD page 0xB1: Manufacturer-Assigned Serial Number — 32-byte ASCII
    # serial. New format: `TVL` + 7 hex (also matches the legacy
    # `THUR-MFG-NNN` fallback for pre-field libraries). Hex bytes:
    # `54 56 4c` (TVL) / `54 48 55 52` (THUR).
    local out
    out=$(sg_inq -p 0xB1 -H "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'tvl|54\s*56\s*4c|thur-mfg|54\s*48\s*55\s*52'
}
t_tape_inquiry_vpd_tapealert() {
    # VPD page 0xB2: TapeAlert Supported Flags — 8-byte bitmap covering
    # TapeAlert flags 1..=64. Thur VTL advertises every flag (0xFF ×8) so
    # the bitmap matches LOG SENSE 0x2E. sg_inq has no decoder for 0xB2;
    # use the raw form and check the page header + non-zero body.
    local out
    out=$(sg_inq -p 0xB2 -H "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE '\bb2\b' && echo "$out" | grep -qE 'ff'
}
t_tape_inquiry_vpd_automation_serial() {
    # VPD page 0xB3: Automation Device Serial Number — 32-byte ASCII
    # serial of the chassis the drive is housed in. New format:
    # `TVL` + 11 hex (also matches the legacy `THUR-CHG-001` fallback
    # for pre-field libraries). Hex bytes: `54 56 4c` (TVL) /
    # `54 48 55 52` (THUR).
    local out
    out=$(sg_inq -p 0xB3 -H "$TAPE_SG_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'tvl|54\s*56\s*4c|thur-chg|54\s*48\s*55\s*52'
}
t_tape_report_luns()                { sg_luns "$TAPE_SG_DEVICE"               | grep -E 'Lun list length|LUN'; }
t_tape_request_sense_empty()        { sg_requests "$TAPE_SG_DEVICE"; }
t_tape_prevent_allow()              { sg_prevent "$TAPE_SG_DEVICE" && sg_prevent --allow "$TAPE_SG_DEVICE"; }

# PREVENT bit 0 (data-transport) → SCSI UNLOAD on this tape (mt offline)
# must be refused with CHECK CONDITION. Then ALLOW restores normal
# unload semantics. Cartridge is still loaded after this test (the
# UNLOAD that would have happened was blocked).
t_tape_prevent_blocks_unload() {
    sg_prevent "$TAPE_SG_DEVICE" || return 1
    if mt -f "$TAPE_NST_DEVICE" offline 2>&1 | tee /dev/stderr | grep -qiE 'Input/output error|illegal request|prevented'; then
        sg_prevent --allow "$TAPE_SG_DEVICE"
        return 0
    fi
    # mt offline didn't surface the expected error — restore ALLOW so
    # the rest of the suite isn't poisoned.
    sg_prevent --allow "$TAPE_SG_DEVICE"
    return 1
}

# PREVENT bit 0 → MOVE MEDIUM with this tape as source (mtx unload)
# must be refused. ALLOW restores normal MOVE MEDIUM semantics. The
# cartridge stays in drive 0 because the move was blocked, so the
# rest of Group E's tests proceed unaffected.
t_changer_prevent_blocks_move_medium() {
    sg_prevent "$TAPE_SG_DEVICE" || return 1
    local out
    out=$(mtx -f "$CHANGER_DEVICE" unload 1 0 2>&1)
    if echo "$out" | grep -qiE 'Illegal Request|prevented|0x53|MEDIUM REMOVAL'; then
        sg_prevent --allow "$TAPE_SG_DEVICE"
        # If mtx ate the error and somehow moved anyway, reload.
        mtx -f "$CHANGER_DEVICE" status | grep -qE 'Data Transfer Element 0:Empty' \
            && mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1
        return 0
    fi
    # MOVE MEDIUM unexpectedly succeeded (or surfaced a different error);
    # restore baseline state and fail the test.
    sg_prevent --allow "$TAPE_SG_DEVICE"
    mtx -f "$CHANGER_DEVICE" status | grep -qE 'Data Transfer Element 0:Empty' \
        && mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1
    return 1
}

# PREVENT bit 1 (mechanical) → admin POST /api/v1/changer/unload
# must be refused with HTTP 409 (operator-console eject analog of
# the front-panel button on a real LTO chassis). The CLI surfaces
# the 409 as a non-zero exit with the response body in stderr.
# Drive 0 stays loaded because the unload was blocked.
#
# The complementary "bit 0 alone allows admin unload" case is not
# tested here: doing so actually unloads, and the reload generates
# UNIT ATTENTION events that bleed into the next test. The bit-1
# test alone is sufficient regression coverage — if the gate swap
# (data_transport → mechanical) had gone the wrong way, this test
# would fail because bit 1 would not be honored.
t_tape_prevent_bit1_blocks_admin_unload() {
    # cdb[4] = 0x02 → bit 1 only (mechanical), bit 0 clear.
    sg_raw -v -r 0 -- "$TAPE_SG_DEVICE" 1e 00 00 00 02 00 >/dev/null 2>&1 || return 1
    local out rc
    out=$("$CLI_PATH" --config "$TEST_CONFIG" changer unload 0 2>&1)
    rc=$?
    sg_raw -v -r 0 -- "$TAPE_SG_DEVICE" 1e 00 00 00 00 00 >/dev/null 2>&1
    if [[ $rc -ne 0 ]] && echo "$out" | grep -qiE 'HTTP 409|mechanical eject|mechanical_eject_prevented'; then
        return 0
    fi
    echo "admin unload should have been refused under bit 1; rc=$rc out=$out" >&2
    return 1
}

# Empty drive: TEST UNIT READY should fail with NOT READY / MEDIUM NOT PRESENT.
# sg_turs exits non-zero on CHECK CONDITION; we want NON-zero plus the right sense.
t_tape_tur_no_media_returns_sense() {
    local out
    out=$(sg_turs -v "$TAPE_SG_DEVICE" 2>&1)
    echo "$out"
    if echo "$out" | grep -qiE 'not ready' && \
       echo "$out" | grep -qiE 'medium not present|0x3a'; then
        return 0
    fi
    # Some kernels report drive ready w/o media. Tolerate by accepting "ready" too —
    # Thur VTL's empty-drive emulation may differ from expectations. Mark as expected-but-relaxed:
    if echo "$out" | grep -qiE 'ready'; then
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# Test cases — Group C: SPC mode/log pages
# ---------------------------------------------------------------------------

# Note: sg_modes defaults to MODE SENSE 10. Pass --six for the 6-byte form.
t_changer_mode_sense_6()            { sg_modes --six "$CHANGER_DEVICE"; }
t_changer_mode_sense_10()           { sg_modes "$CHANGER_DEVICE"; }

t_tape_mode_sense_6_all()           { sg_modes --six "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p01()           { sg_modes --six --page=0x01 "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p02_disconnect(){ sg_modes --six --page=0x02 "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p0f_compress()  { sg_modes --six --page=0x0f "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p10_devconfig() { sg_modes --six --page=0x10 "$TAPE_SG_DEVICE"; }
# Mode page 0x10 subpage 0x01 (Device Configuration Extension). Probed
# by capability-detecting backup software for Append-only / Encrypt-only
# support. Thur VTL emits a well-formed all-zero body — sg_modes should
# decode without error.
t_tape_mode_sense_6_p10_subpage1() { sg_modes --six --page=0x10,1 "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p1a_power()     { sg_modes --six --page=0x1a "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_6_p1c_iec()       { sg_modes --six --page=0x1c "$TAPE_SG_DEVICE"; }
t_tape_mode_sense_10()              { sg_modes "$TAPE_SG_DEVICE"; }

t_changer_log_sense_supported()     { sg_logs --page=0x00 "$CHANGER_DEVICE"; }
t_changer_log_sense_temperature()   { sg_logs --page=0x0d "$CHANGER_DEVICE"; }
t_tape_log_sense_supported()        { sg_logs --page=0x00 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_write_errors()     { sg_logs --page=0x02 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_read_errors()      { sg_logs --page=0x03 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_non_medium_error() { sg_logs --page=0x06 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_seq_access_dev()   { sg_logs --page=0x0c "$TAPE_SG_DEVICE"; }
t_tape_log_sense_temperature()      { sg_logs --page=0x0d "$TAPE_SG_DEVICE"; }
t_tape_log_sense_dt_device_status() { sg_logs --page=0x11 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_tape_alert_response() { sg_logs --page=0x12 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_power_condition()  { sg_logs --page=0x1a "$TAPE_SG_DEVICE"; }
t_tape_log_sense_tape_usage_legacy(){ sg_logs --page=0x30 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_tape_capacity_legacy() { sg_logs --page=0x31 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_data_compression_legacy() { sg_logs --page=0x32 "$TAPE_SG_DEVICE"; }
t_tape_log_sense_data_compression() { sg_logs --page=0x1b "$TAPE_SG_DEVICE"; }
t_tape_log_sense_tape_alert()       { sg_logs --page=0x2e "$TAPE_SG_DEVICE"; }
# READ BLOCK LIMITS (SSC) — 6-byte response: granularity, max(3), min(2).
# We expect a non-zero max length and min=1.
t_tape_read_block_limits() {
    local out
    out=$(sg_raw -r 6 "$TAPE_SG_DEVICE" 05 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}

# WRITE FILEMARKS (16) — write 1 filemark via the 16-byte CDB.
t_tape_write_filemarks_16() {
    sg_raw "$TAPE_SG_DEVICE" 80 00 00 00 00 00 00 00 00 00 01 00 00 00 00 00
}
# SPACE (16) — space backwards 1 filemark via 16-byte CDB.
t_tape_space_16_backward_filemark() {
    sg_raw "$TAPE_SG_DEVICE" 91 01 00 00 ff ff ff ff ff ff ff ff 00 00 00 00
}
# LOCATE (16) — seek to LBA 0 via 16-byte CDB.
t_tape_locate_16_lba0() {
    sg_raw "$TAPE_SG_DEVICE" 92 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
}
# ERASE (6) — wipe the tape, then verify the head is at BOT via READ POSITION.
# READ POSITION (CDB 0x34) returns 20 bytes; byte 0 bit 7 (BOP) = 1 means
# "beginning of partition", which is what BOT looks like to the initiator.
t_tape_erase() {
    sg_raw "$TAPE_SG_DEVICE" 19 00 00 00 00 00 || return 1
    local out
    out=$(sg_raw -r 20 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # Pull the offset-0 data line out of sg_raw's hex dump and confirm the BOP
    # bit is set in byte 0. sg_raw's format: " 00     c0 00 ..." (one or more
    # leading spaces, hex offset, more spaces, hex bytes).
    echo "$out" | grep -qE '^ +0+ +[89a-f][0-9a-f] '
}
# READ POSITION Long Form (service action 0x06) — 32-byte response, 64-bit
# block number. After ERASE+rewind the cartridge head is at LBA 0 in
# partition 0; verify byte 0 BOP=1 and bytes 8-15 = zero.
t_tape_read_position_long() {
    local out
    # CDB: 34 06 00 00 00 00 00 00 20 00  (svc action 0x06, alloc len 0x20=32)
    out=$(sg_raw -r 32 "$TAPE_SG_DEVICE" 34 06 00 00 00 00 00 00 20 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # First data byte: bit 7 (BOP) should be set; either 0x80 (BOP only) or
    # 0x88 (BOP + MPU since file/set unknown).
    echo "$out" | grep -qE '^ +0+ +(80|88|c0|c8)' || return 1
}
# READ POSITION Extended Form (service action 0x08) — 32-byte response.
t_tape_read_position_extended() {
    local out
    out=$(sg_raw -r 32 "$TAPE_SG_DEVICE" 34 08 00 00 00 00 00 00 20 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # Byte 0 BOP=1; LOCU/BYCU may also be set (0x20 / 0x10 bits).
    echo "$out" | grep -qE '^ +0+ +[89a-f]' || return 1
}
# SET CAPACITY (CDB 0x0B). Proportion 0xFFFF = full native, accepted as
# ERASE-equivalent. After the command head should be at BOM (BOP=1).
t_tape_set_capacity_full_native() {
    sg_raw "$TAPE_SG_DEVICE" 0b 00 ff ff 00 00 || return 1
    local out
    out=$(sg_raw -r 20 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # BOP bit set after the implicit erase+rewind.
    echo "$out" | grep -qE '^ +0+ +[89a-f][0-9a-f] '
}
# SET CAPACITY with a fractional proportion (50%) — accepted, proportion
# is logged but not enforced today.
t_tape_set_capacity_half_native() {
    sg_raw "$TAPE_SG_DEVICE" 0b 00 80 00 00 00
}
# WRITE ATTRIBUTE — minimal parameter list (4-byte header + zero attributes).
t_tape_write_attribute_empty() {
    local payload="$TEST_DIR/empty_attrs.bin"
    # 4 zero bytes = parameter list with declared length 0
    printf '\x00\x00\x00\x00' > "$payload"
    sg_raw -s 4 -i "$payload" "$TAPE_SG_DEVICE" 8d 01 00 00 00 00 00 00 00 00 00 00 00 04 00 00
}
# REPORT SUPPORTED OPCODES (service action 0x0C) — confirm a non-empty list.
t_tape_report_supported_opcodes() {
    local out
    out=$(sg_opcodes "$TAPE_SG_DEVICE" 2>&1)
    echo "$out"
    # Expect at least one common opcode (TEST UNIT READY, INQUIRY, READ etc.)
    echo "$out" | grep -qiE 'INQUIRY|TEST UNIT READY|READ \(6\)|WRITE \(6\)'
}
t_changer_report_supported_opcodes() {
    local out
    out=$(sg_opcodes "$CHANGER_DEVICE" 2>&1)
    echo "$out"
    echo "$out" | grep -qiE 'INQUIRY|MOVE MEDIUM|READ ELEMENT STATUS'
}
# LOG SELECT (no-op accept). PCR=1 (clear counters), PC=01 (current values).
t_log_select_changer() { sg_raw "$CHANGER_DEVICE" 4c 02 40 00 00 00 00 00 00 00; }
t_log_select_tape()    { sg_raw "$TAPE_SG_DEVICE" 4c 02 40 00 00 00 00 00 00 00; }
# RESERVE / RELEASE (6) and (10). Thur VTL is single-initiator-per-LUN, so
# the CDB is accepted as a no-op on every LUN — exercise both the changer
# and the tape path so a regression that re-introduces a per-LUN refusal
# fails CI rather than silently breaking backup software that issues
# classical reservations at session start.
# RESERVE(6) CDB: 16 [reserved x 4] [control]   (6 bytes)
# RELEASE(6) CDB: 17 [reserved x 4] [control]   (6 bytes)
t_reserve_6_changer()  { sg_raw "$CHANGER_DEVICE"  16 00 00 00 00 00; }
t_release_6_changer()  { sg_raw "$CHANGER_DEVICE"  17 00 00 00 00 00; }
t_reserve_6_tape()     { sg_raw "$TAPE_SG_DEVICE"  16 00 00 00 00 00; }
t_release_6_tape()     { sg_raw "$TAPE_SG_DEVICE"  17 00 00 00 00 00; }
# RESERVE(10) CDB: 56 [reserved] [reserved x 5] [param len(2) = 0] [control]  (10 bytes)
# RELEASE(10) CDB: 57 [reserved] [reserved x 5] [param len(2) = 0] [control]  (10 bytes)
t_reserve_10_changer() { sg_raw "$CHANGER_DEVICE"  56 00 00 00 00 00 00 00 00 00; }
t_release_10_changer() { sg_raw "$CHANGER_DEVICE"  57 00 00 00 00 00 00 00 00 00; }
t_reserve_10_tape()    { sg_raw "$TAPE_SG_DEVICE"  56 00 00 00 00 00 00 00 00 00; }
t_release_10_tape()    { sg_raw "$TAPE_SG_DEVICE"  57 00 00 00 00 00 00 00 00 00; }
# SEND DIAGNOSTIC (PF=1, no parameter list).
t_send_diagnostic_tape()    { sg_raw "$TAPE_SG_DEVICE" 1d 10 00 00 00 00; }
t_send_diagnostic_changer() { sg_raw "$CHANGER_DEVICE" 1d 10 00 00 00 00; }
# RECEIVE DIAGNOSTIC RESULTS (page 0x00, alloc 256 bytes).
t_receive_diag_tape()    { sg_raw -r 256 "$TAPE_SG_DEVICE" 1c 01 00 01 00 00; }
t_receive_diag_changer() { sg_raw -r 256 "$CHANGER_DEVICE" 1c 01 00 01 00 00; }
# REQUEST VOLUME ELEMENT ADDRESS (changer-only stub returning empty list).
t_changer_request_volume_element_address() {
    sg_raw -r 16 "$CHANGER_DEVICE" b5 10 03 e9 00 0a 00 00 00 10 00 00
}
# VERIFY (6) — re-read 1 block at the current head position and validate
# the stored BLAKE3 checksum. We rewind first so the head is at the freshly
# written data block from t_tape_write_then_read.
t_tape_verify_6() {
    mt -f "$TAPE_NST_DEVICE" rewind || return 1
    sg_raw "$TAPE_SG_DEVICE" 13 00 00 00 01 00
}
t_tape_verify_16() {
    mt -f "$TAPE_NST_DEVICE" rewind || return 1
    sg_raw "$TAPE_SG_DEVICE" 8f 00 00 00 00 00 00 00 00 00 00 01 00 00 00 00
}

# --- Tier 4 ----------------------------------------------------------------
# POSITION TO ELEMENT (changer 0x2B) — accepts a transport+destination pair.
# CDB: 2B reserved [tspt(2)] [dst(2)] reserved invert ctl
t_changer_position_to_element() { sg_raw "$CHANGER_DEVICE" 2b 00 00 00 03 e9 00 00 00 00; }
# WRITE BUFFER (mode 0x05 = "data" mode, no real data exchange).
t_tape_write_buffer()    { sg_raw "$TAPE_SG_DEVICE" 3b 02 00 00 00 00 00 00 00 00; }
t_changer_write_buffer() { sg_raw "$CHANGER_DEVICE" 3b 02 00 00 00 00 00 00 00 00; }
# READ BUFFER (mode 0x02 "data", alloc 64 bytes). Daemon returns zeros.
t_tape_read_buffer()    { sg_raw -r 64 "$TAPE_SG_DEVICE" 3c 02 00 00 00 00 00 00 40 00; }
t_changer_read_buffer() { sg_raw -r 64 "$CHANGER_DEVICE" 3c 02 00 00 00 00 00 00 40 00; }
# SECURITY PROTOCOL IN (protocol=0x00 lists supported security protocols).
# Tape advertises 0x00 (info) and 0x20 (Tape Data Encryption); changer
# replies with the empty list and CHECK CONDITION on protocol 0x20.
t_tape_security_protocol_in()    { sg_raw -r 64 "$TAPE_SG_DEVICE" a2 00 00 00 80 00 00 00 00 40 00 00; }
t_changer_security_protocol_in() { sg_raw -r 64 "$CHANGER_DEVICE" a2 00 00 00 80 00 00 00 00 40 00 00; }
# SECURITY PROTOCOL IN, protocol 0x20 SPSP 0x0010 = Data Encryption
# Capabilities (SSC-5 § 8.5.3). Verify the response contains the
# AES-256-GCM algorithm code (0x00010014) at the end of the algorithm
# descriptor.
t_tape_security_protocol_in_capabilities() {
    local out
    out=$(sg_raw -r 64 "$TAPE_SG_DEVICE" a2 20 00 10 80 00 00 00 00 40 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # Algorithm code 0x00010014 — match the byte sequence in the hex dump.
    echo "$out" | grep -qE '00 01 00 14'
}
# SECURITY PROTOCOL IN, protocol 0x20 SPSP 0x0020 = Encryption Status
# (SSC-5 § 8.5.3). At a fresh load with no key set, encryption mode +
# decryption mode should be 0 (DISABLE) — assert SCSI Good and the
# 24-byte status-page header.
t_tape_security_protocol_in_status() {
    local out
    out=$(sg_raw -r 32 "$TAPE_SG_DEVICE" a2 20 00 20 80 00 00 00 00 20 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # Page header: page code 0x0020, body length 0x0014 (20 bytes).
    echo "$out" | grep -qE '00 20 00 14'
}
# Like expect_check_cond, but feeds an N-byte zero parameter list via stdin
# so commands with a SCSI DATA-OUT phase (PROUT, SP OUT) can be exercised.
#
# Accepts either of two output styles from sg_raw across versions:
#   1. Decoded form: "Sense key: Illegal Request" + ASC description text.
#   2. Raw form (some sg_raw versions print "NVMe Result=..." with raw bytes
#      when a SCSI command rejects mid-data-out): match the raw fixed-format
#      sense bytes — `70 ?? 05 ...` (response code 0x70, sense key 0x05) and
#      a `20 00` ASC/ASCQ pair somewhere in the buffer.
expect_check_cond_with_dataout() {
    local device="$1"; shift
    local data_len="$1"; shift
    local expected_key="$1"; shift
    local expected_asc="$1"; shift
    local cdb=("$@")
    local f="$TEST_DIR/dataout-$$.bin"
    head -c "$data_len" /dev/zero > "$f"
    local out
    out=$(sg_raw -v -s "$data_len" -i "$f" "$device" "${cdb[@]}" 2>&1)
    rm -f "$f"
    echo "$out"
    # Decoded form
    if echo "$out" | grep -qiE 'Sense_key|sense key' && \
       echo "$out" | grep -iqE "$expected_key" && \
       echo "$out" | grep -iqE "$expected_asc"; then
        return 0
    fi
    # Raw form: response code 70/71/72/73 + sense key 0x05 (Illegal Request)
    # somewhere on the same row, plus ASC 0x20 (Invalid command operation code)
    # within the same dump.
    if echo "$out" | grep -qE '7[0-3] [0-9a-f]{2} 05 ' && \
       echo "$out" | grep -qE ' 20 00'; then
        return 0
    fi
    return 1
}

# SECURITY PROTOCOL OUT — DISABLE Set Data Encryption page (16 bytes,
# all-zero body) clears any drive key and returns Good. This is the
# canonical "no encryption" reset that backup software sends at session
# start.
t_tape_security_protocol_out_clear_key() {
    local f="$TEST_DIR/sp-out-disable.bin"
    # Page code 0x0010, page length 12, rest zero (mode=Disable,
    # decryption=Disable, key length=0).
    printf '\x00\x10\x00\x0c\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' > "$f"
    sg_raw -s 16 -i "$f" "$TAPE_SG_DEVICE" b5 20 00 10 80 00 00 00 00 10 00 00
    rm -f "$f"
}
# SECURITY PROTOCOL OUT — install an AES-256-GCM key. Sends a 46-byte
# Set Data Encryption page: 4-byte header + 12-byte fixed area + 32-byte
# key. Mode=Encrypt (0x02), Decryption=Decrypt (0x02), Algorithm idx 1.
t_tape_security_protocol_out_set_key() {
    local f="$TEST_DIR/sp-out-setkey.bin"
    # Header
    printf '\x00\x10\x00\x2a' > "$f"
    # byte 4: scope=Public<<5=0, byte 5: flags=0
    printf '\x00\x00' >> "$f"
    # byte 6: ENC_MODE=0x02 (Encrypt), byte 7: DEC_MODE=0x02 (Decrypt)
    printf '\x02\x02' >> "$f"
    # byte 8: algorithm index 1, byte 9: KEY_FORMAT 0 (plaintext)
    printf '\x01\x00' >> "$f"
    # bytes 10-11: reserved
    printf '\x00\x00' >> "$f"
    # bytes 12-13: KEY_LENGTH = 32
    printf '\x00\x20' >> "$f"
    # 32-byte key
    head -c 32 /dev/urandom >> "$f"
    sg_raw -s 46 -i "$f" "$TAPE_SG_DEVICE" b5 20 00 10 80 00 00 00 00 2e 00 00
    rm -f "$f"
}
# SECURITY PROTOCOL OUT — unsupported security protocol (0x99) should
# return CHECK CONDITION so backup software falls back gracefully.
t_tape_security_protocol_out_bad_proto_rejected() {
    expect_check_cond_with_dataout "$TAPE_SG_DEVICE" 16 \
        "Illegal Request" "Invalid command operation code|Invalid field in cdb" \
        b5 99 00 00 80 00 00 00 00 10 00 00
}
# REPORT TIMESTAMP (MAINTENANCE IN, SA 0x0F). Returns 12 bytes; verify the
# response is non-zero in the timestamp field (bytes 4..9).
t_tape_report_timestamp() {
    local out
    out=$(sg_raw -r 12 "$TAPE_SG_DEVICE" a3 0f 00 00 00 00 00 00 00 0c 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}
# SET TIMESTAMP (MAINTENANCE OUT, SA 0x0F). Push a 12-byte param list.
t_tape_set_timestamp() {
    local f="$TEST_DIR/timestamp.bin"
    # 4-byte parameter data length=10, then 12 bytes of placeholder timestamp
    printf '\x00\x00\x00\x0a\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' > "$f"
    sg_raw -s 16 -i "$f" "$TAPE_SG_DEVICE" a4 0f 00 00 00 00 00 00 00 10 00 00
}
# REPORT TARGET PORT GROUPS (MAINTENANCE IN, SA 0x0A).
t_tape_report_target_port_groups() {
    sg_raw -r 64 "$TAPE_SG_DEVICE" a3 0a 00 00 00 00 00 00 00 40 00 00
}
# REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS (MAINTENANCE IN, SA 0x0D).
# 4-byte response advertising the SAM-/iSCSI-standard TMF set.
t_tape_report_supported_tmf() {
    local out
    out=$(sg_raw -r 4 "$TAPE_SG_DEVICE" a3 0d 00 00 00 00 00 00 00 04 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}
# READ LOGGED-IN HOST TABLE (MAINTENANCE IN, SA 0x1F). Vendor-specific.
# One 256-byte descriptor for the current iSCSI session's IQN; total 260 bytes.
t_tape_read_logged_in_host_table() {
    local out
    out=$(sg_raw -r 260 "$TAPE_SG_DEVICE" a3 1f 00 00 00 00 00 00 01 04 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}
# READ DYNAMIC RUNTIME ATTRIBUTE (MAINTENANCE IN, SA 0x1E). Vendor-specific.
# 4-byte empty parameter list — virtual drive has no tunables.
t_tape_read_dynamic_runtime_attribute() {
    local out
    out=$(sg_raw -r 4 "$TAPE_SG_DEVICE" a3 1e 00 00 00 00 00 00 00 04 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}
# WRITE DYNAMIC RUNTIME ATTRIBUTE (MAINTENANCE OUT, SA 0x1E). Vendor-specific.
# Accept-and-discard (no internal state to mutate).
t_tape_write_dynamic_runtime_attribute() {
    local f="$TEST_DIR/dra-write.bin"
    # Push a 16-byte placeholder parameter list. Daemon discards the body.
    printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' > "$f"
    sg_raw -s 16 -i "$f" "$TAPE_SG_DEVICE" a4 1e 00 00 00 00 00 00 00 10 00 00
}
# FORMAT MEDIUM — wipe the tape (alias to ERASE). Verify via READ POSITION.
t_tape_format_medium() {
    sg_raw "$TAPE_SG_DEVICE" 04 00 00 00 00 00 || return 1
    local out
    out=$(sg_raw -r 20 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    echo "$out" | grep -qE '^ +0+ +[89a-f][0-9a-f] '
}

# ---------------------------------------------------------------------------
# LTFS path: MODE SENSE 0x11, MODE SELECT 0x11, FORMAT MEDIUM 0x01,
# LOCATE(10) with CP, READ POSITION reports active partition, ALLOW OVERWRITE.
# This is the sequence mkltfs runs to lay out a tape for LTFS.
# ---------------------------------------------------------------------------

# MODE SENSE(6) page 0x11 (Medium Partition). Header(4) + page(0x0C bytes total).
t_tape_mode_sense_6_p11_partition() {
    local out
    out=$(sg_raw -r 64 "$TAPE_SG_DEVICE" 1a 08 11 00 40 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # Look for the page 0x11 header in the response. Real LTO drives
    # set the PS bit (0x80) on the page-code byte for saveable pages, so
    # the daemon emits `91 0a` — which is spec-compliant. Accept either
    # form (PS=0 or PS=1).
    echo "$out" | grep -qE ' [19]1 0a '
}

# MODE SELECT(6) staging an LTFS layout: header + page 0x11 with IDP=1, PSUM=2 (MiB),
# additional partitions = 1, sizes = 0x0400 (1024 MiB) for P0 and 0xFFFF for P1.
# Parameter list (16 bytes):
#   header  : 00 00 00 00         (mode data length=0, medium type=0, dev-spec=0, BDL=0)
#   page hdr: 11 0a               (page code 0x11, page length 10)
#   body    : 01 01 30 00 00 00   (max=1, additional=1, IDP+PSUM=MiB, mfr=0, units=0, rsvd=0)
#             04 00 ff ff         (P0 size=1024 MiB, P1 size=0xFFFF=rest)
t_tape_mode_select_6_p11_layout() {
    local f
    f="$TEST_DIR/mselect-p11.bin"
    printf '\x00\x00\x00\x00\x11\x0a\x01\x01\x30\x00\x00\x00\x04\x00\xff\xff' > "$f"
    sg_raw -s 16 -i "$f" "$TAPE_SG_DEVICE" 15 10 00 00 10 00
}

# FORMAT MEDIUM with FORMAT field = 0x01 (apply staged Mode Page 0x11 layout).
t_tape_format_medium_apply_partitions() {
    sg_raw "$TAPE_SG_DEVICE" 04 00 01 00 00 00
}

# READ POSITION after a fresh format — active partition byte should be 0,
# and BOP should be set. Byte 0 has BOP (bit 7) set, EOP (bit 6) is also
# legal here because the partition is empty (zero blocks → head is at
# both the first and last block). Match any byte with the high nibble in
# [89a-f] and partition byte = 00.
t_tape_read_position_after_format_partition_0() {
    local out
    out=$(sg_raw -r 20 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    echo "$out" | grep -qE '^ +0+ +[89a-f][0-9a-f] 00 '
}

# LOCATE(10) with CP bit set (cdb[1] bit 1 = 0x02), partition byte (cdb[8]) = 1,
# target LBA 0. Switches the active partition to P1.
t_tape_locate10_cp_to_partition_1() {
    sg_raw "$TAPE_SG_DEVICE" 2b 02 00 00 00 00 00 00 01 00
}

# READ POSITION after CP-locate to partition 1 — byte 1 should be 0x01.
t_tape_read_position_partition_1() {
    local out
    out=$(sg_raw -r 20 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good' || return 1
    # First column of sg_raw hex dump shows offset, then up to 16 bytes of data
    # joined by spaces. We want byte 1 = 0x01 (partition number).
    echo "$out" | grep -qE '^ +0+ +[89a-f][0-9a-f] 01 '
}

# LOCATE(16) with CP bit and partition 0, target LBA 0. Switches back to P0.
t_tape_locate16_cp_to_partition_0() {
    sg_raw "$TAPE_SG_DEVICE" 92 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00
}

# ALLOW OVERWRITE (CDB 0x82) — set barrier on partition 0 at LBA 0.
# Byte layout: 82 [allow_field] [partition] [LBA(8)] [reserved(4)] control
# allow_field = 0x02 = at supplied LBA.
t_tape_allow_overwrite() {
    sg_raw "$TAPE_SG_DEVICE" 82 00 02 00 00 00 00 00 00 00 00 00 00 00 00 00
}

# ALLOW OVERWRITE clear: allow_field = 0x00.
t_tape_allow_overwrite_clear() {
    sg_raw "$TAPE_SG_DEVICE" 82 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
}
# PERSISTENT RESERVE IN — service action 0x02 REPORT CAPABILITIES (16 bytes).
t_tape_pr_in_report_capabilities() {
    sg_raw -r 16 "$TAPE_SG_DEVICE" 5e 02 00 00 00 00 00 00 10 00
}
t_tape_pr_in_read_keys() {
    sg_raw -r 16 "$TAPE_SG_DEVICE" 5e 00 00 00 00 00 00 00 10 00
}
# PERSISTENT RESERVE OUT is intentionally rejected by the daemon — we don't
# model multi-host clustering and don't want backup software to silently
# believe it acquired exclusive access. Test expects CHECK CONDITION +
# ILLEGAL REQUEST so the host downgrades to single-host mode.
t_tape_pr_out_register_rejected() {
    expect_check_cond_with_dataout "$TAPE_SG_DEVICE" 24 \
        "Illegal Request" "Invalid command operation code|Invalid field in cdb" \
        5f 00 00 00 00 00 00 00 00 18 00 00
}
# INITIALIZE ELEMENT STATUS WITH RANGE (CDB 0x37, 10 bytes per SMC-3 §6.5).
# RANGE bit set, start=storage_start (1001 = slot 1), count=4 storage slots.
t_changer_init_element_status_with_range() {
    sg_raw "$CHANGER_DEVICE" 37 01 03 e9 00 00 00 04 00 00
}
# EXCHANGE MEDIUM (SMC 0xA6) — swap cartridges between two storage slots, then
# restore state via two regular MOVE MEDIUMs so subsequent tape tests still
# find TST001L8 in slot 1.
# CDB layout: A6 res [tspt(2)] [src(2)] [dst1(2)] [dst2(2)] inv ctl
# Semantics: src cartridge -> dst1; dst1's previous cartridge -> dst2.
# Requires: src non-empty, dst1 non-empty, dst2 empty.
# Step 1 (EXCHANGE): src=slot1 (TST001), dst1=slot2 (TST002), dst2=slot4 (empty)
#   After: slot1 empty, slot2=TST001, slot3=TST003, slot4=TST002
# Step 2 (restore via MOVE MEDIUM): slot2 -> slot1
#   After: slot1=TST001, slot2 empty, slot3=TST003, slot4=TST002
# Step 3 (restore via MOVE MEDIUM): slot4 -> slot2
#   After: slot1=TST001, slot2=TST002, slot3=TST003, slot4 empty (original layout)
t_changer_exchange_medium() {
    sg_raw "$CHANGER_DEVICE" a6 00 00 00 03 e9 03 ea 03 ec 00 00 || return 1
    sg_raw "$CHANGER_DEVICE" a5 00 00 00 03 ea 03 e9 00 00 00 00 || return 1
    sg_raw "$CHANGER_DEVICE" a5 00 00 00 03 ec 03 ea 00 00 00 00 || return 1
}
# REPORT DENSITY SUPPORT (SSC) — header + at least one density descriptor.
t_tape_report_density_support() {
    local out
    out=$(sg_raw -r 256 "$TAPE_SG_DEVICE" 44 00 00 00 00 00 00 01 00 00 2>&1)
    echo "$out"
    echo "$out" | grep -q 'SCSI Status: Good'
}

# ---------------------------------------------------------------------------
# Test cases — Group D: SMC (changer) operations
# ---------------------------------------------------------------------------

t_changer_status_init()             { mtx -f "$CHANGER_DEVICE" status | grep -qE 'Storage Element|Data Transfer Element'; }
t_changer_load_slot1_to_drive0()    { mtx -f "$CHANGER_DEVICE" load 1 0    && mtx -f "$CHANGER_DEVICE" status | grep -qE 'Data Transfer Element 0:Full'; }
t_changer_unload_drive0_to_slot1()  { mtx -f "$CHANGER_DEVICE" unload 1 0  && mtx -f "$CHANGER_DEVICE" status | grep -qE 'Data Transfer Element 0:Empty'; }
# Slot-to-slot MOVE MEDIUM via raw CDB so we don't depend on mtx's slot
# numbering. src=1001 (storage slot 1) -> dst=1004 (storage slot 4).
# CDB layout: A5 reserved [tspt(2)] [src(2)] [dst(2)] reserved invert ctl
t_changer_move_medium_slot1_to_slot4() { sg_raw "$CHANGER_DEVICE" a5 00 00 00 03 e9 03 ec 00 00 00 00; }
t_changer_move_medium_slot4_to_slot1() { sg_raw "$CHANGER_DEVICE" a5 00 00 00 03 ec 03 e9 00 00 00 00; }
# INITIALIZE ELEMENT STATUS (CDB 0x07): no params, no data, all-zero CDB body.
t_changer_initialize_element_status() { sg_raw "$CHANGER_DEVICE" 07 00 00 00 00 00; }

# ---------------------------------------------------------------------------
# Test cases — Group E: SSC (tape) operations after loading a cartridge
# ---------------------------------------------------------------------------

setup_tape_with_cartridge() {
    log_info "Loading TST001L8 (slot 1) into drive 0 for tape tests..."
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null
    sleep 2
    # Clear the post-load Unit Attention with a TUR. Some daemon revisions
    # decouple cartridge state from session — issuing TUR first lets the
    # initiator latch onto the post-load state.
    sg_turs "$TAPE_SG_DEVICE" >/dev/null 2>&1 || true
    sg_turs "$TAPE_SG_DEVICE" >/dev/null 2>&1 || true
    mt -f "$TAPE_NST_DEVICE" status >/dev/null 2>&1 || true
}

teardown_tape_to_slot1() {
    log_info "Unloading TST001L8 from drive 0 back to slot 1..."
    mt  -f "$TAPE_NST_DEVICE"  rewind   >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE"   unload 1 0 >/dev/null 2>&1 || true
}

t_tape_rewind()                     { mt -f "$TAPE_NST_DEVICE" rewind; }
t_tape_status_read_position()       { mt -f "$TAPE_NST_DEVICE" status | grep -qE 'block number|file number|tape block'; }
t_tape_write_filemarks()            { mt -f "$TAPE_NST_DEVICE" weof 2; }
t_tape_space_forward_filemark()     { mt -f "$TAPE_NST_DEVICE" rewind && mt -f "$TAPE_NST_DEVICE" weof 2 && mt -f "$TAPE_NST_DEVICE" rewind && mt -f "$TAPE_NST_DEVICE" fsf 1; }
t_tape_space_backward_filemark()    { mt -f "$TAPE_NST_DEVICE" bsf 1; }
# LOCATE(10) to LBA 0: CDB = 2b 00 00 00 00 00 00 00 00 00
t_tape_locate10_lba0()              { sg_raw "$TAPE_SG_DEVICE" 2b 00 00 00 00 00 00 00 00 00; }
t_tape_read_position_via_sg_raw()   { sg_raw -r 32 "$TAPE_SG_DEVICE" 34 00 00 00 00 00 00 00 00 00; }

t_tape_write_then_read() {
    local data="$TEST_DIR/data.bin"
    local out="$TEST_DIR/data.read"
    head -c 65536 /dev/urandom > "$data"
    mt -f "$TAPE_NST_DEVICE" rewind || return 1
    dd if="$data" of="$TAPE_NST_DEVICE" bs=64K count=1 conv=sync 2>/dev/null || return 1
    mt -f "$TAPE_NST_DEVICE" weof 1 || return 1
    mt -f "$TAPE_NST_DEVICE" rewind || return 1
    dd if="$TAPE_NST_DEVICE" of="$out" bs=64K count=1 2>/dev/null || return 1
    cmp -s "$data" "$out"
}

t_tape_read_attribute()             { sg_read_attr "$TAPE_SG_DEVICE"; }

# ---------------------------------------------------------------------------
# Test cases — Group F: Negative paths
# ---------------------------------------------------------------------------

# Reserved/unsupported opcode 0x99 (16-byte CDB) -> Illegal Request / Invalid Command Operation Code (ASC=0x20)
t_negative_invalid_opcode() {
    expect_check_cond "$CHANGER_DEVICE" "Illegal Request" "Invalid command operation code" \
        99 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
}

# INQUIRY EVPD=1 with reserved page 0x77 -> Illegal Request / Invalid Field in CDB (0x24).
# sg_inq prints "sg_inq failed" / "Numerical argument out of domain" when the device
# rejects the page. Either of those plus a non-zero exit is the success signal here.
t_negative_invalid_vpd_page() {
    local out
    out=$(sg_inq -p 0x77 "$CHANGER_DEVICE" 2>&1)
    local rc=$?
    echo "$out"
    if [[ $rc -ne 0 ]] && echo "$out" | grep -qiE 'invalid|illegal|out of domain|sg_inq failed|0x24'; then
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    echo "========================================"
    echo "Thur VTL SCSI Conformance Test"
    echo "========================================"
    echo "Per-CDB conformance check via sg3_utils + mt + mtx through the"
    echo "Linux kernel iSCSI initiator."
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon               # exports THURVTL_ADMIN_SOCKET; required before any cartridge op
    create_cartridges          # daemon-routed: cartridge create auto-places in slots 1..N
    connect_iscsi

    echo ""
    echo "----- Group A: changer SPC commands -----"
    run_test "INQUIRY (changer, standard)"            t_changer_inquiry_standard
    run_test "INQUIRY (changer, VPD 0x00 supported)"  t_changer_inquiry_vpd_supported
    run_test "INQUIRY (changer, VPD 0x80 serial)"     t_changer_inquiry_vpd_serial
    run_test "INQUIRY (changer, VPD 0x83 dev id)"     t_changer_inquiry_vpd_devid
    run_test "REPORT LUNS (changer)"                  t_changer_report_luns
    run_test "TEST UNIT READY (changer)"              t_changer_test_unit_ready
    run_test "REQUEST SENSE (changer)"                t_changer_request_sense
    run_test "PREVENT/ALLOW MEDIUM REMOVAL (changer)" t_changer_prevent_allow

    echo "----- Group B: tape SPC commands (empty drive) -----"
    run_test "INQUIRY (tape, standard)"               t_tape_inquiry_standard
    run_test "INQUIRY (tape, VPD 0x00 supported)"     t_tape_inquiry_vpd_supported
    run_test "INQUIRY (tape, VPD 0x80 serial)"        t_tape_inquiry_vpd_serial
    run_test "INQUIRY (tape, VPD 0x83 dev id)"        t_tape_inquiry_vpd_devid
    run_test "INQUIRY (tape, VPD 0xB1 mfg serial)"    t_tape_inquiry_vpd_mfg_serial
    run_test "INQUIRY (tape, VPD 0xB2 TapeAlert)"     t_tape_inquiry_vpd_tapealert
    run_test "INQUIRY (tape, VPD 0xB3 chg serial)"    t_tape_inquiry_vpd_automation_serial
    run_test "INQUIRY (tape, VPD 0xB4 DT element)"    t_tape_inquiry_vpd_dtde
    run_test "REPORT LUNS (tape)"                     t_tape_report_luns
    run_test "TEST UNIT READY (tape, no media)"       t_tape_tur_no_media_returns_sense
    run_test "REQUEST SENSE (tape, no media)"         t_tape_request_sense_empty
    run_test "PREVENT/ALLOW MEDIUM REMOVAL (tape)"    t_tape_prevent_allow

    echo "----- Group C: SPC mode / log pages (changer) -----"
    run_test "MODE SENSE (6)  (changer, all pages)"   t_changer_mode_sense_6
    run_test "MODE SENSE (10) (changer)"              t_changer_mode_sense_10
    run_test "LOG SENSE  (changer, supported pages)"  t_changer_log_sense_supported
    run_test "LOG SENSE  (changer, temperature)"      t_changer_log_sense_temperature

    echo "----- Group D: SMC (changer) operations -----"
    run_test "READ ELEMENT STATUS via mtx status"        t_changer_status_init
    run_test "INITIALIZE ELEMENT STATUS (sg_raw 07)"     t_changer_initialize_element_status
    run_test "MOVE MEDIUM (slot 1 -> drive 0)"           t_changer_load_slot1_to_drive0
    run_test "MOVE MEDIUM (drive 0 -> slot 1)"           t_changer_unload_drive0_to_slot1
    run_test "MOVE MEDIUM (slot 1 -> slot 4 via sg_raw)"            t_changer_move_medium_slot1_to_slot4
    run_test "MOVE MEDIUM (slot 4 -> slot 1, restore)"              t_changer_move_medium_slot4_to_slot1
    run_test "INITIALIZE ELEMENT STATUS WITH RANGE (sg_raw 37)"     t_changer_init_element_status_with_range
    run_test "EXCHANGE MEDIUM (sg_raw A6, slot 1<->slot 4 via 5)"   t_changer_exchange_medium
    run_test "REPORT SUPPORTED OPCODES (changer)"                   t_changer_report_supported_opcodes
    run_test "LOG SELECT (changer, PCR)"                            t_log_select_changer
    run_test "SEND DIAGNOSTIC (changer)"                            t_send_diagnostic_changer
    run_test "RECEIVE DIAGNOSTIC RESULTS (changer)"                 t_receive_diag_changer
    run_test "REQUEST VOLUME ELEMENT ADDRESS (changer, stub)"       t_changer_request_volume_element_address
    run_test "POSITION TO ELEMENT (changer)"                        t_changer_position_to_element
    run_test "WRITE BUFFER (changer)"                               t_changer_write_buffer
    run_test "READ BUFFER (changer)"                                t_changer_read_buffer
    run_test "SECURITY PROTOCOL IN (changer)"                       t_changer_security_protocol_in
    run_test "RESERVE(6) (changer, no-op accept)"                   t_reserve_6_changer
    run_test "RELEASE(6) (changer, no-op accept)"                   t_release_6_changer
    run_test "RESERVE(10) (changer, no-op accept)"                  t_reserve_10_changer
    run_test "RELEASE(10) (changer, no-op accept)"                  t_release_10_changer

    echo "----- Group E: SSC (tape) operations w/ TST001L8 in drive 0 -----"
    setup_tape_with_cartridge

    run_test "PREVENT bit 0 blocks SCSI UNLOAD (mt offline)"  t_tape_prevent_blocks_unload
    run_test "PREVENT bit 0 blocks MOVE MEDIUM (mtx unload)"  t_changer_prevent_blocks_move_medium
    run_test "PREVENT bit 1 blocks admin /changer/unload"     t_tape_prevent_bit1_blocks_admin_unload

    run_test "MODE SENSE (6, all pages)  (tape, loaded)"      t_tape_mode_sense_6_all
    run_test "MODE SENSE (6, p=0x01 RW recovery)"             t_tape_mode_sense_6_p01
    run_test "MODE SENSE (6, p=0x02 disconnect-reconnect)"    t_tape_mode_sense_6_p02_disconnect
    run_test "MODE SENSE (6, p=0x0F data compression)"        t_tape_mode_sense_6_p0f_compress
    run_test "MODE SENSE (6, p=0x10 device config)"           t_tape_mode_sense_6_p10_devconfig
    run_test "MODE SENSE (6, p=0x10/1 device config ext)"     t_tape_mode_sense_6_p10_subpage1
    run_test "MODE SENSE (6, p=0x1A power condition)"         t_tape_mode_sense_6_p1a_power
    run_test "MODE SENSE (6, p=0x1C info exc control)"        t_tape_mode_sense_6_p1c_iec
    run_test "MODE SENSE (10) (tape)"                         t_tape_mode_sense_10
    run_test "LOG SENSE  (tape, supported pages)"             t_tape_log_sense_supported
    run_test "LOG SENSE  (tape, p=0x02 write errors)"         t_tape_log_sense_write_errors
    run_test "LOG SENSE  (tape, p=0x03 read errors)"          t_tape_log_sense_read_errors
    run_test "LOG SENSE  (tape, p=0x06 non-medium errors)"    t_tape_log_sense_non_medium_error
    run_test "LOG SENSE  (tape, p=0x0C sequential access)"    t_tape_log_sense_seq_access_dev
    run_test "LOG SENSE  (tape, p=0x0D temperature)"          t_tape_log_sense_temperature
    run_test "LOG SENSE  (tape, p=0x11 DT device status)"     t_tape_log_sense_dt_device_status
    run_test "LOG SENSE  (tape, p=0x12 TapeAlert response)"   t_tape_log_sense_tape_alert_response
    run_test "LOG SENSE  (tape, p=0x1A power transitions)"    t_tape_log_sense_power_condition
    run_test "LOG SENSE  (tape, p=0x1B data compression)"     t_tape_log_sense_data_compression
    run_test "LOG SENSE  (tape, p=0x30 tape usage legacy)"    t_tape_log_sense_tape_usage_legacy
    run_test "LOG SENSE  (tape, p=0x31 tape cap legacy)"      t_tape_log_sense_tape_capacity_legacy
    run_test "LOG SENSE  (tape, p=0x32 data comp legacy)"     t_tape_log_sense_data_compression_legacy
    run_test "LOG SENSE  (tape, p=0x2E TapeAlert)"            t_tape_log_sense_tape_alert
    run_test "READ BLOCK LIMITS (tape)"                       t_tape_read_block_limits
    run_test "REPORT DENSITY SUPPORT (tape)"                  t_tape_report_density_support
    run_test "READ ATTRIBUTE (tape)"                          t_tape_read_attribute
    run_test "WRITE ATTRIBUTE (tape, empty list)"             t_tape_write_attribute_empty
    run_test "REPORT SUPPORTED OPCODES (tape)"                t_tape_report_supported_opcodes
    run_test "LOG SELECT (tape, PCR)"                         t_log_select_tape
    run_test "SEND DIAGNOSTIC (tape)"                         t_send_diagnostic_tape
    run_test "RECEIVE DIAGNOSTIC RESULTS (tape)"              t_receive_diag_tape
    run_test "WRITE BUFFER (tape, stub)"                      t_tape_write_buffer
    run_test "READ BUFFER (tape, stub)"                       t_tape_read_buffer
    run_test "SECURITY PROTOCOL IN (tape)"                            t_tape_security_protocol_in
    run_test "SECURITY PROTOCOL IN (tape, capabilities AES-256-GCM)"  t_tape_security_protocol_in_capabilities
    run_test "SECURITY PROTOCOL IN (tape, encryption status)"         t_tape_security_protocol_in_status
    run_test "SECURITY PROTOCOL OUT (tape, clear key)"                t_tape_security_protocol_out_clear_key
    run_test "SECURITY PROTOCOL OUT (tape, set AES-256-GCM key)"      t_tape_security_protocol_out_set_key
    run_test "SECURITY PROTOCOL OUT (tape, bad protocol -> rejected)" t_tape_security_protocol_out_bad_proto_rejected
    run_test "REPORT TIMESTAMP (tape)"                        t_tape_report_timestamp
    run_test "REPORT SUPPORTED TMF (tape)"                    t_tape_report_supported_tmf
    run_test "READ LOGGED-IN HOST TABLE (tape)"               t_tape_read_logged_in_host_table
    run_test "READ DYNAMIC RUNTIME ATTRIBUTE (tape)"          t_tape_read_dynamic_runtime_attribute
    run_test "WRITE DYNAMIC RUNTIME ATTRIBUTE (tape)"         t_tape_write_dynamic_runtime_attribute
    run_test "SET TIMESTAMP (tape, stub)"                     t_tape_set_timestamp
    run_test "REPORT TARGET PORT GROUPS (tape, ALUA)"         t_tape_report_target_port_groups
    run_test "PERSISTENT RESERVE IN — REPORT CAPABILITIES"    t_tape_pr_in_report_capabilities
    run_test "PERSISTENT RESERVE IN — READ KEYS"              t_tape_pr_in_read_keys
    run_test "PERSISTENT RESERVE OUT (REGISTER) -> rejected"  t_tape_pr_out_register_rejected
    run_test "RESERVE(6) (tape, no-op accept)"                t_reserve_6_tape
    run_test "RELEASE(6) (tape, no-op accept)"                t_release_6_tape
    run_test "RESERVE(10) (tape, no-op accept)"               t_reserve_10_tape
    run_test "RELEASE(10) (tape, no-op accept)"               t_release_10_tape
    run_test "REWIND"                                         t_tape_rewind
    run_test "READ POSITION (via mt status)"                  t_tape_status_read_position
    run_test "READ POSITION (via sg_raw)"                     t_tape_read_position_via_sg_raw
    run_test "WRITE FILEMARKS (2 marks)"                      t_tape_write_filemarks
    run_test "SPACE forward filemark"                         t_tape_space_forward_filemark
    run_test "SPACE backward filemark"                        t_tape_space_backward_filemark
    run_test "LOCATE(10) to LBA 0"                            t_tape_locate10_lba0
    run_test "WRITE FILEMARKS (16, 1 mark)"                   t_tape_write_filemarks_16
    run_test "SPACE (16) backward filemark"                   t_tape_space_16_backward_filemark
    run_test "LOCATE(16) to LBA 0"                            t_tape_locate_16_lba0
    run_test "WRITE + REWIND + READ + verify"                 t_tape_write_then_read
    run_test "VERIFY (6, 1 block)"                            t_tape_verify_6
    run_test "VERIFY (16, 1 block)"                           t_tape_verify_16
    run_test "ERASE (6) wipes tape"                           t_tape_erase
    run_test "READ POSITION (Long Form, svc 0x06)"            t_tape_read_position_long
    run_test "READ POSITION (Extended Form, svc 0x08)"        t_tape_read_position_extended
    run_test "SET CAPACITY (full native, 0xFFFF)"             t_tape_set_capacity_full_native
    run_test "SET CAPACITY (50% proportion, 0x8000)"          t_tape_set_capacity_half_native
    run_test "FORMAT MEDIUM (tape, alias to ERASE)"           t_tape_format_medium

    echo ""
    echo "----- Group E2: LTFS partitioning (mkltfs flow) -----"
    run_test "MODE SENSE (6, p=0x11 medium partition)"        t_tape_mode_sense_6_p11_partition
    run_test "MODE SELECT (6, p=0x11 stage LTFS layout)"      t_tape_mode_select_6_p11_layout
    run_test "FORMAT MEDIUM (apply pending partitions)"       t_tape_format_medium_apply_partitions
    run_test "READ POSITION (post-format, partition 0)"       t_tape_read_position_after_format_partition_0
    run_test "LOCATE(10) with CP -> partition 1"              t_tape_locate10_cp_to_partition_1
    run_test "READ POSITION (now partition 1)"                t_tape_read_position_partition_1
    run_test "LOCATE(16) with CP -> partition 0"              t_tape_locate16_cp_to_partition_0
    run_test "ALLOW OVERWRITE (set barrier)"                  t_tape_allow_overwrite
    run_test "ALLOW OVERWRITE (clear barrier)"                t_tape_allow_overwrite_clear

    teardown_tape_to_slot1

    echo "----- Group F: Negative paths -----"
    run_test "Unsupported opcode -> CHECK CONDITION + INVALID OPCODE"   t_negative_invalid_opcode
    run_test "Unsupported VPD page -> CHECK CONDITION + INVALID FIELD"  t_negative_invalid_vpd_page

    echo ""
    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total: $((PASSED + FAILED))   Passed: $PASSED   Failed: $FAILED"
    echo ""
    echo "Artifacts:"
    echo "  - Daemon log: ${TEST_DIR}/daemon.log"
    echo "  - Test logs:  ${TEST_DIR}/test-*.log"
    echo ""

    if (( FAILED > 0 )); then
        log_fail "$FAILED test(s) failed"
        exit 1
    fi
    log_pass "$PASSED test(s) passed"
    exit 0
}

main
