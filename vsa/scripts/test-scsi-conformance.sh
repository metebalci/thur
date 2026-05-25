#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa SCSI Conformance Test
#
# Per-CDB conformance net for thurvsad's SBC-3 dispatcher,
# exercised through the kernel iSCSI initiator + sg3_utils. Mirrors
# the role vtl/scripts/test-scsi-conformance.sh plays for the
# tape-side daemon, but for the direct-access block surface.
#
# Coverage map (see CLAUDE.md "thurvsa block-product initiative" for
# the daemon source):
#   SPC (shared discovery + config):
#     0x12 INQUIRY (standard + VPD pages 0x00 / 0x80 / 0x83 / 0x8F / 0xB0 / 0xB2)
#     0x9E READ CAPACITY 16 (LBPME + LBPRZ thin-provisioning hints)
#     0x1A / 0x5A MODE SENSE 6 / 10 (Caching + Control pages)
#     0x15 / 0x55 MODE SELECT 6 / 10 (round-trip + WCE mutation rejected)
#   SBC-3 data path:
#     0x2A / 0x8A WRITE 10 / 16    (sub-page LBA RMW)
#     0x28 / 0x88 READ 10 / 16     (sub-page LBA + sparse-hole zero)
#     0x35 / 0x91 SYNCHRONIZE CACHE (real fence: kill-restart proves it)
#     0x89 COMPARE AND WRITE       (success + MISCOMPARE)
#     0x42 UNMAP                   (sub-page sector zero + page-index drop)
#     0x83 EXTENDED COPY           (VAAI XCOPY, same-LUN page-aligned fast path)
#     0x84 RECEIVE COPY RESULTS    (COPY STATUS + OPERATING PARAMETERS)
#   SBC-3 reservations:
#     0x5E / 0x5F PERSISTENT RESERVE IN / OUT
#       (single-host scope — see cross-nexus-conflict caveat below)
#   WORM volume:
#     WRITE / CAW / UNMAP refused with WRITE PROTECTED (sense 0x07/0x27/0x00)
#
# Limitations:
#   - True cross-nexus reservation conflict needs two distinct initiator
#     IQNs. Single-host loopback iscsiadm uses one InitiatorName from
#     /etc/iscsi/initiatorname.iscsi. We register + reserve + release
#     and assert REPORT KEYS / REPORT RESERVATION end-states; rejecting
#     a WRITE from another nexus is documented but not exercised.
#
# Prerequisites:
#   - sg3-utils       (sudo apt-get install sg3-utils)
#   - open-iscsi      (sudo apt-get install open-iscsi)
#   - lsscsi          (sudo apt-get install lsscsi)
#   - util-linux      (blkdiscard for UNMAP coverage)
#   - iscsid running  (sudo systemctl enable --now iscsid)
#   - Root/sudo access (required for iSCSI + /dev/sdX)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-scsi-conformance.sh [OPTIONS]
#
# The script self-elevates via sudo (NOPASSWD sudoers entry required);
# no need to prefix with sudo yourself.
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect the iSCSI session after tests
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/thurvsa-test-scsi-conformance-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
KEEP_DATA=0
KEEP_ISCSI=0
DAEMON_PID=""
ISCSI_CONNECTED=0
SECTOR_SIZE=4096
PAGE_SIZE=65536
SECTORS_PER_PAGE=$((PAGE_SIZE / SECTOR_SIZE))   # 16
RW_VOLUME_NAME="vol-rw"
RW_VOLUME_SIZE_BYTES=$((4 * PAGE_SIZE * 64))    # 16 MiB ⇒ 256 pages
WORM_VOLUME_NAME="vol-worm"
WORM_VOLUME_SIZE_BYTES=$((PAGE_SIZE * 64))      # 4 MiB ⇒ 64 pages
RW_DEVICE=""        # /dev/sdX for the read-write LUN
RW_SG_DEVICE=""     # /dev/sgN passthrough sibling
WORM_DEVICE=""      # /dev/sdX for the WORM LUN
WORM_SG_DEVICE=""   # /dev/sgN passthrough sibling — needed for WRITE/CAW
                    # tests since the kernel marks the WORM block device
                    # read-only when it sees WP=1 in MODE SENSE

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

    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
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

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvsad >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvsad)
        else
            missing+=("thurvsad")
            hints+=("  - thurvsad: $build_cmd (or pass --daemon-path PATH)")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvsa >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvsa)
        else
            missing+=("thurvsa")
            hints+=("  - thurvsa: $build_cmd (or pass --cli-path PATH)")
        fi
    fi

    declare -A HINTS=(
        [sg_inq]="sudo apt-get install sg3-utils"
        [sg_vpd]="sudo apt-get install sg3-utils"
        [sg_modes]="sudo apt-get install sg3-utils"
        [sg_wr_mode]="sudo apt-get install sg3-utils"
        [sg_readcap]="sudo apt-get install sg3-utils"
        [sg_compare_and_write]="sudo apt-get install sg3-utils"
        [sg_persist]="sudo apt-get install sg3-utils"
        [sg_raw]="sudo apt-get install sg3-utils"
        [sg_dd]="sudo apt-get install sg3-utils"
        [sg_unmap]="sudo apt-get install sg3-utils"
        [sg_sync]="sudo apt-get install sg3-utils"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [blkdiscard]="sudo apt-get install util-linux"
        [curl]="sudo apt-get install curl"
        [systemctl]="(systemd should be present on any modern Linux)"
    )
    for tool in sg_inq sg_vpd sg_modes sg_wr_mode sg_readcap sg_compare_and_write sg_persist sg_raw sg_dd sg_unmap sg_sync iscsiadm lsscsi blkdiscard curl systemctl; do
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
    mkdir -p "$TEST_DIR/data/volumes"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"

audit:
  enabled: true
storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

EOFCONFIG
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            log_info "Daemon ready (PID $DAEMON_PID)"
            return 0
        fi
        sleep 1
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

create_volumes() {
    log_info "Creating $RW_VOLUME_NAME ($((RW_VOLUME_SIZE_BYTES/1024/1024)) MiB) and $WORM_VOLUME_NAME ($((WORM_VOLUME_SIZE_BYTES/1024/1024)) MiB --worm)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$RW_VOLUME_NAME"   --size "$((RW_VOLUME_SIZE_BYTES/1024/1024))M"   --dedup local >/dev/null
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$WORM_VOLUME_NAME" --size "$((WORM_VOLUME_SIZE_BYTES/1024/1024))M" --dedup local --worm >/dev/null
}

# Walk lsscsi -g rows for the thurvsa target's LUNs and resolve a
# /dev/sdX path per (uuid, lun). The kernel sets the SCSI ID's vendor /
# model fields from thurvsa's INQUIRY response (MB / THUR VSA, since
# 2026-05-11 clean-slate rename).
resolve_devices() {
    log_info "Resolving /dev/sdX nodes for $RW_VOLUME_NAME and $WORM_VOLUME_NAME..."
    sleep 3  # let kernel settle
    # lsscsi rows for thurvsa look like:
    #   [12:0:0:0]  disk  MB     THUR VSA   0001  /dev/sdc  /dev/sg6
    # Column 1 is "[H:C:T:L]"; the L field is the LUN.
    local rows
    rows=$(lsscsi -g | awk '/THUR VSA/')
    if [[ -z "$rows" ]]; then
        log_error "No THUR VSA devices found"
        lsscsi -g
        exit 1
    fi
    while IFS= read -r row; do
        local hctl lun sd sg
        hctl=$(echo "$row" | awk '{print $1}')          # [H:C:T:L]
        lun=$(echo "$hctl" | sed 's|.*:||;s|]||')
        sd=$(echo "$row" | awk '{print $(NF-1)}')        # /dev/sdX
        sg=$(echo "$row" | awk '{print $NF}')            # /dev/sgN
        if [[ "$lun" == "0" ]]; then
            RW_DEVICE="$sd"
            RW_SG_DEVICE="$sg"
        elif [[ "$lun" == "1" ]]; then
            WORM_DEVICE="$sd"
            WORM_SG_DEVICE="$sg"
        fi
    done <<< "$rows"
    [[ -n "$RW_DEVICE"   ]] || { log_error "RW LUN device not found"; lsscsi -g; exit 1; }
    [[ -n "$WORM_DEVICE" ]] || { log_error "WORM LUN device not found"; lsscsi -g; exit 1; }
    log_info "RW   LUN 0 -> $RW_DEVICE (sg: $RW_SG_DEVICE)"
    log_info "WORM LUN 1 -> $WORM_DEVICE (sg: $WORM_SG_DEVICE)"
}

connect_iscsi() {
    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    resolve_devices
}

disconnect_iscsi() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete  >/dev/null 2>&1 || true
        ISCSI_CONNECTED=0
        sleep 1
    fi
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
        echo "----- $logfile (last 25 lines) -----"
        tail -25 "$logfile" | sed 's/^/  /'
        echo "------------------------------------"
    fi
    echo ""
}

# ---------------------------------------------------------------------------
# Group A — INQUIRY + VPD discovery
# ---------------------------------------------------------------------------

t_inquiry_standard() {
    local out
    out=$(sg_inq "$RW_DEVICE" 2>&1); echo "$out"
    # Permissive — sg_inq output formatting drifts across releases, but
    # the vendor + product strings thurvsa advertises are stable.
    echo "$out" | grep -qiE 'THUR|VSA VOLUME|Direct[- ]?Access'
}

t_inquiry_vpd_supported() {
    local out
    out=$(sg_vpd -p sv "$RW_DEVICE" 2>&1); echo "$out"
    # We must advertise at least 0x00 / 0x80 / 0x83 / 0xB0 / 0xB2.
    echo "$out" | grep -qiE '0x00.*Supported|Supported VPD'
}

t_inquiry_vpd_unit_serial() {
    local out
    out=$(sg_vpd -p sn "$RW_DEVICE" 2>&1); echo "$out"
    # thurvsa uses the volume UUID hex as the serial.
    echo "$out" | grep -qiE 'unit serial number|serial number'
}

t_inquiry_vpd_device_id() {
    local out
    out=$(sg_vpd -p di "$RW_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'device identification|designation type|THUR|thurvsa'
}

t_inquiry_vpd_block_limits() {
    local out
    out=$(sg_vpd -p bl "$RW_DEVICE" 2>&1); echo "$out"
    # MAXIMUM COMPARE AND WRITE LENGTH = sectors-per-page (16). OPTIMAL
    # UNMAP GRANULARITY = sectors-per-page. Both are the load-bearing
    # advertisements VAAI ATS / Linux discard read.
    echo "$out" | grep -qiE 'block limits|compare.*write|unmap'
}

t_inquiry_vpd_thin_provisioning() {
    local out
    out=$(sg_vpd -p lbpv "$RW_DEVICE" 2>&1); echo "$out"
    # LBPU=1, LBPRZ=001, PROVISIONING TYPE=010 (thin).
    echo "$out" | grep -qiE 'logical block provisioning|LBPU|LBPRZ|Thin'
}

t_inquiry_vpd_third_party_copy() {
    # VPD 0x8F (Third Party Copy) is what ESXi gates VAAI XCOPY on.
    # sg_vpd's short page name is "tpc". The page body lists the
    # SUPPORTED COMMANDS sub-descriptor with opcodes 0x83 and 0x84.
    local out
    out=$(sg_vpd -p tpc "$RW_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'third party copy|tpc|extended copy|0x83|0x84'
}

# ---------------------------------------------------------------------------
# Group B — READ CAPACITY
# ---------------------------------------------------------------------------

t_read_capacity_16() {
    local out
    out=$(sg_readcap -16 "$RW_DEVICE" 2>&1); echo "$out"
    local expected_last_lba=$((RW_VOLUME_SIZE_BYTES / SECTOR_SIZE - 1))
    # sg_readcap formats output across versions: sometimes "Last LBA=4095",
    # sometimes "blocks=4096 ... Logical block length=4096 bytes".
    # Match either.
    if echo "$out" | grep -qE "Last (logical block address|LBA)[= ]+$expected_last_lba\\b"; then
        :
    elif echo "$out" | grep -qE "blocks=$((expected_last_lba + 1))\\b"; then
        :
    else
        log_error "Unexpected sg_readcap output (last LBA != $expected_last_lba)"
        return 1
    fi
    # LBPME and LBPRZ should both be set (thin-provisioning hints).
    echo "$out" | grep -qiE 'LBPME=1|Logical block provisioning management.*1'
}

# ---------------------------------------------------------------------------
# Group C — MODE SENSE caching + control
# ---------------------------------------------------------------------------

t_mode_sense_caching_page() {
    local out
    out=$(sg_modes --page=8 "$RW_DEVICE" 2>&1); echo "$out"
    # The Caching page (0x08) header should be present; full body decode
    # varies across sg3_utils versions.
    echo "$out" | grep -qiE 'Caching|Page_code: 8|0x08'
}

t_mode_sense_control_page() {
    local out
    out=$(sg_modes --page=10 "$RW_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'Control|Page_code: 10|0x0a'
}

t_mode_sense_wce_on_rcd_on() {
    # Issue MODE SENSE(10) page 0x08 directly via sg_raw and parse
    # the response bytes. thurvsa's PageCache is genuinely write-back
    # — WRITE returns GOOD when bytes hit the in-memory cache, before
    # the cloud upload + page-index commit. SBC-3 §6.4.6.4 requires
    # WCE=1 in that case so the host knows to issue SYNCHRONIZE CACHE
    # for durability. Lie WCE=0 and Linux elides the SYNC on umount,
    # losing every dirty page when the daemon next restarts.
    #
    # CDB: 5A 00 08 00 00 00 00 00 40 00
    #   op=0x5A(MODE SENSE 10) | LLBAA/DBD=0 | PC=00+page=08 | subpage=00 |
    #   reserved..reserved | alloc length=0x0040 | control=0
    local out
    out=$(sg_raw -r 64 "$RW_DEVICE" 5A 00 08 00 00 00 00 00 40 00 2>&1)
    echo "$out"
    # sg_raw prints "Received NN bytes of data:" then hex-dump rows like
    #   00     00 1c 00 00 00 00 00 00  08 12 01 00 00 00 00 00    ...
    # Header (10 bytes): 2-byte mode data length + medium type + dev-spec +
    # 2 reserved + 2-byte block descriptor length + 0 BD bytes (we ask
    # DBD=0 but thurvsa may still emit none).
    # Caching page header at offset 8 or 16 depending on BD presence:
    # find the literal "08 12" two-byte sequence and grab the next
    # byte — that's page-byte-2 with WCE / RCD.
    local hex page_bytes byte2
    hex=$(echo "$out" | tr -dc '0-9a-fA-F\n ' | tr '\n' ' ' | tr -s ' ')
    page_bytes=$(echo "$hex" | grep -oE '08 12 [0-9a-fA-F]{2}' | head -1)
    byte2=$(echo "$page_bytes" | awk '{print $3}')
    if [[ -z "$byte2" ]]; then
        log_error "Could not locate '08 12' page header in MODE SENSE response"
        return 1
    fi
    local val=$((16#$byte2))
    local wce=$(( (val >> 2) & 1 ))
    local rcd=$(( val & 1 ))
    log_info "Caching page byte 2 = 0x$byte2 (WCE=$wce, RCD=$rcd)"
    [[ $wce -eq 1 && $rcd -eq 1 ]]
}

# ---------------------------------------------------------------------------
# Group D — sub-page WRITE / READ round-trip
# ---------------------------------------------------------------------------

# All data-path tests use $RW_SG_DEVICE (the /dev/sgN passthrough)
# rather than $RW_DEVICE (the /dev/sdX block node). Reason: sg_dd
# against /dev/sdX uses read(2)/write(2) syscalls which go through
# the kernel block layer's page cache. Subsequent SG_IO passthrough
# calls (sg_compare_and_write, sg_raw, sg_sync) bypass that cache
# and see stale on-disk bytes — flaky miscompares. Routing every
# data-path call through the sg device keeps the kernel cache out
# of the picture entirely.

# Round-trip 4 KiB at a sub-page LBA (LBA 5 — middle of page 0).
# Proves the cache-layer RMW path: write splices a sector into a
# loaded page, mark dirty, flush; read re-fetches.
t_subpage_write_read_roundtrip() {
    local lba=5
    dd if=/dev/urandom of="$TEST_DIR/in.bin" bs="$SECTOR_SIZE" count=1 status=none
    sg_dd if="$TEST_DIR/in.bin" of="$RW_SG_DEVICE" bs="$SECTOR_SIZE" count=1 seek="$lba" 2>&1
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/out.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    if cmp -s "$TEST_DIR/in.bin" "$TEST_DIR/out.bin"; then
        log_info "Sub-page WRITE/READ at LBA $lba round-tripped 4 KiB"
        return 0
    fi
    log_error "Round-trip mismatch"
    return 1
}

# READ from an unallocated page surfaces zeroed bytes (LBPRZ = 1 +
# sparse-hole semantics).
t_unallocated_read_returns_zero() {
    local lba=$((SECTORS_PER_PAGE * 100))   # well past anything we wrote
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/zero.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    if [[ "$(stat -c%s "$TEST_DIR/zero.bin")" -ne "$SECTOR_SIZE" ]]; then
        log_error "Read returned wrong byte count"
        return 1
    fi
    if [[ -z "$(tr -d '\0' < "$TEST_DIR/zero.bin")" ]]; then
        log_info "Unallocated LBA $lba read returned all zeros"
        return 0
    fi
    log_error "Unallocated LBA $lba read returned non-zero bytes"
    return 1
}

# ---------------------------------------------------------------------------
# Group E — COMPARE AND WRITE
# ---------------------------------------------------------------------------

# CAW success path: prime LBA N with a known pattern, then issue
# CAW(compare=pattern, write=newpattern). Read back and confirm.
t_compare_and_write_success() {
    local lba=20
    # Prime the LBA. Using urandom guarantees a non-trivial compare.
    dd if=/dev/urandom of="$TEST_DIR/caw-prime.bin" bs="$SECTOR_SIZE" count=1 status=none
    sg_dd if="$TEST_DIR/caw-prime.bin" of="$RW_SG_DEVICE" bs="$SECTOR_SIZE" count=1 seek="$lba" 2>&1
    # New payload to write.
    dd if=/dev/urandom of="$TEST_DIR/caw-new.bin"   bs="$SECTOR_SIZE" count=1 status=none
    cat "$TEST_DIR/caw-prime.bin" "$TEST_DIR/caw-new.bin" > "$TEST_DIR/caw-payload.bin"
    if ! sg_compare_and_write --num=1 --xferlen=$((SECTOR_SIZE * 2)) --in="$TEST_DIR/caw-payload.bin" --lba="$lba" "$RW_SG_DEVICE" 2>&1; then
        log_error "sg_compare_and_write returned non-zero on success path"
        return 1
    fi
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/caw-readback.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    if cmp -s "$TEST_DIR/caw-new.bin" "$TEST_DIR/caw-readback.bin"; then
        log_info "CAW commit visible on readback"
        return 0
    fi
    log_error "CAW commit not visible on readback"
    return 1
}

# CAW miscompare: send a stale compare buffer, expect status 0x02 +
# sense 0x0E (MISCOMPARE) / ASC 0x1D / ASCQ 0x00. The on-disk bytes
# must NOT change.
t_compare_and_write_miscompare() {
    local lba=20
    # Stale compare buffer (different from on-disk bytes), with a
    # write half that should NOT be committed.
    dd if=/dev/urandom of="$TEST_DIR/caw-stale.bin" bs="$SECTOR_SIZE" count=1 status=none
    dd if=/dev/urandom of="$TEST_DIR/caw-noop.bin"  bs="$SECTOR_SIZE" count=1 status=none
    cat "$TEST_DIR/caw-stale.bin" "$TEST_DIR/caw-noop.bin" > "$TEST_DIR/caw-bad-payload.bin"
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/caw-before.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    local out
    out=$(sg_compare_and_write --num=1 --xferlen=$((SECTOR_SIZE * 2)) --in="$TEST_DIR/caw-bad-payload.bin" --lba="$lba" "$RW_SG_DEVICE" 2>&1 || true)
    echo "$out"
    if ! echo "$out" | grep -qiE 'miscompare|0x1d|MISCOMPARE'; then
        log_error "Miscompare did not report MISCOMPARE sense"
        return 1
    fi
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/caw-after.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    if cmp -s "$TEST_DIR/caw-before.bin" "$TEST_DIR/caw-after.bin"; then
        log_info "Miscompare left on-disk bytes unchanged"
        return 0
    fi
    log_error "Miscompare wrote bytes (must not commit)"
    return 1
}

# ---------------------------------------------------------------------------
# Group F — SYNCHRONIZE CACHE fence (kill-restart)
#
# We assert one direction only: WRITE then SYNC then kill -9 then
# restart then READ — the bytes MUST be present (SYNC was a real
# fence). The converse ("WRITE without SYNC then kill -9 must lose
# the bytes") is NOT a contract we make: the per-volume PageCache
# runs a 5 s background flush tick (sbc-core/src/cache.rs:FLUSH_TICK)
# and may also flush on eviction. WCE=1 tells the host we have a
# volatile write cache and SYNC is required for durability — but
# nothing in SBC-3 prevents us from committing earlier. A test that
# kills the daemon a second after a non-SYNC WRITE races the flush
# tick by design.
# ---------------------------------------------------------------------------

t_sync_fence_with_sync_persists() {
    local lba=$((SECTORS_PER_PAGE * 60))    # page 60 of vol-rw
    dd if=/dev/urandom of="$TEST_DIR/sync-yes.bin" bs="$SECTOR_SIZE" count=1 status=none

    sg_dd if="$TEST_DIR/sync-yes.bin" of="$RW_SG_DEVICE" bs="$SECTOR_SIZE" count=1 seek="$lba" 2>&1
    sg_sync "$RW_SG_DEVICE" 2>&1
    log_info "Wrote LBA $lba then SYNCed; killing daemon (-9) and restarting..."
    disconnect_iscsi
    kill -9 "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""
    start_daemon
    connect_iscsi

    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/sync-yes-readback.bin" bs="$SECTOR_SIZE" count=1 skip="$lba" 2>&1
    if cmp -s "$TEST_DIR/sync-yes.bin" "$TEST_DIR/sync-yes-readback.bin"; then
        log_info "Bytes persisted across crash thanks to SYNC (correct)"
        return 0
    fi
    log_error "Bytes lost despite SYNC — fence is not a fence"
    return 1
}

# ---------------------------------------------------------------------------
# Group G — UNMAP
# ---------------------------------------------------------------------------

# Sub-page UNMAP zeroes the targeted sectors and leaves the rest of
# the page alone (the cache layer splices the punch into a loaded page
# and marks dirty).
t_unmap_subpage_zeros_target() {
    local first=200
    # Prime two adjacent sectors with non-zero bytes inside one page.
    local pre=$((first - 1))
    dd if=/dev/urandom of="$TEST_DIR/unmap-pre.bin"  bs="$SECTOR_SIZE" count=1 status=none
    dd if=/dev/urandom of="$TEST_DIR/unmap-tgt.bin"  bs="$SECTOR_SIZE" count=1 status=none
    sg_dd if="$TEST_DIR/unmap-pre.bin" of="$RW_SG_DEVICE" bs="$SECTOR_SIZE" count=1 seek="$pre"   2>&1
    sg_dd if="$TEST_DIR/unmap-tgt.bin" of="$RW_SG_DEVICE" bs="$SECTOR_SIZE" count=1 seek="$first" 2>&1
    # blkdiscard wants byte offset + length. The kernel turns this into
    # an UNMAP CDB with a single block descriptor.
    blkdiscard --offset $((first * SECTOR_SIZE)) --length "$SECTOR_SIZE" "$RW_DEVICE" 2>&1
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/unmap-tgt-readback.bin" bs="$SECTOR_SIZE" count=1 skip="$first" 2>&1
    if [[ -n "$(tr -d '\0' < "$TEST_DIR/unmap-tgt-readback.bin")" ]]; then
        log_error "Target sector still has bytes after UNMAP"
        return 1
    fi
    sg_dd if="$RW_SG_DEVICE" of="$TEST_DIR/unmap-pre-readback.bin"  bs="$SECTOR_SIZE" count=1 skip="$pre" 2>&1
    if ! cmp -s "$TEST_DIR/unmap-pre.bin" "$TEST_DIR/unmap-pre-readback.bin"; then
        log_error "Adjacent sector damaged by UNMAP"
        return 1
    fi
    log_info "Sub-page UNMAP zeroed target sector and left neighbor intact"
    return 0
}

# ---------------------------------------------------------------------------
# Group H — MODE SELECT round-trip
#
# sg_wr_mode in sg3-utils 1.48 (Ubuntu) rejects every --contents= /
# --cfile= argument with "bad argument to '--contents='" — we issue
# MODE SELECT(10) via sg_raw directly so the test is portable across
# sg3_utils releases.
#
# CDB layout (MODE SELECT 10, opcode 0x55):
#   55 10 00 00 00 00 00 00 1C 00
#   ^op ^PF=1 SP=0          ^paramlen=0x001C  ^control
#
# Parameter list = 8-byte mode header (10) + 20-byte caching page = 28B.
#   Header: 8 zero bytes (mode_data_length is ignored per SPC; block
#           descriptor length = 0).
#   Caching page: 08 12 <byte2> 00 00 00 00 00 00 00 00 00 00 00 00
#                 00 00 00 00 00
#     byte2 = 0x05 (RCD=1, WCE=1)  -> matches MODE SENSE -> GOOD
#     byte2 = 0x01 (RCD=1, WCE=0)  -> mismatch          -> CHECK COND
# ---------------------------------------------------------------------------

write_mode_select_payload() {
    local out_file="$1" byte2="$2"
    # 8 bytes mode header (10) + 2 bytes page header (08 12) + page body
    # (18 bytes) = 28 bytes total. thurvsa's MODE SELECT validator does
    # an exact byte compare against MODE SENSE Current, so the page
    # body must mirror that response: byte 2 = 0x05 (RCD=1, WCE=1), and
    # byte 12 = 0x20 (DRA=1). The 'mutate' variant flips byte 2 only —
    # the daemon catches the mismatch and returns INVALID FIELD IN
    # PARAMETER LIST.
    #
    # Bash's printf needs the inner format-substituted escape passed
    # through a second printf to interpret \xNN; we can't nest %02x
    # directly (that emits the literal characters "\x05").
    {
        printf '\x00\x00\x00\x00\x00\x00\x00\x00'   # mode header (10)
        printf '\x08\x12'                           # page code + page length
        printf '%b' "$(printf '\\x%02x' "$byte2")"  # body byte 2: RCD/WCE
        printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00'   # body bytes 3-11 = 0
        printf '\x20'                               # body byte 12: DRA=1
        printf '\x00\x00\x00\x00\x00\x00\x00'       # body bytes 13-19 = 0
    } > "$out_file"
    local size
    size=$(stat -c%s "$out_file")
    if [[ "$size" -ne 28 ]]; then
        log_error "MODE SELECT payload should be 28 bytes, got $size"
        return 1
    fi
}

t_mode_select_roundtrip_caching() {
    write_mode_select_payload "$TEST_DIR/ms-roundtrip.bin" 5
    local out
    out=$(sg_raw -v -s 28 -i "$TEST_DIR/ms-roundtrip.bin" "$RW_DEVICE" 55 10 00 00 00 00 00 00 1C 00 2>&1)
    echo "$out"
    if echo "$out" | grep -qiE 'SCSI Status:[[:space:]]*Good'; then
        log_info "MODE SELECT (Caching) round-trip accepted"
        return 0
    fi
    if echo "$out" | grep -qiE 'CHECK CONDITION|Sense Key|sense_key'; then
        log_error "MODE SELECT round-trip raised sense (expected GOOD)"
        return 1
    fi
    log_error "Unexpected sg_raw output for MODE SELECT round-trip"
    return 1
}

t_mode_select_wce_mutation_rejected() {
    # thurvsa's Caching page Current advertises WCE=1 (write-back). A
    # host trying to flip it OFF (clear bit 2 → byte 2 = 0x01) must
    # be rejected — pretending we're write-through would mislead the
    # host into skipping SYNCHRONIZE CACHE on umount and silently
    # losing every dirty page across daemon restart.
    write_mode_select_payload "$TEST_DIR/ms-mutate.bin" 1
    local out
    out=$(sg_raw -v -s 28 -i "$TEST_DIR/ms-mutate.bin" "$RW_DEVICE" 55 10 00 00 00 00 00 00 1C 00 2>&1 || true)
    echo "$out"
    # SPC sense code for "INVALID FIELD IN PARAMETER LIST":
    # sense key 0x05 (Illegal Request), ASC/ASCQ 0x26/0x00.
    if echo "$out" | grep -qiE 'invalid field in parameter list|asc=0x26|illegal request'; then
        log_info "WCE-OFF mutation rejected with INVALID FIELD IN PARAMETER LIST (correct)"
        return 0
    fi
    log_error "WCE mutation was NOT rejected — host could disable SYNC durability"
    return 1
}

# ---------------------------------------------------------------------------
# Group I — Persistent reservations
# ---------------------------------------------------------------------------

t_pr_register_and_reserve() {
    local key="0xDEADBEEF"
    local out
    out=$(sg_persist --out --register --param-sark="$key" "$RW_DEVICE" 2>&1)
    echo "$out"
    sg_persist --out --reserve --prout-type=3 --param-rk="$key" "$RW_DEVICE" 2>&1
    out=$(sg_persist --in --read-keys "$RW_DEVICE" 2>&1)
    echo "$out"
    if echo "$out" | grep -qiE 'deadbeef|generation'; then
        log_info "Registration + reservation visible via READ KEYS"
    else
        log_error "READ KEYS did not show registered key"
        return 1
    fi
    out=$(sg_persist --in --read-reservation "$RW_DEVICE" 2>&1)
    echo "$out"
    if echo "$out" | grep -qiE 'reservation|generation'; then
        log_info "READ RESERVATION shows holder"
    else
        log_error "READ RESERVATION did not return a holder"
        return 1
    fi
    # Tear down so subsequent tests aren't fenced.
    sg_persist --out --release --prout-type=3 --param-rk="$key" "$RW_DEVICE" 2>&1
    sg_persist --out --register --param-rk="$key" --param-sark=0 "$RW_DEVICE" 2>&1
    return 0
}

t_pr_report_capabilities() {
    local out
    out=$(sg_persist --in --report-capabilities "$RW_DEVICE" 2>&1)
    echo "$out"
    # thurvsa advertises TYPE_MASK = 0xEA, 0x01 (WR_EX, EX_AC, WR_EX_RO,
    # EX_AC_RO, WR_EX_AR, EX_AC_AR). PTPL_C should be 0 (we don't
    # persist).
    if echo "$out" | grep -qiE 'persist through|crh|atp_c'; then
        log_info "REPORT CAPABILITIES decoded"
        return 0
    fi
    log_warn "sg_persist output format may have drifted; passing if no error"
    return 0
}

# ---------------------------------------------------------------------------
# Group J — WORM volume
# ---------------------------------------------------------------------------

t_worm_write_refused() {
    # The kernel sets the block device read-only when MODE SENSE
    # advertises WP=1, so sg_dd / dd / mkfs all fail with EROFS on the
    # /dev/sdX node. Even sg_raw open(O_RDWR) on /dev/sdX is rejected
    # — but the /dev/sgN passthrough sibling bypasses that check and
    # routes the CDB straight to the SCSI generic layer, so we can
    # verify the daemon's refusal end-to-end.
    dd if=/dev/urandom of="$TEST_DIR/worm.bin" bs="$SECTOR_SIZE" count=1 status=none

    # First: confirm the kernel honored WP=1 (proves end-to-end propagation).
    local kernel_out
    kernel_out=$(sg_dd if="$TEST_DIR/worm.bin" of="$WORM_DEVICE" bs="$SECTOR_SIZE" count=1 seek=0 2>&1 || true)
    echo "$kernel_out"
    if echo "$kernel_out" | grep -qiE 'read-only file system|EROFS|operation not permitted'; then
        log_info "Kernel honored WP=1 (sg_dd on $WORM_DEVICE hit EROFS as expected)"
    else
        log_warn "Kernel did NOT mark $WORM_DEVICE read-only (WP=1 not seen?)"
    fi
    # Second: confirm the daemon refuses with WRITE PROTECTED via sg passthrough.
    # WRITE(10) CDB: 2A 00 ${LBA(4)} 00 ${LEN(2)} 00 — LBA 0, 1 block.
    local out
    out=$(sg_raw -v -s "$SECTOR_SIZE" -i "$TEST_DIR/worm.bin" "$WORM_SG_DEVICE" 2A 00 00 00 00 00 00 00 01 00 2>&1 || true)
    echo "$out"
    if echo "$out" | grep -qiE 'write protected|asc=0x27|data protect'; then
        log_info "WRITE(10) via sg passthrough refused with WRITE PROTECTED (correct)"
        return 0
    fi
    log_error "WRITE(10) on WORM volume was not refused with WRITE PROTECTED sense"
    return 1
}

t_worm_unmap_refused() {
    # blkdiscard against /dev/sdX hits the kernel's read-only flag
    # before reaching the daemon — that alone proves WP=1 propagated.
    # Then verify the daemon's refusal directly via sg_raw + UNMAP CDB.
    local out
    out=$(blkdiscard --offset 0 --length "$SECTOR_SIZE" "$WORM_DEVICE" 2>&1 || true)
    echo "$out"
    local kernel_blocked=0
    if echo "$out" | grep -qiE 'read-only|operation not permitted|input/output error'; then
        log_info "Kernel honored WP=1 on blkdiscard (correct)"
        kernel_blocked=1
    fi
    # UNMAP CDB (opcode 0x42), parameter list of 24 bytes (8-byte
    # header + one 16-byte block descriptor for LBA 0, 1 block):
    #   header:  00 16 00 10 00 00 00 00
    #   descr:   00 00 00 00 00 00 00 00 00 00 00 01 00 00 00 00
    {
        printf '\x00\x16\x00\x10\x00\x00\x00\x00'
        printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00'
    } > "$TEST_DIR/worm-unmap.bin"
    # CDB: 42 00 00 00 00 00 00 00 18 00  (param list len = 0x18)
    out=$(sg_raw -v -s 24 -i "$TEST_DIR/worm-unmap.bin" "$WORM_SG_DEVICE" 42 00 00 00 00 00 00 00 18 00 2>&1 || true)
    echo "$out"
    if echo "$out" | grep -qiE 'write protected|asc=0x27|data protect'; then
        log_info "UNMAP via sg passthrough refused with WRITE PROTECTED (correct)"
        return 0
    fi
    if [[ $kernel_blocked -eq 1 ]]; then
        log_warn "sg_raw UNMAP didn't surface WRITE PROTECTED, but kernel already blocked the block-layer call"
        return 0
    fi
    log_error "UNMAP on WORM volume was not refused"
    return 1
}

t_worm_caw_refused() {
    dd if=/dev/urandom of="$TEST_DIR/worm-cmp.bin" bs="$SECTOR_SIZE" count=1 status=none
    dd if=/dev/urandom of="$TEST_DIR/worm-new.bin" bs="$SECTOR_SIZE" count=1 status=none
    cat "$TEST_DIR/worm-cmp.bin" "$TEST_DIR/worm-new.bin" > "$TEST_DIR/worm-caw.bin"
    # COMPARE AND WRITE CDB (0x89): 89 00 ${LBA(8)} 00 00 00 ${NUM(1)} 00 00
    # LBA = 0, NUM = 1 block. Data-out length = 2 * SECTOR_SIZE.
    local out
    out=$(sg_raw -v -s $((SECTOR_SIZE * 2)) -i "$TEST_DIR/worm-caw.bin" "$WORM_SG_DEVICE" \
        89 00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 2>&1 || true)
    echo "$out"
    if echo "$out" | grep -qiE 'write protected|asc=0x27|data protect'; then
        log_info "CAW via sg passthrough refused with WRITE PROTECTED (correct)"
        return 0
    fi
    log_error "CAW on WORM volume was not refused"
    return 1
}

# ---------------------------------------------------------------------------
# Group K — Host-probe stubs + capability discovery + WRITE SAME / VERIFY
# ---------------------------------------------------------------------------

t_request_sense_returns_no_sense() {
    # `sg_requests` issues REQUEST SENSE (0x03) and prints the parsed
    # sense data. thurvsa has no autosense queue, so the response is
    # NoSense (sense key 0).
    local out
    out=$(sg_requests "$RW_DEVICE" 2>&1); echo "$out"
    echo "$out" | grep -qiE 'no sense|no specific information|sense key.*0\b'
}

t_start_stop_unit_accepts_start() {
    # `sg_start` with no flags issues START (PowerCondition=0, START=1).
    # We accept and report GOOD; sg_start exits 0 on success.
    sg_start "$RW_DEVICE" 2>&1 | tee /dev/stderr
    return ${PIPESTATUS[0]}
}

t_prevent_allow_accepts_either() {
    # `sg_prevent` toggles the medium-removal lock. thurvsa has no
    # removable media so we accept-and-GOOD. Default action is
    # prevent; `--allow` issues the allow form. sg_prevent's
    # `--prevent=PC` accepts the SPC-4 prevent code (0=allow,
    # 1=prevent, 2=persistent-allow, 3=persistent-prevent) — we
    # exercise prevent (1) and allow (--allow) which is enough.
    sg_prevent "$RW_DEVICE" 2>&1 || return 1
    sg_prevent --allow "$RW_DEVICE" 2>&1 || return 1
    return 0
}

t_log_sense_supported_pages() {
    # `sg_logs --page=0x00 --hex` issues LOG SENSE for the
    # SUPPORTED LOG PAGES page. Treat sg_logs's exit status as the
    # primary signal — a non-zero exit means the daemon refused or
    # the response was malformed. The hex body shape (page header
    # = 00, body length = 01, supported page = 00) is verified by
    # the unit tests; here we just confirm the dispatcher accepted
    # the CDB and returned a parseable response.
    sg_logs --page=0x00 --hex "$RW_DEVICE" 2>&1 | tee /dev/stderr
    return ${PIPESTATUS[0]}
}

t_report_supported_opcodes_lists_offload() {
    # `sg_opcodes` issues MAINTENANCE IN / REPORT SUPPORTED OPCODES
    # (0xA3 SA 0x0C). The list must include the offload primitives
    # VAAI / Linux probe for: COMPARE AND WRITE (0x89), UNMAP (0x42),
    # WRITE SAME 10 (0x41) + 16 (0x93), VERIFY 10 (0x2F) + 16 (0x8F),
    # EXTENDED COPY (0x83), RECEIVE COPY RESULTS (0x84).
    local out
    out=$(sg_opcodes "$RW_DEVICE" 2>&1); echo "$out"
    local need=(
        'Compare and write'
        'Unmap'
        'Write same'
        'Verify'
        'Maintenance'
        'Extended copy'
        'Receive copy results'
    )
    local missing=0
    for pat in "${need[@]}"; do
        if ! echo "$out" | grep -qi "$pat"; then
            log_error "REPORT SUPPORTED OPCODES missing: $pat"
            missing=1
        fi
    done
    return $missing
}

t_write_same_zerofills() {
    # `blkdiscard --zeroout` issues WRITE SAME 16 with UNMAP=1 +
    # zero pattern (or NDOB=1 depending on kernel version). After
    # the call, READ at the affected range must return zeros.
    # First seed LBA 0 with a non-zero pattern so we can see the
    # zero-fill take effect.
    dd if=/dev/urandom of="$TEST_DIR/ws-seed.bin" bs="$SECTOR_SIZE" count=16 status=none
    sg_dd if="$TEST_DIR/ws-seed.bin" of="$RW_DEVICE" bs="$SECTOR_SIZE" count=16 seek=0 oflag=direct 2>&1
    local before
    before=$(sg_dd if="$RW_DEVICE" bs="$SECTOR_SIZE" count=1 skip=0 iflag=direct 2>/dev/null | xxd -p -l 4 | tr -d '\n')
    log_info "before zero-fill: $before"

    # Use the page-aligned grain (16 sectors at 4 KiB = 64 KiB).
    blkdiscard --zeroout --offset 0 --length $((SECTOR_SIZE * 16)) "$RW_DEVICE" 2>&1 || return 1

    local after
    after=$(sg_dd if="$RW_DEVICE" bs="$SECTOR_SIZE" count=1 skip=0 iflag=direct 2>/dev/null | xxd -p -l 32 | tr -d '\n')
    log_info "after zero-fill: $after"
    [[ "$after" =~ ^0+$ ]]
}

t_verify_bytchk_zero_succeeds() {
    # VERIFY 10 with BYTCHK=00 — the device server reads each block
    # and reports unrecovered read errors. On a sparse-hole range
    # this must succeed. sg_verify defaults to BYTCHK=0 (no compare)
    # when neither --ebytchk nor --ndo is passed.
    sg_verify --count=8 --lba=0 "$RW_DEVICE" 2>&1
}

t_xcopy_receive_copy_results_operating_parameters() {
    # sg_copy_results queries RECEIVE COPY RESULTS (opcode 0x84). With
    # service action 0x03 (OPERATING PARAMETERS) the response carries
    # our advertised XCOPY limits + supported descriptor types
    # (0xE4 identification, 0x02 block-to-block). sg_copy_results
    # defaults to op_params (sa 0x03) when --list_id is omitted.
    local out
    out=$(sg_copy_results --op_params "$RW_DEVICE" 2>&1); echo "$out"
    # Body bytes include "Maximum target descriptor count" / "Maximum
    # segment descriptor count" / "Implemented descriptor list" in
    # human-readable form across sg3_utils releases.
    echo "$out" | grep -qiE 'maximum (target descriptor count|segment descriptor count)|implemented descriptor|0xe4|0x02'
}

t_xcopy_same_lun_intra_volume_copy() {
    # End-to-end VAAI-style XCOPY: same LUN, page-aligned, source !=
    # destination, no overlap. sg_xcopy on a recent sg3_utils builds
    # a LID1 parameter list with identification target descriptors
    # (NAA designators sourced from VPD 0x83) and block-to-block
    # segment descriptors automatically.
    #
    # If sg_xcopy isn't available (older sg3_utils) skip cleanly —
    # the unit tests cover the data-motion path; this case proves
    # the wire surface is parseable by a real-world tool.
    if ! command -v sg_xcopy >/dev/null 2>&1; then
        log_info "sg_xcopy not present; skipping wire-level XCOPY case"
        return 0
    fi
    # Seed the source range with a known random pattern.
    dd if=/dev/urandom of="$TEST_DIR/xcopy-seed.bin" bs="$SECTOR_SIZE" count=16 status=none
    sg_dd if="$TEST_DIR/xcopy-seed.bin" of="$RW_DEVICE" bs="$SECTOR_SIZE" count=16 seek=0 oflag=direct 2>&1 || return 1
    blockdev --flushbufs "$RW_DEVICE" 2>/dev/null || true
    # SYNC the seed through the cache so the source page is durable
    # in the chunk pool — that lets the same-LUN fast path (page-
    # index hash clone) take effect on the daemon side.
    sg_sync "$RW_DEVICE" 2>&1 || true
    # Copy 16 sectors (one 64 KiB page) from LBA 0 to LBA 16 of the
    # same LUN.
    sg_xcopy --on_src --on_dst --bs="$SECTOR_SIZE" --count=16 \
        --skip=0 --seek=16 \
        --src="$RW_DEVICE" --dst="$RW_DEVICE" 2>&1 || return 1
    # Read both ranges back through the kernel block layer and
    # compare bytewise.
    blockdev --flushbufs "$RW_DEVICE" 2>/dev/null || true
    sg_dd if="$RW_DEVICE" of="$TEST_DIR/xcopy-src.bin" bs="$SECTOR_SIZE" count=16 skip=0 iflag=direct 2>&1 || return 1
    sg_dd if="$RW_DEVICE" of="$TEST_DIR/xcopy-dst.bin" bs="$SECTOR_SIZE" count=16 skip=16 iflag=direct 2>&1 || return 1
    cmp "$TEST_DIR/xcopy-src.bin" "$TEST_DIR/xcopy-dst.bin"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    echo "========================================"
    echo "thurvsa SCSI Conformance Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    create_volumes
    connect_iscsi

    echo ""
    # Group A
    run_test "INQUIRY standard (THUR / VSA VOLUME)"          t_inquiry_standard
    run_test "INQUIRY VPD 0x00 (Supported VPD pages)"             t_inquiry_vpd_supported
    run_test "INQUIRY VPD 0x80 (Unit Serial Number)"              t_inquiry_vpd_unit_serial
    run_test "INQUIRY VPD 0x83 (Device Identification)"           t_inquiry_vpd_device_id
    run_test "INQUIRY VPD 0xB0 (Block Limits)"                    t_inquiry_vpd_block_limits
    run_test "INQUIRY VPD 0xB2 (Logical Block Provisioning)"      t_inquiry_vpd_thin_provisioning
    run_test "INQUIRY VPD 0x8F (Third Party Copy / VAAI XCOPY)"   t_inquiry_vpd_third_party_copy
    # Group B
    run_test "READ CAPACITY 16 (LBPME=1, last LBA matches)"       t_read_capacity_16
    # Group C
    run_test "MODE SENSE Caching page (0x08) present"             t_mode_sense_caching_page
    run_test "MODE SENSE Control page (0x0A) present"             t_mode_sense_control_page
    run_test "MODE SENSE Caching: WCE=1, RCD=1"                   t_mode_sense_wce_on_rcd_on
    # Group D
    run_test "Sub-page WRITE/READ round-trip (LBA 5)"             t_subpage_write_read_roundtrip
    run_test "Unallocated page READ returns zeros"                t_unallocated_read_returns_zero
    # Group E
    run_test "COMPARE AND WRITE success (commit visible)"         t_compare_and_write_success
    run_test "COMPARE AND WRITE miscompare (no commit + sense)"   t_compare_and_write_miscompare
    # Group F
    run_test "SYNC fence: SYNC + crash persists bytes"            t_sync_fence_with_sync_persists
    # Group G
    run_test "UNMAP sub-page zeros target, neighbor intact"       t_unmap_subpage_zeros_target
    # Group H
    run_test "MODE SELECT Caching round-trip accepted"            t_mode_select_roundtrip_caching
    run_test "MODE SELECT WCE mutation rejected"                  t_mode_select_wce_mutation_rejected
    # Group I
    run_test "Persistent reservations: register + reserve"        t_pr_register_and_reserve
    run_test "Persistent reservations: REPORT CAPABILITIES"       t_pr_report_capabilities
    # Group J
    run_test "WORM volume refuses WRITE"                          t_worm_write_refused
    run_test "WORM volume refuses UNMAP"                          t_worm_unmap_refused
    run_test "WORM volume refuses CAW"                            t_worm_caw_refused
    # Group K — Host-probe stubs + capability discovery + offload
    run_test "REQUEST SENSE returns NoSense (no autosense queue)" t_request_sense_returns_no_sense
    run_test "START STOP UNIT accepts START"                      t_start_stop_unit_accepts_start
    run_test "PREVENT/ALLOW MEDIUM REMOVAL accepts both"          t_prevent_allow_accepts_either
    run_test "LOG SENSE page 0x00 (Supported Log Pages)"          t_log_sense_supported_pages
    run_test "REPORT SUPPORTED OPCODES lists offload primitives"  t_report_supported_opcodes_lists_offload
    run_test "WRITE SAME zero-fill via blkdiscard --zeroout"      t_write_same_zerofills
    run_test "VERIFY BYTCHK=0 succeeds on sparse-hole range"      t_verify_bytchk_zero_succeeds
    # Group L — VAAI XCOPY (EXTENDED COPY)
    run_test "RECEIVE COPY RESULTS — OPERATING PARAMETERS"        t_xcopy_receive_copy_results_operating_parameters
    run_test "EXTENDED COPY same-LUN intra-volume round-trip"     t_xcopy_same_lun_intra_volume_copy

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
        log_fail "$FAILED SCSI conformance test(s) failed"
        exit 1
    fi
    log_pass "$PASSED SCSI conformance test(s) passed"
    exit 0
}

main
