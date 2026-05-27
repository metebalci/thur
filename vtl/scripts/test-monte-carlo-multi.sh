#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvtl Monte Carlo Multi-Initiator Test
#
# KNOWN LIMITATION: this harness currently fails because the daemon's
# scsi_ssc::drive_manager acquires a per-session drive lock at first
# SCSI access (INQUIRY at iSCSI login) and holds it until session
# close (release_session_locks fires from the close path). The
# kernel iSCSI initiator probes ALL LUNs after --login, so whichever
# of A or B logs in first grabs locks on every drive and the other
# initiator's writes all return 'drive N is reserved by another
# session' (sense key Reservation Conflict). Genuine multi-initiator
# concurrent ops need either:
#   (a) per-cartridge / per-partition session admission (the daemon's
#       logical partitions design), or
#   (b) a lock model that grants per-command rather than per-session.
# The harness + workflow ship as the skeleton for when (a) or (b)
# lands; the workflow is manual-only (no schedule:) until then.
#
# Sibling to vtl/scripts/test-monte-carlo.sh. Spins up a single
# thurvtl daemon (4 drives, 8 slots, 6 carts) and runs two concurrent
# op streams from two distinct iSCSI initiator IQNs. Each initiator
# is pinned to a disjoint drive set + cart set:
#
#   Initiator A: iqn.2025-10.com.metebalci:mc-init-a
#       drives = [0, 1]
#       carts  = [MC01L8, MC02L8, MC03L8]
#
#   Initiator B: iqn.2025-10.com.metebalci:mc-init-b
#       drives = [2, 3]
#       carts  = [MC04L8, MC05L8, MC06L8]
#
# The pinning makes the two streams non-interfering at the
# application layer; the daemon's per-drive locking + concurrent SCSI
# dispatch is what we're actually exercising. Both streams hit the
# shared changer (mtx load / unload / move), which the daemon must
# serialize.
#
# Op mix: write_record / read_verify / rewind / load_cycle. Filemarks,
# changer_move, and import_export are skipped here — they're well
# covered by the single-initiator harness and they're not the focus.
#
# Multi-initiator iSCSI on one host uses iscsiadm's iface infrastructure:
# two named ifaces, each with its own iface.initiatorname, give the
# kernel two independent sessions to the same target. Per-session
# device discovery walks /sys/class/iscsi_session/ to correlate
# session -> host -> SCSI disks.
#
# Backend: local-only (the focus is concurrency, not backend variants).
# Backend variants are covered by the single-initiator harness via
# THURVTL_TEST_BACKEND.
#
# Prerequisites:
#   - mtx, mt-st, sg3-utils, open-iscsi, lsscsi
#   - iscsid running (sudo systemctl enable --now iscsid)
#   - Root/sudo access
#
# Usage:
#   ./vtl/scripts/test-monte-carlo-multi.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run (default: pick from /dev/urandom)
#   --quick               150 ops/initiator (default: 1000 ops/initiator)
#   --ops N               Override per-initiator op count
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvtld path
#   --cli-path PATH       Override thurvtl path
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port
#   --http-port PORT      Override HTTP port
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

TEST_DIR="/tmp/thurvtl-monte-carlo-multi-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
SEED=""
QUICK=0
OPS=""

CHANGER_DEVICE_A=""
CHANGER_DEVICE_B=""

# Chassis: 4 drives, 8 storage slots, 6 carts split 3/3 between the
# two initiators.
NUM_DRIVES=4
NUM_SLOTS=8
CARTS_A=(MC01L8 MC02L8 MC03L8)
CARTS_B=(MC04L8 MC05L8 MC06L8)
DRIVES_A=(0 1)
DRIVES_B=(2 3)
INIT_IQN_A="iqn.2025-10.com.metebalci:mc-init-a"
INIT_IQN_B="iqn.2025-10.com.metebalci:mc-init-b"
IFACE_A="thurvtl-mc-a"
IFACE_B="thurvtl-mc-b"

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --ops) OPS="$2"; shift 2 ;;
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

[[ $QUICK -eq 1 ]] && : "${OPS:=150}"
: "${OPS:=1000}"

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    iscsiadm -m node --targetname "$TARGET_IQN" -I "$IFACE_A" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" -I "$IFACE_B" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --op delete >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$IFACE_A" -o delete >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$IFACE_B" -o delete >/dev/null 2>&1 || true
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"
    local tool missing=()
    for tool in mtx mt iscsiadm lsscsi openssl curl cmp systemctl; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    [[ -x "$DAEMON_PATH" ]] || missing+=("thurvtld at $DAEMON_PATH")
    [[ -x "$CLI_PATH" ]] || missing+=("thurvtl at $CLI_PATH")
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        exit 1
    fi
    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service is not running."
        exit 1
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

library:
  num_slots: $NUM_SLOTS
  num_drives: $NUM_DRIVES
  lto_generation: 8

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"

disk_cache:
  disk_free_min_gb: 0

storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

keystore:
  backends:
    local: { type: local }
EOFCONFIG
}

start_daemon_mc() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    DAEMON_LOG_MODE=append start_thur_daemon
}

create_cartridges() {
    local c
    for c in "${CARTS_A[@]}" "${CARTS_B[@]}"; do
        log_info "Creating cartridge $c..."
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$c" \
            --lto-generation 8 --backend local >/dev/null 2>&1; then
            log_error "cartridge create $c failed"
            tail -20 "${TEST_DIR}/daemon.log"
            exit 1
        fi
    done
}

# Create a persistent iSCSI iface with a custom initiatorname; idempotent.
setup_iface() {
    local iface="$1" initiator_iqn="$2"
    iscsiadm -m iface -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$iface" -o update -n iface.initiatorname -v "$initiator_iqn" >/dev/null 2>&1
}

# Discover + login from a specific iface. Returns 0 on success.
iscsi_login_iface() {
    local iface="$1"
    local out
    if ! out=$(iscsiadm -m discovery -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" 2>&1); then
        log_error "discovery via iface $iface failed:"
        echo "$out" | sed 's/^/    /' >&2
        return 1
    fi
    if ! out=$(iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            -I "$iface" --login 2>&1); then
        log_error "login via iface $iface failed:"
        echo "$out" | sed 's/^/    /' >&2
        return 1
    fi
}

# For a given iface, list the SCSI block device paths attached. Walks
# /sys/class/iscsi_session/sessionN/iface_name to find the matching
# session, then maps session N -> host M via /sys/class/iscsi_host/, then
# enumerates devices under /sys/class/scsi_device/M\:*.
devices_for_iface() {
    local iface="$1"
    local session
    for session in /sys/class/iscsi_session/session*; do
        [[ -r "$session/iface_name" ]] || continue
        if [[ "$(cat "$session/iface_name")" == "$iface" ]]; then
            local host_dir
            for host_dir in "$session/device/target"*/; do
                # Walk into the target dir; SCSI devices appear as M:C:I:L subdirs.
                local sd
                for sd in "$host_dir"*/block/sd*; do
                    [[ -e "$sd" ]] && basename "$sd"
                done
            done
        fi
    done | sort
}

# Resolve changer + tape devices for an iface.
# Sets the iface-suffixed globals (CHANGER_DEVICE_A / TAPE_DEVS_A[] etc.)
# via nameref.
discover_devices_for_iface() {
    local iface="$1" suffix="$2"
    local -n changer_var="CHANGER_DEVICE_$suffix"
    local -n tapes_var="TAPE_DEVS_$suffix"
    local -n nsts_var="NST_DEVS_$suffix"
    local devs=()
    local _ tries=0
    while (( tries < 10 )); do
        mapfile -t devs < <(devices_for_iface "$iface")
        (( ${#devs[@]} >= NUM_DRIVES + 1 )) && break   # +1 for changer
        sleep 1
        tries=$(( tries + 1 ))
    done
    if (( ${#devs[@]} < NUM_DRIVES + 1 )); then
        log_error "iface $iface: only found ${#devs[@]} devices, expected $((NUM_DRIVES + 1))"
        return 1
    fi
    # Classify: the changer is the one lsscsi tags as 'mediumx'; tapes are 'tape'.
    local d row
    for d in "${devs[@]}"; do
        row=$(lsscsi -g 2>/dev/null | awk -v p="/dev/$d" '$NF==p || $(NF-1)==p {print}')
        if echo "$row" | grep -q mediumx; then
            changer_var=$(echo "$row" | awk '{print $NF}')
        elif echo "$row" | grep -q tape; then
            tapes_var+=("/dev/$d")
        fi
    done
    # Sort tape devices by their /dev/sdN name — that matches LUN order
    # so we can index by drive_id.
    IFS=$'\n' tapes_var=($(printf '%s\n' "${tapes_var[@]}" | sort)) ; unset IFS
    local i
    for (( i=0; i<${#tapes_var[@]}; i++ )); do
        nsts_var+=("$(echo "${tapes_var[$i]}" | sed 's|/dev/sd|/dev/sg|')")  # placeholder, see below
        nsts_var[$i]=$(echo "${tapes_var[$i]}" | sed 's|/dev/st|/dev/nst|; s|/dev/sd|/dev/nst|')
    done
    # Actually for tape devices the path is /dev/stN, not /dev/sdN; the
    # devices_for_iface walk found block dirs which are only for the
    # changer's sg path. Re-walk for /dev/stN under the iface.
    # Simpler: just grep lsscsi for tapes attached to the iface's host.
    return 0
}

iscsi_login() {
    setup_iface "$IFACE_A" "$INIT_IQN_A"
    setup_iface "$IFACE_B" "$INIT_IQN_B"
    iscsi_login_iface "$IFACE_A" || { log_error "iscsi login (A) failed"; return 1; }
    iscsi_login_iface "$IFACE_B" || { log_error "iscsi login (B) failed"; return 1; }
    sleep 4
}

# Get all THUR VTL device rows from lsscsi, grouped by [H:C:I:L] host.
# Returns one of two device groups for the requested initiator suffix.
# We map iface -> session -> host via sysfs, then filter lsscsi by host.
resolve_initiator_devices() {
    local iface="$1"
    local host_id session
    for session in /sys/class/iscsi_session/session*; do
        [[ -r "$session/ifacename" ]] || continue
        if [[ "$(cat "$session/ifacename")" != "$iface" ]]; then continue; fi
        # The class symlink resolves to .../platform/host<H>/session<N>/iscsi_session/session<N>
        # (Ubuntu 22+ open-iscsi 2.1.x). Parse the host id out of the
        # resolved path so we can correlate to lsscsi's [H:C:I:L] tuples.
        local real
        real=$(readlink -f "$session")
        host_id=$(echo "$real" | sed -nE 's,.*/platform/host([0-9]+)/.*,\1,p')
        [[ -n "$host_id" ]] && break
    done
    if [[ -z "$host_id" ]]; then
        echo "[ERROR] Could not find host_id for iface $iface" >&2
        for session in /sys/class/iscsi_session/session*; do
            [[ -r "$session/ifacename" ]] || continue
            echo "[ERROR]   $session/ifacename -> $(cat "$session/ifacename" 2>/dev/null)" >&2
        done
        return 1
    fi
    lsscsi -g | awk -v p="[$host_id:" 'index($1, p) == 1 {print}'
}

setup_initiator_devices() {
    local suffix="$1" iface="$2"
    local rows
    rows=$(resolve_initiator_devices "$iface") || return 1
    local changer tape_devs=() nst_devs=() row
    while IFS= read -r row; do
        if [[ "$row" == *mediumx* ]]; then
            changer=$(echo "$row" | awk '{print $NF}')
        elif [[ "$row" == *tape* ]]; then
            local stdev
            stdev=$(echo "$row" | awk '{print $(NF-1)}')
            tape_devs+=("$stdev")
            nst_devs+=("$(echo "$stdev" | sed 's|/dev/st|/dev/nst|')")
        fi
    done <<< "$rows"
    if [[ -z "$changer" ]]; then
        log_error "initiator $suffix: no changer device found"
        echo "$rows" >&2
        return 1
    fi
    if (( ${#tape_devs[@]} < NUM_DRIVES )); then
        log_error "initiator $suffix: only ${#tape_devs[@]} tape devices found, expected $NUM_DRIVES"
        echo "$rows" >&2
        return 1
    fi
    # Sort tape paths by /dev/stN N — that matches LUN order ascending.
    IFS=$'\n' nst_devs=($(printf '%s\n' "${nst_devs[@]}" | sort -V)) ; unset IFS
    eval "CHANGER_DEVICE_${suffix}=\"\$changer\""
    eval "NST_DEVS_${suffix}=(\"\${nst_devs[@]}\")"
    log_info "initiator $suffix: changer=$changer drives=(${nst_devs[*]})"
}

# ---------------------------------------------------------------------------
# Per-initiator op loop. Runs in a subshell. Args:
#   $1 = initiator suffix ("A" or "B")
#   $2 = changer device path
#   $3 = pipe-separated /dev/nstN device list (one per pinned drive)
#   $4 = comma-separated drive-id list (e.g. "0,1")
#   $5 = comma-separated cart barcode list
#   $6 = ops count
#   $7 = ops log path
#   $8 = MC_SEED for this run
# ---------------------------------------------------------------------------

run_initiator_ops() {
    local suffix="$1" changer="$2" nst_list="$3" drive_ids_csv="$4" carts_csv="$5"
    local n_ops="$6" ops_log="$7" seed="$8"

    local -a NST_DEVS
    IFS='|' read -ra NST_DEVS <<< "$nst_list"
    local -a PINNED_DRIVES
    IFS=',' read -ra PINNED_DRIVES <<< "$drive_ids_csv"
    local -a CARTS
    IFS=',' read -ra CARTS <<< "$carts_csv"

    MC_SEED="$seed"
    MC_OP_LOG="$ops_log"
    : > "$MC_OP_LOG"
    mc_op_stats_init

    declare -A RECORDS
    declare -A NEXT_REC_IDX
    declare -A DRIVE_LOADED
    local c di
    for c in "${CARTS[@]}"; do RECORDS[$c]=""; NEXT_REC_IDX[$c]=0; done
    for di in "${PINNED_DRIVES[@]}"; do DRIVE_LOADED[$di]=""; done

    # ---- helpers ---------------------------------------------------------
    daemon_loaded_in_drive_local() {
        local drive_idx="$1"
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
            | python3 -c "
import sys, json
di = $drive_idx
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('location') == 'drive' and int(c.get('slot_id', -1)) == di:
        print(c.get('barcode', ''))
        break
"
    }
    daemon_drive_of_cart_local() {
        local bc="$1"
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
            | python3 -c "
import sys, json
bc = '$bc'
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('barcode') == bc and c.get('location') == 'drive':
        sid = c.get('slot_id')
        if sid is not None: print(int(sid))
        break
"
    }
    slot_of_cart_local() {
        local bc="$1"
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
            | python3 -c "
import sys, json
bc = '$bc'
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('barcode') == bc and c.get('location') == 'storage':
        sid = c.get('slot_id')
        if sid is not None: print(int(sid) + 1)
        break
"
    }
    any_empty_slot_local() {
        mtx -f "$changer" status 2>/dev/null \
            | awk '/Storage Element [0-9]+:Empty/ { for (i=1;i<=NF;i++) if ($i == "Element") { print $(i+1); exit } }'
    }
    # NST_DEVS is the full per-initiator device list, sorted by LUN —
    # LUN i+1 = drive i, so position == absolute drive_id.
    nst_for_drive() {
        echo "${NST_DEVS[$1]}"
    }
    pick_pinned_drive() {
        local idx
        idx=$(mc_rng_u32 "drive-pick-$suffix" "${#PINNED_DRIVES[@]}")
        echo "${PINNED_DRIVES[$idx]}"
    }
    ensure_loaded_local() {
        local want="$1" drive_idx="$2"
        DRIVE_LOADED[$drive_idx]=$(daemon_loaded_in_drive_local "$drive_idx")
        local current="${DRIVE_LOADED[$drive_idx]}"
        if [[ -z "$want" && -n "$current" ]]; then return 0; fi
        if [[ -n "$want" && "$current" == "$want" ]]; then return 0; fi
        if [[ -n "$want" ]]; then
            local other
            other=$(daemon_drive_of_cart_local "$want")
            if [[ -n "$other" && "$other" != "$drive_idx" ]]; then
                # Cart pinned to our set is loaded on the OTHER initiator's
                # drives — this means the daemon state drifted, since we
                # don't share carts. Fail loudly so a bug here doesn't
                # masquerade as an unloaded retry loop.
                local in_my_set=0 pd
                for pd in "${PINNED_DRIVES[@]}"; do
                    [[ "$pd" -eq "$other" ]] && { in_my_set=1; break; }
                done
                if (( in_my_set == 0 )); then
                    log_error "init $suffix: cart $want is in drive $other (other initiator's)"
                    return 1
                fi
                local other_nst
                other_nst=$(nst_for_drive "$other")
                mt -f "$other_nst" rewind >/dev/null 2>&1 || true
                local origin
                origin=$(any_empty_slot_local)
                [[ -z "$origin" ]] && origin=1
                mtx -f "$changer" unload "$origin" "$other" >/dev/null 2>&1 || true
                DRIVE_LOADED[$other]=""
            fi
        fi
        if [[ -n "$current" ]]; then
            mt -f "$(nst_for_drive "$drive_idx")" rewind >/dev/null 2>&1 || true
            local origin
            origin=$(any_empty_slot_local)
            [[ -z "$origin" ]] && origin=1
            if ! mtx -f "$changer" unload "$origin" "$drive_idx" >/dev/null 2>&1; then
                log_error "init $suffix ensure_loaded: unload of $current from drive $drive_idx failed"
                return 1
            fi
            DRIVE_LOADED[$drive_idx]=""
        fi
        local target_bc target_slot
        if [[ -n "$want" ]]; then
            target_bc="$want"
            target_slot=$(slot_of_cart_local "$want")
        else
            local loaded_set=" " pd
            for pd in "${PINNED_DRIVES[@]}"; do
                [[ -n "${DRIVE_LOADED[$pd]}" ]] && loaded_set+="${DRIVE_LOADED[$pd]} "
            done
            local tries=0 cand
            while (( tries < ${#CARTS[@]} )); do
                cand="${CARTS[$(mc_rng_u32 "load-pick-$suffix-$tries" "${#CARTS[@]}")]}"
                if [[ "$loaded_set" != *" $cand "* ]]; then
                    target_slot=$(slot_of_cart_local "$cand")
                    [[ -n "$target_slot" ]] && { target_bc="$cand"; break; }
                fi
                tries=$(( tries + 1 ))
            done
        fi
        if [[ -z "$target_slot" ]]; then
            log_error "init $suffix ensure_loaded: no cart available for drive $drive_idx"
            return 1
        fi
        if ! mtx -f "$changer" load "$target_slot" "$drive_idx" >/dev/null 2>&1; then
            log_error "init $suffix ensure_loaded: load of $target_bc -> drive $drive_idx failed"
            return 1
        fi
        DRIVE_LOADED[$drive_idx]="$target_bc"
    }
    seek_eod_local() {
        local drive_idx="$1"
        local bc="${DRIVE_LOADED[$drive_idx]}"
        [[ -z "$bc" ]] && return 0
        local nst
        nst=$(nst_for_drive "$drive_idx")
        mt -f "$nst" rewind >/dev/null 2>&1
        local n=0 line
        while IFS= read -r line; do
            [[ "$line" == R:* ]] && n=$(( n + 1 ))
        done <<< "${RECORDS[$bc]}"
        (( n > 0 )) && mt -f "$nst" fsr "$n" >/dev/null 2>&1
    }

    # ---- op handlers -----------------------------------------------------
    op_write() {
        local drive_idx; drive_idx=$(pick_pinned_drive)
        ensure_loaded_local "" "$drive_idx" || return 1
        seek_eod_local "$drive_idx"
        local bc="${DRIVE_LOADED[$drive_idx]}"
        local idx="${NEXT_REC_IDX[$bc]}"
        local size; size=$(mc_pick_size_boundary_biased "size-write-$suffix")
        (( size > 4194304 )) && size=4194304
        local tmp="$TEST_DIR/scratch-$suffix.rec"
        mc_content_to "$bc" "$idx" "$size" "$tmp"
        local nst; nst=$(nst_for_drive "$drive_idx")
        if ! dd if="$tmp" of="$nst" bs="$size" count=1 status=none 2>/dev/null; then
            log_error "init $suffix write: dd failed (drive=$drive_idx bc=$bc idx=$idx size=$size)"
            return 1
        fi
        RECORDS[$bc]+="R:$idx:$size"$'\n'
        NEXT_REC_IDX[$bc]=$(( idx + 1 ))
        mc_log_op write init="$suffix" drive="$drive_idx" cart="$bc" idx="$idx" size="$size"
    }
    op_read_verify() {
        local candidates=() bc
        for c in "${CARTS[@]}"; do [[ -n "${RECORDS[$c]}" ]] && candidates+=("$c"); done
        if (( ${#candidates[@]} == 0 )); then
            mc_log_op read_verify init="$suffix" status=no_records
            return 0
        fi
        bc="${candidates[$(mc_rng_u32 "verify-cart-$suffix" "${#candidates[@]}")]}"
        local drive_idx; drive_idx=$(pick_pinned_drive)
        ensure_loaded_local "$bc" "$drive_idx" || return 1
        local nst; nst=$(nst_for_drive "$drive_idx")
        mt -f "$nst" rewind >/dev/null 2>&1 || return 1
        local entry kind idx size n=0
        local expected="$TEST_DIR/scratch-$suffix.expect" actual="$TEST_DIR/scratch-$suffix.actual"
        while IFS= read -r entry; do
            [[ -z "$entry" ]] && continue
            kind="${entry%%:*}"
            local rest="${entry#*:}"
            idx="${rest%%:*}"; size="${rest#*:}"
            if [[ "$kind" == "R" ]]; then
                mc_content_to "$bc" "$idx" "$size" "$expected"
                if ! dd if="$nst" of="$actual" bs="$size" count=1 status=none 2>/dev/null; then
                    log_error "init $suffix read: dd failed (bc=$bc idx=$idx size=$size)"
                    return 1
                fi
                local actual_size; actual_size=$(stat -c%s "$actual")
                if [[ "$actual_size" != "$size" ]]; then
                    log_error "init $suffix read: short read on $bc idx=$idx exp=$size got=$actual_size"
                    return 1
                fi
                if ! cmp -s "$expected" "$actual"; then
                    log_error "init $suffix read: content mismatch on $bc idx=$idx size=$size"
                    return 1
                fi
                n=$(( n + 1 ))
            fi
        done <<< "${RECORDS[$bc]}"
        mc_log_op read_verify init="$suffix" drive="$drive_idx" cart="$bc" records="$n"
    }
    op_rewind() {
        local drive_idx; drive_idx=$(pick_pinned_drive)
        ensure_loaded_local "" "$drive_idx" || return 1
        mt -f "$(nst_for_drive "$drive_idx")" rewind >/dev/null 2>&1 || true
        mc_log_op rewind init="$suffix" drive="$drive_idx" cart="${DRIVE_LOADED[$drive_idx]}"
    }
    op_load_cycle() {
        local drive_idx; drive_idx=$(pick_pinned_drive)
        DRIVE_LOADED[$drive_idx]=$(daemon_loaded_in_drive_local "$drive_idx")
        local prev="${DRIVE_LOADED[$drive_idx]}"
        if [[ -z "$prev" ]]; then
            mc_log_op load_cycle init="$suffix" drive="$drive_idx" status=already_empty
            return 0
        fi
        mt -f "$(nst_for_drive "$drive_idx")" rewind >/dev/null 2>&1 || true
        local origin; origin=$(any_empty_slot_local); [[ -z "$origin" ]] && origin=1
        if ! mtx -f "$changer" unload "$origin" "$drive_idx" >/dev/null 2>&1; then
            log_error "init $suffix load_cycle: unload failed (drive=$drive_idx cart=$prev)"
            return 1
        fi
        DRIVE_LOADED[$drive_idx]=""
        mc_log_op load_cycle init="$suffix" drive="$drive_idx" prev="$prev"
    }

    local WEIGHTS=("30:write" "55:read_verify" "8:rewind" "7:load_cycle")
    mc_assert_weights "op-$suffix" "${WEIGHTS[@]}"

    local progress_every=$(( n_ops / 10 )); (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n_ops; MC_OP_INDEX++ )); do
        local op
        op=$(mc_pick_weighted "op-$suffix" "${WEIGHTS[@]}")
        case "$op" in
            write)        op_write || exit 1 ;;
            read_verify)  op_read_verify || exit 1 ;;
            rewind)       op_rewind || exit 1 ;;
            load_cycle)   op_load_cycle || exit 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            log_info "  [init $suffix $MC_OP_INDEX/$n_ops] loaded=$(for di in "${PINNED_DRIVES[@]}"; do printf 'd%s=%s ' "$di" "${DRIVE_LOADED[$di]:-<empty>}"; done)"
        fi
    done
    mc_op_stats_dump
    exit 0
}

main() {
    echo "========================================"
    echo "thurvtl Monte Carlo Multi-Initiator Test"
    echo "========================================"
    echo ""
    check_prerequisites
    assign_ports
    create_test_config
    start_daemon_mc
    create_cartridges
    iscsi_login
    setup_initiator_devices A "$IFACE_A" || exit 1
    setup_initiator_devices B "$IFACE_B" || exit 1

    mc_seed_init "$SEED" "$TEST_DIR/ops.log"
    log_info "Running $OPS ops/initiator concurrently across 2 initiators (4 drives, 6 carts)"

    # Resolve per-initiator state and serialise into compact args.
    local nst_a nst_b
    nst_a=$(IFS='|'; echo "${NST_DEVS_A[*]}")
    nst_b=$(IFS='|'; echo "${NST_DEVS_B[*]}")
    local da_csv db_csv ca_csv cb_csv
    da_csv=$(IFS=','; echo "${DRIVES_A[*]}")
    db_csv=$(IFS=','; echo "${DRIVES_B[*]}")
    ca_csv=$(IFS=','; echo "${CARTS_A[*]}")
    cb_csv=$(IFS=','; echo "${CARTS_B[*]}")

    # Per-initiator subshells. Each seeds from a DERIVED seed (parent
    # seed XOR initiator tag) so the two streams aren't byte-identical.
    local seed_a=$(( MC_SEED + 1 ))
    local seed_b=$(( MC_SEED + 2 ))

    (
        run_initiator_ops A "$CHANGER_DEVICE_A" "$nst_a" "$da_csv" "$ca_csv" \
            "$OPS" "$TEST_DIR/ops-a.log" "$seed_a"
    ) &
    local pid_a=$!
    (
        run_initiator_ops B "$CHANGER_DEVICE_B" "$nst_b" "$db_csv" "$cb_csv" \
            "$OPS" "$TEST_DIR/ops-b.log" "$seed_b"
    ) &
    local pid_b=$!

    local rc_a=0 rc_b=0
    wait "$pid_a" || rc_a=$?
    wait "$pid_b" || rc_b=$?

    if (( rc_a != 0 || rc_b != 0 )); then
        log_fail "Concurrent op loop failed (A exit=$rc_a B exit=$rc_b)"
        log_info "Op log A: $TEST_DIR/ops-a.log"
        log_info "Op log B: $TEST_DIR/ops-b.log"
        exit 1
    fi

    echo ""
    echo "========================================"
    log_pass "$OPS ops/initiator x 2 initiators  (seed=$MC_SEED)"
    echo "========================================"
    echo "  reusable reproducer: --seed $MC_SEED --ops $OPS"
    echo "  op logs: $TEST_DIR/ops-{a,b}.log"
    exit 0
}

main
