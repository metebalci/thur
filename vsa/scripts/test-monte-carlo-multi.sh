#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa Monte Carlo Multi-Initiator Test
#
# KNOWN LIMITATION: mkfs.ext4 currently refuses with "/dev/sdX is
# apparently in use by the system" when both initiator sessions are
# logged in concurrently — the kernel's post-login LUN probe holds
# the device long enough that mkfs (even with -F) refuses. The
# daemon side has no per-volume session lock (each PageCache is
# accessed concurrently by design), so once the harness gets past
# mkfs, the per-volume PageCache isolation + write-back ordering
# under concurrent host traffic should validate cleanly. Workarounds
# considered: stagger logins, mkfs from a single-initiator pre-flight
# then attach the second initiator after — neither lands cleanly with
# iscsiadm's iface plumbing. The harness + workflow ship as a
# skeleton; the workflow is manual-only until the mkfs path stabilises.
#
# Sibling to vsa/scripts/test-monte-carlo.sh. Spins up a single
# thurvsad daemon serving 4 volumes (vol-mc-a1, vol-mc-a2,
# vol-mc-b1, vol-mc-b2) over iSCSI, and runs two concurrent op
# streams from two distinct initiator IQNs:
#
#   Initiator A: iqn.2025-10.com.metebalci:vsa-mc-a
#       volumes = [vol-mc-a1, vol-mc-a2]
#       mount   = TEST_DIR/mnt-a-{a1,a2}
#
#   Initiator B: iqn.2025-10.com.metebalci:vsa-mc-b
#       volumes = [vol-mc-b1, vol-mc-b2]
#       mount   = TEST_DIR/mnt-b-{b1,b2}
#
# Volumes are private to each initiator at the application layer
# (the daemon makes all 4 visible to both sessions; the harness just
# never touches the other initiator's volumes). The daemon's
# per-volume PageCache + write-back ordering + SYNCHRONIZE CACHE
# fencing is what we're exercising under concurrent host traffic.
#
# Op mix: write_new / overwrite / read_verify / delete / sync.
# Directory ops + write_at_offset + restart are skipped here — the
# single-initiator harness covers those well; this one focuses on
# concurrent dispatch.
#
# Transport: iSCSI only. NVMe/TCP multi-initiator (separate hostnqns)
# is left for a follow-up; the per-volume isolation property is the
# same, so adding it is mechanical.
#
# Prerequisites:
#   - e2fsprogs, util-linux, openssl
#   - open-iscsi + iscsid running
#   - Root/sudo access
#
# Usage:
#   ./vsa/scripts/test-monte-carlo-multi.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run (default: pick from /dev/urandom)
#   --quick               150 ops/initiator (default: 1000 ops/initiator)
#   --ops N               Override per-initiator op count
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvsad path
#   --cli-path PATH       Override thurvsa path
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

TEST_DIR="/tmp/thurvsa-monte-carlo-multi-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SEED=""
QUICK=0
OPS=""

VOLUME_NAMES_A=("vol-mc-a1" "vol-mc-a2")
VOLUME_NAMES_B=("vol-mc-b1" "vol-mc-b2")
VOLUME_SIZE_MIB=512
INIT_IQN_A="iqn.2025-10.com.metebalci:vsa-mc-a"
INIT_IQN_B="iqn.2025-10.com.metebalci:vsa-mc-b"
IFACE_A="thurvsa-mc-a"
IFACE_B="thurvsa-mc-b"

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
    local mp
    for mp in "$TEST_DIR"/mnt-*; do
        [[ -d "$mp" ]] && mountpoint -q "$mp" 2>/dev/null && umount "$mp" 2>/dev/null || true
    done
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
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"
    local tool missing=()
    for tool in mkfs.ext4 mount umount openssl curl cmp systemctl iscsiadm lsscsi; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    [[ -x "$DAEMON_PATH" ]] || missing+=("thurvsad at $DAEMON_PATH")
    [[ -x "$CLI_PATH" ]] || missing+=("thurvsa at $CLI_PATH")
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
    mkdir -p "$TEST_DIR/data/volumes"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"
EOFCONFIG
}

start_daemon_mc() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local _
    for _ in {1..60}; do
        if ss -tln 2>/dev/null | grep -q ":$ISCSI_PORT\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $ISCSI_PORT)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon did not become ready"; tail -30 "${TEST_DIR}/daemon.log"; exit 1
}

create_volumes() {
    local name
    for name in "${VOLUME_NAMES_A[@]}" "${VOLUME_NAMES_B[@]}"; do
        log_info "Creating volume $name (${VOLUME_SIZE_MIB} MiB)..."
        "$CLI_PATH" --config "$TEST_CONFIG" volume create "$name" \
            --size "${VOLUME_SIZE_MIB}M" --backend local >/dev/null
    done
}

setup_iface() {
    local iface="$1" initiator_iqn="$2"
    iscsiadm -m iface -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$iface" -o update -n iface.initiatorname -v "$initiator_iqn" >/dev/null 2>&1
}

iscsi_login_iface() {
    local iface="$1"
    iscsiadm -m discovery -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
        -I "$iface" --login >/dev/null 2>&1
}

# Walk sysfs to find the SCSI host for an iface's session, then
# enumerate /dev/sdN under that host. Returns one /dev/sdN per LUN in
# numeric LUN order (= volume creation order = alphabetical within
# the boot-time discover_and_register sort).
resolve_initiator_devices() {
    local iface="$1"
    local session host_id=""
    for session in /sys/class/iscsi_session/session*; do
        [[ -r "$session/ifacename" ]] || continue
        if [[ "$(cat "$session/ifacename")" != "$iface" ]]; then continue; fi
        # Parse host_id from the symlink-resolved real path; see the
        # equivalent helper in vtl/scripts/test-monte-carlo-multi.sh.
        local real
        real=$(readlink -f "$session")
        host_id=$(echo "$real" | sed -nE 's,.*/platform/host([0-9]+)/.*,\1,p')
        [[ -n "$host_id" ]] && break
    done
    if [[ -z "$host_id" ]]; then
        echo "[ERROR] Could not find host_id for iface $iface" >&2
        return 1
    fi
    # lsscsi prefixes rows with [H:C:I:L]; lsscsi sorts by LUN within host.
    lsscsi 2>/dev/null | awk -v p="[$host_id:" 'index($1, p) == 1 && /THUR VSA/ {print $NF}'
}

setup_initiator_devices() {
    local suffix="$1" iface="$2"
    local devs=() row
    local tries=0
    while (( tries < 10 )); do
        mapfile -t devs < <(resolve_initiator_devices "$iface")
        # We expect 4 LUNs visible (the daemon serves all volumes).
        (( ${#devs[@]} >= 4 )) && break
        sleep 1
        tries=$(( tries + 1 ))
    done
    if (( ${#devs[@]} < 4 )); then
        log_error "initiator $suffix: only ${#devs[@]} THUR VSA devices visible (expected 4)"
        return 1
    fi
    # The boot-time discover_and_register sorts volumes alphabetically:
    #   vol-mc-a1 (LUN 0) -> devs[0]
    #   vol-mc-a2 (LUN 1) -> devs[1]
    #   vol-mc-b1 (LUN 2) -> devs[2]
    #   vol-mc-b2 (LUN 3) -> devs[3]
    if [[ "$suffix" == "A" ]]; then
        eval "RW_DEVS_A=(\"${devs[0]}\" \"${devs[1]}\")"
    else
        eval "RW_DEVS_B=(\"${devs[2]}\" \"${devs[3]}\")"
    fi
    log_info "initiator $suffix: devices=(${devs[*]})"
}

iscsi_login_both() {
    setup_iface "$IFACE_A" "$INIT_IQN_A"
    setup_iface "$IFACE_B" "$INIT_IQN_B"
    iscsi_login_iface "$IFACE_A" || { log_error "iscsi login (A) failed"; return 1; }
    iscsi_login_iface "$IFACE_B" || { log_error "iscsi login (B) failed"; return 1; }
    sleep 4
}

# One-shot per-volume mkfs + mount under per-initiator mount points.
mkfs_and_mount_all() {
    local i
    local -a RW_DEVS VOLS MNTS
    for suffix in A B; do
        if [[ "$suffix" == "A" ]]; then
            RW_DEVS=("${RW_DEVS_A[@]}")
            VOLS=("${VOLUME_NAMES_A[@]}")
        else
            RW_DEVS=("${RW_DEVS_B[@]}")
            VOLS=("${VOLUME_NAMES_B[@]}")
        fi
        for (( i=0; i<${#VOLS[@]}; i++ )); do
            local mp="${TEST_DIR}/mnt-${suffix,,}-${VOLS[$i]}"
            mkdir -p "$mp"
            log_info "mkfs.ext4 on ${RW_DEVS[$i]} (volume ${VOLS[$i]})"
            local mkfs_out
            if ! mkfs_out=$(mkfs.ext4 -F "${RW_DEVS[$i]}" 2>&1); then
                log_error "mkfs failed on ${RW_DEVS[$i]}:"
                echo "$mkfs_out" | sed 's/^/    /' >&2
                return 1
            fi
            mount "${RW_DEVS[$i]}" "$mp" || { log_error "mount ${RW_DEVS[$i]} -> $mp failed"; return 1; }
            if [[ "$suffix" == "A" ]]; then
                MOUNTS_A+=("$mp")
            else
                MOUNTS_B+=("$mp")
            fi
        done
    done
}

# ---------------------------------------------------------------------------
# Per-initiator op loop. Each runs in a subshell against its own mount
# set; the model state is local to the subshell.
# Args: suffix, pipe-separated mount-point list, ops count, log path, seed.
# ---------------------------------------------------------------------------
run_initiator_ops() {
    local suffix="$1" mounts_csv="$2" n_ops="$3" ops_log="$4" seed="$5"
    local -a MOUNTS
    IFS='|' read -ra MOUNTS <<< "$mounts_csv"

    MC_SEED="$seed"
    MC_OP_LOG="$ops_log"
    : > "$MC_OP_LOG"
    mc_op_stats_init

    declare -A FILE_VERSIONS
    declare -A FILE_SIZES
    declare -A CONTENT_KEY
    declare -a ALIVE_PATHS=()
    local NEXT_PATH_INDEX=1

    pick_mount() {
        echo "${MOUNTS[$(mc_rng_u32 "mount-$suffix" "${#MOUNTS[@]}")]}"
    }
    new_path_local() {
        NEXT_PATH_INDEX=$(( NEXT_PATH_INDEX + 1 ))
        printf '%s/f-%05d' "$(pick_mount)" "$NEXT_PATH_INDEX"
    }
    pick_existing() {
        local n=${#ALIVE_PATHS[@]}
        (( n > 0 )) || return 1
        echo "${ALIVE_PATHS[$(mc_rng_u32 "pick-$suffix" "$n")]}"
    }
    regen_local() {
        local path="$1" version="$2" size="$3" out="$4"
        local key="${CONTENT_KEY[$path]:-$path}"
        mc_content_to "$key" "$version" "$size" "$out"
    }

    op_write_new() {
        local path; path=$(new_path_local)
        CONTENT_KEY[$path]="$path"
        local size; size=$(mc_pick_size_boundary_biased "size-write-$suffix")
        (( size > 4194304 )) && size=4194304
        local tmp="$TEST_DIR/scratch-$suffix"
        regen_local "$path" 1 "$size" "$tmp"
        if ! cp "$tmp" "$path" 2>/dev/null; then
            local victim; victim=$(pick_existing) || { unset 'CONTENT_KEY[$path]'; mc_log_op write_new init="$suffix" status=enospc_no_victim; return 0; }
            rm -f "$victim"; FILE_VERSIONS[$victim]=0; unset 'FILE_SIZES[$victim]'; unset 'CONTENT_KEY[$victim]'
            local new=() p; for p in "${ALIVE_PATHS[@]}"; do (( ${FILE_VERSIONS[$p]:-0} > 0 )) && new+=("$p"); done; ALIVE_PATHS=("${new[@]}")
            if ! cp "$tmp" "$path" 2>/dev/null; then unset 'CONTENT_KEY[$path]'; mc_log_op write_new init="$suffix" status=enospc_after_evict; return 0; fi
        fi
        FILE_VERSIONS[$path]=1; FILE_SIZES[$path]=$size; ALIVE_PATHS+=("$path")
        mc_log_op write_new init="$suffix" path="$path" size="$size" v=1
    }
    op_overwrite() {
        local path; path=$(pick_existing) || { op_write_new; return $?; }
        local v=${FILE_VERSIONS[$path]} new_v=$(( v + 1 ))
        local size; size=$(mc_pick_size_boundary_biased "size-overwrite-$suffix")
        (( size > 4194304 )) && size=4194304
        local tmp="$TEST_DIR/scratch-$suffix"
        regen_local "$path" "$new_v" "$size" "$tmp"
        if ! cp "$tmp" "$path" 2>/dev/null; then
            mc_log_op overwrite init="$suffix" path="$path" size="$size" v="$new_v" status=enospc
            return 0
        fi
        FILE_VERSIONS[$path]=$new_v; FILE_SIZES[$path]=$size
        mc_log_op overwrite init="$suffix" path="$path" size="$size" v="$new_v"
    }
    op_read_verify() {
        local path; path=$(pick_existing) || { mc_log_op read_verify init="$suffix" status=no_files; return 0; }
        local v=${FILE_VERSIONS[$path]} size=${FILE_SIZES[$path]}
        local actual_size; actual_size=$(stat -c%s "$path" 2>/dev/null || echo missing)
        if [[ "$actual_size" != "$size" ]]; then
            log_error "init $suffix verify: size mismatch at $path model=$size actual=$actual_size"
            return 1
        fi
        local tmp="$TEST_DIR/scratch-$suffix.expect"
        regen_local "$path" "$v" "$size" "$tmp"
        if ! cmp -s "$path" "$tmp"; then
            log_error "init $suffix verify: content mismatch at $path v=$v size=$size"
            return 1
        fi
        mc_log_op read_verify init="$suffix" path="$path" size="$size" v="$v"
    }
    op_delete() {
        local path; path=$(pick_existing) || { mc_log_op delete init="$suffix" status=no_files; return 0; }
        rm -f "$path" 2>/dev/null || { log_error "init $suffix delete: rm failed at $path"; return 1; }
        FILE_VERSIONS[$path]=0; unset 'FILE_SIZES[$path]'; unset 'CONTENT_KEY[$path]'
        local new=() p; for p in "${ALIVE_PATHS[@]}"; do (( ${FILE_VERSIONS[$p]:-0} > 0 )) && new+=("$p"); done; ALIVE_PATHS=("${new[@]}")
        mc_log_op delete init="$suffix" path="$path"
    }
    op_sync() { sync; mc_log_op sync init="$suffix"; }

    local WEIGHTS=("28:write_new" "18:overwrite" "38:read_verify" "10:delete" "6:sync")
    mc_assert_weights "op-$suffix" "${WEIGHTS[@]}"

    local progress_every=$(( n_ops / 10 )); (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n_ops; MC_OP_INDEX++ )); do
        local op
        op=$(mc_pick_weighted "op-$suffix" "${WEIGHTS[@]}")
        case "$op" in
            write_new)   op_write_new || exit 1 ;;
            overwrite)   op_overwrite || exit 1 ;;
            read_verify) op_read_verify || exit 1 ;;
            delete)      op_delete || exit 1 ;;
            sync)        op_sync || exit 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            log_info "  [init $suffix $MC_OP_INDEX/$n_ops] alive=${#ALIVE_PATHS[@]}"
        fi
    done
    mc_op_stats_dump
    exit 0
}

main() {
    echo "========================================"
    echo "thurvsa Monte Carlo Multi-Initiator Test"
    echo "========================================"
    echo ""
    check_prerequisites
    assign_ports
    create_test_config
    start_daemon_mc
    create_volumes
    iscsi_login_both
    setup_initiator_devices A "$IFACE_A" || exit 1
    setup_initiator_devices B "$IFACE_B" || exit 1
    declare -a MOUNTS_A=() MOUNTS_B=()
    mkfs_and_mount_all || exit 1

    mc_seed_init "$SEED" "$TEST_DIR/ops.log"
    log_info "Running $OPS ops/initiator concurrently (4 volumes, 2/initiator)"

    local ma mb seed_a seed_b
    ma=$(IFS='|'; echo "${MOUNTS_A[*]}")
    mb=$(IFS='|'; echo "${MOUNTS_B[*]}")
    seed_a=$(( MC_SEED + 1 ))
    seed_b=$(( MC_SEED + 2 ))

    ( run_initiator_ops A "$ma" "$OPS" "$TEST_DIR/ops-a.log" "$seed_a" ) &
    local pid_a=$!
    ( run_initiator_ops B "$mb" "$OPS" "$TEST_DIR/ops-b.log" "$seed_b" ) &
    local pid_b=$!

    local rc_a=0 rc_b=0
    wait "$pid_a" || rc_a=$?
    wait "$pid_b" || rc_b=$?

    if (( rc_a != 0 || rc_b != 0 )); then
        log_fail "Concurrent op loop failed (A=$rc_a B=$rc_b)"
        log_info "Op logs: $TEST_DIR/ops-{a,b}.log"
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
