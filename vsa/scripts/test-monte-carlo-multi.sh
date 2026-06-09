#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa Monte Carlo Multi-Initiator Test
#
# Sibling to vsa/scripts/test-monte-carlo.sh. Spins up a single
# thurvsad daemon serving 4 volumes (vol-mc-a1, vol-mc-a2,
# vol-mc-b1, vol-mc-b2), and runs two concurrent op streams from two
# distinct initiators against their own pair of volumes:
#
#   Initiator A: vol-mc-a1, vol-mc-a2  -> mnt-a-{a1,a2}
#   Initiator B: vol-mc-b1, vol-mc-b2  -> mnt-b-{b1,b2}
#
# Transport selectable via --transport iscsi|nvmetcp (default iscsi).
# Op generator, content model, and verification are
# transport-agnostic — only login / device-discovery / logout branch.
#
# --transport iscsi:
#   Two distinct initiator IQNs over per-`iface` iscsiadm sessions.
#   Per-CHAP-user volume admission (`iscsi users add USER --volume
#   ...`) fences each session to its 2 admitted LUNs. Without
#   admission, the kernel's post-login LUN probe across all 4
#   devices races and mkfs.ext4 refuses with "device is apparently
#   in use by the system" even under -F; admission makes the probe
#   race disappear by keeping each session blind to the other's
#   LUNs.
#
# --transport nvmetcp:
#   Two distinct host NQNs over plaintext NVMe/TCP. Each `nvme
#   connect` creates its own controller (nvme0, nvme1, ...) with
#   its own /dev/nvmeXn<NSID> tree, so the kernel-claim-race that
#   afflicts iSCSI doesn't apply — multi-controller separation is
#   what isolates the device trees, not admission. Both controllers
#   see all 4 namespaces; the harness application-layer-isolates by
#   only touching its assigned pair. (TLS-PSK + per-host admission
#   is exercised by unit tests + the iSCSI variant here — wiring it
#   in this script needs `tlshd` on the host and is a follow-up.)
#
# Op mix: write_new / overwrite / read_verify / delete / sync.
# Directory ops + write_at_offset + restart are skipped here — the
# single-initiator harness covers those; this one focuses on
# concurrent dispatch.
#
# Prerequisites:
#   - e2fsprogs, util-linux, openssl
#   - --transport iscsi:   open-iscsi + iscsid + lsscsi
#   - --transport nvmetcp: nvme-cli + linux-kernel nvme_tcp module
#   - Root/sudo access
#
# Usage:
#   ./vsa/scripts/test-monte-carlo-multi.sh [OPTIONS]
#
# Options:
#   --transport T         iscsi (default) or nvmetcp
#   --seed N              Reproduce a prior run (default: pick from /dev/urandom)
#   --quick               150 ops/initiator (default: 1000 ops/initiator)
#   --ops N               Override per-initiator op count
#   --debug               Use ./target/debug/ binaries (default: ./target/release/)
#   --daemon-path PATH    Override thurvsad path
#   --cli-path PATH       Override thurvsa path
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port  (iscsi transport)
#   --nvmetcp-port PORT   Override NVMe/TCP port (nvmetcp transport)
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
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
TRANSPORT="iscsi"
SEED=""
QUICK=0
OPS=""

VOLUME_NAMES_A=("vol-mc-a1" "vol-mc-a2")
VOLUME_NAMES_B=("vol-mc-b1" "vol-mc-b2")
VOLUME_SIZE_MIB=512
# At-rest encryption embedded in the default run: one volume per
# initiator (vol-mc-a2 / vol-mc-b2) is created --encrypt --keystore
# local, the other plaintext, so each concurrent stream sweeps both
# data paths under contention. No flag needed.
ENCRYPTED_VOLUMES=("vol-mc-a2" "vol-mc-b2")

# iSCSI-only identity (one iface + one CHAP user per initiator).
INIT_IQN_A="iqn.2025-10.com.metebalci:vsa-mc-a"
INIT_IQN_B="iqn.2025-10.com.metebalci:vsa-mc-b"
IFACE_A="thurvsa-mc-a"
IFACE_B="thurvsa-mc-b"
CHAP_USER_A="mc-user-a-$$"
CHAP_USER_B="mc-user-b-$$"
CHAP_PASS_A="mc-secret-a-$(od -An -N12 -tx8 /dev/urandom | tr -d ' \n')"
CHAP_PASS_B="mc-secret-b-$(od -An -N12 -tx8 /dev/urandom | tr -d ' \n')"

# NVMe-TCP-only identity (one host NQN per initiator).
HOST_NQN_A="nqn.2025-10.com.metebalci:host-mc-a"
HOST_NQN_B="nqn.2025-10.com.metebalci:host-mc-b"
# Resolved post-`nvme connect` via list-subsys lookup.
NVME_CTRL_A=""
NVME_CTRL_B=""

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --transport) TRANSPORT="$2"; shift 2 ;;
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
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

case "$TRANSPORT" in
    iscsi|nvmetcp) ;;
    *) echo "Unknown --transport '$TRANSPORT' (expected iscsi or nvmetcp)" >&2; exit 1 ;;
esac

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
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" -I "$IFACE_A" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" -I "$IFACE_B" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" --op delete >/dev/null 2>&1 || true
        iscsiadm -m iface -I "$IFACE_A" -o delete >/dev/null 2>&1 || true
        iscsiadm -m iface -I "$IFACE_B" -o delete >/dev/null 2>&1 || true
    else
        # `nvme disconnect -n SUBNQN` tears every controller bound to
        # the subsystem in one shot — drops both initiator A and B.
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
        sleep 1
    fi
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
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE, transport: $TRANSPORT)..."
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"
    local tool missing=()
    local base_tools=(mkfs.ext4 mount umount openssl curl cmp systemctl)
    for tool in "${base_tools[@]}"; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        for tool in iscsiadm lsscsi; do
            command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
        done
    else
        command -v nvme >/dev/null 2>&1 || missing+=("nvme (nvme-cli)")
    fi
    [[ -x "$DAEMON_PATH" ]] || missing+=("thurvsad at $DAEMON_PATH")
    [[ -x "$CLI_PATH" ]] || missing+=("thurvsa at $CLI_PATH")
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        exit 1
    fi
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null \
                && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running."
            exit 1
        fi
    else
        # Linux nvme_tcp module is auto-loaded by `nvme connect`, but
        # the kernel must support it.
        if ! modprobe nvme_tcp 2>/dev/null && ! lsmod | grep -q '^nvme_tcp\b'; then
            log_error "kernel module nvme_tcp is not available"
            exit 1
        fi
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

# Mirror test-helpers.sh assign_ports but parameterised on TRANSPORT.
assign_ports_mc() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        [[ -z "$ISCSI_PORT" ]] && ISCSI_PORT=$(pick_free_port)
    else
        [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(pick_free_port)
    fi
    [[ -z "$HTTP_PORT" ]] && HTTP_PORT=$(pick_free_port)
    log_info "Using $TRANSPORT port $(_transport_port), HTTP port $HTTP_PORT"
}

# Wait on whichever port the transport binds.
_transport_port() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then echo "$ISCSI_PORT"; else echo "$NVMETCP_PORT"; fi
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes"
    local transport_block
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        # CHAP is mandatory under VSA admission — without it, both
        # initiators would see all 4 LUNs and the kernel-claim-race
        # comes back.
        transport_block=$'iscsi:\n  listen: "127.0.0.1:'"$ISCSI_PORT"$'"\n  auth:\n    method: CHAP'
    else
        # Plaintext NVMe/TCP; admission is by-design see-everything
        # in plaintext (mirror of iSCSI no-CHAP). Multi-controller
        # device separation is what isolates the two initiators.
        transport_block=$'transports: [nvmetcp]\nnvmetcp:\n  listen: "127.0.0.1:'"$NVMETCP_PORT"$'"'
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
$transport_block
storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"
keystore:
  backends:
    local:
      type: local
EOFCONFIG
}

start_daemon_mc() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad ($TRANSPORT)..."
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local port
    port=$(_transport_port)
    local _
    for _ in {1..60}; do
        if ss -tln 2>/dev/null | grep -q ":$port\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $port)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon did not become ready"; tail -30 "${TEST_DIR}/daemon.log"; exit 1
}

create_volumes() {
    local name
    for name in "${VOLUME_NAMES_A[@]}" "${VOLUME_NAMES_B[@]}"; do
        local enc_args=() enc_note=""
        if printf '%s\n' "${ENCRYPTED_VOLUMES[@]}" | grep -qxF "$name"; then
            enc_args=(--encrypt --keystore local)
            enc_note=" (encrypted)"
        fi
        log_info "Creating volume $name (${VOLUME_SIZE_MIB} MiB)${enc_note}..."
        "$CLI_PATH" --config "$TEST_CONFIG" volume create "$name" \
            --size "${VOLUME_SIZE_MIB}M" --backend local "${enc_args[@]}" >/dev/null
    done
}

setup_chap_users() {
    log_info "Adding CHAP users with disjoint --volume sets..."
    "$CLI_PATH" --config "$TEST_CONFIG" iscsi users add "$CHAP_USER_A" \
        --password "$CHAP_PASS_A" \
        --volume "${VOLUME_NAMES_A[0]}" --volume "${VOLUME_NAMES_A[1]}" >/dev/null
    "$CLI_PATH" --config "$TEST_CONFIG" iscsi users add "$CHAP_USER_B" \
        --password "$CHAP_PASS_B" \
        --volume "${VOLUME_NAMES_B[0]}" --volume "${VOLUME_NAMES_B[1]}" >/dev/null
}

setup_iface() {
    local iface="$1" initiator_iqn="$2"
    iscsiadm -m iface -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$iface" -o update -n iface.initiatorname -v "$initiator_iqn" >/dev/null 2>&1
}

iscsi_login_iface() {
    local iface="$1" user="$2" pass="$3"
    # SendTargets discovery itself needs CHAP — stash creds on the
    # discoverydb entry first. iscsiadm keys discoverydb on the
    # (portal, iface) pair, so each iface gets its own creds.
    iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        -o update -n discovery.sendtargets.auth.authmethod -v CHAP >/dev/null 2>&1
    iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        -o update -n discovery.sendtargets.auth.username -v "$user" >/dev/null 2>&1
    iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        -o update -n discovery.sendtargets.auth.password -v "$pass" >/dev/null 2>&1
    iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -I "$iface" --discover >/dev/null 2>&1
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        --op update -n node.session.auth.authmethod -v CHAP >/dev/null 2>&1
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        --op update -n node.session.auth.username -v "$user" >/dev/null 2>&1
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" \
        --op update -n node.session.auth.password -v "$pass" >/dev/null 2>&1
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

setup_initiator_devices_iscsi() {
    local suffix="$1" iface="$2"
    local devs=() row
    local tries=0
    while (( tries < 10 )); do
        mapfile -t devs < <(resolve_initiator_devices "$iface")
        # With per-CHAP-user admission, each session only sees its 2
        # admitted volumes — not all 4.
        (( ${#devs[@]} >= 2 )) && break
        sleep 1
        tries=$(( tries + 1 ))
    done
    if (( ${#devs[@]} < 2 )); then
        log_error "initiator $suffix: only ${#devs[@]} THUR VSA devices visible (expected 2)"
        return 1
    fi
    if (( ${#devs[@]} > 2 )); then
        log_error "initiator $suffix: ${#devs[@]} THUR VSA devices visible (expected 2 — admission leak?)"
        return 1
    fi
    # REPORT LUNS is filtered by admission, so each session sees only
    # the 2 LUNs belonging to its admitted volume set. lsscsi sorts by
    # [H:C:I:L], so devs[0]/[1] are the lower/higher LUN of the
    # admitted pair:
    #   A: vol-mc-a1 (LUN 0) -> devs[0], vol-mc-a2 (LUN 1) -> devs[1]
    #   B: vol-mc-b1 (LUN 2) -> devs[0], vol-mc-b2 (LUN 3) -> devs[1]
    if [[ "$suffix" == "A" ]]; then
        eval "RW_DEVS_A=(\"${devs[0]}\" \"${devs[1]}\")"
    else
        eval "RW_DEVS_B=(\"${devs[0]}\" \"${devs[1]}\")"
    fi
    log_info "initiator $suffix: devices=(${devs[*]})"
}

# NVMe/TCP plaintext: both controllers see all 4 NSIDs. NSIDs are
# stable (nsid = lun + 1, and volumes register in alphabetical
# boot-time order): vol-mc-a1=1, vol-mc-a2=2, vol-mc-b1=3, vol-mc-b2=4.
# Application-layer isolation: A touches NSIDs 1+2, B touches 3+4.
setup_initiator_devices_nvme() {
    local suffix="$1" host_nqn="$2"
    local ctrl=""
    local tries=0
    while (( tries < 10 )); do
        ctrl=$(_nvme_ctrl_for_hostnqn "$host_nqn") && break
        sleep 0.5
        tries=$(( tries + 1 ))
    done
    if [[ -z "$ctrl" ]]; then
        log_error "initiator $suffix: no nvme controller found for hostnqn=$host_nqn"
        return 1
    fi
    local n1 n2
    if [[ "$suffix" == "A" ]]; then
        n1="/dev/${ctrl}n1"; n2="/dev/${ctrl}n2"
        NVME_CTRL_A="$ctrl"
    else
        n1="/dev/${ctrl}n3"; n2="/dev/${ctrl}n4"
        NVME_CTRL_B="$ctrl"
    fi
    # Wait for the namespace block devices to materialise.
    local dev
    for dev in "$n1" "$n2"; do
        local t=0
        while (( t < 10 )); do
            [[ -b "$dev" ]] && break
            sleep 0.5
            t=$(( t + 1 ))
        done
        [[ -b "$dev" ]] || { log_error "initiator $suffix: $dev did not appear"; return 1; }
    done
    if [[ "$suffix" == "A" ]]; then
        RW_DEVS_A=("$n1" "$n2")
    else
        RW_DEVS_B=("$n1" "$n2")
    fi
    log_info "initiator $suffix: controller=$ctrl devices=($n1 $n2)"
}

# Branch on TRANSPORT. iSCSI uses the per-iface SCSI device tree;
# NVMe-TCP uses the per-controller NSID tree.
setup_initiator_devices() {
    local suffix="$1"
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        local iface
        iface=$([[ "$suffix" == "A" ]] && echo "$IFACE_A" || echo "$IFACE_B")
        setup_initiator_devices_iscsi "$suffix" "$iface"
    else
        local host
        host=$([[ "$suffix" == "A" ]] && echo "$HOST_NQN_A" || echo "$HOST_NQN_B")
        setup_initiator_devices_nvme "$suffix" "$host"
    fi
}

iscsi_login_both() {
    setup_iface "$IFACE_A" "$INIT_IQN_A"
    setup_iface "$IFACE_B" "$INIT_IQN_B"
    iscsi_login_iface "$IFACE_A" "$CHAP_USER_A" "$CHAP_PASS_A" \
        || { log_error "iscsi login (A) failed"; return 1; }
    iscsi_login_iface "$IFACE_B" "$CHAP_USER_B" "$CHAP_PASS_B" \
        || { log_error "iscsi login (B) failed"; return 1; }
    sleep 4
}

# -----------------------------------------------------------------------
# NVMe/TCP-specific helpers. Plaintext (no TLS-PSK), one `nvme connect`
# per initiator with a distinct --hostnqn. Each connect produces its own
# controller node under /sys/class/nvme; we read the hostnqn sysfs file
# to identify which nvme<N> belongs to A and which to B.
# -----------------------------------------------------------------------

# Resolve the nvme<N> controller name for a given hostnqn by walking
# /sys/class/nvme/nvme*/hostnqn. Returns the basename ("nvme0") or
# empty on miss.
_nvme_ctrl_for_hostnqn() {
    local want="$1" ctrl
    for ctrl in /sys/class/nvme/nvme*; do
        [[ -d "$ctrl" ]] || continue
        local got
        got=$(cat "$ctrl/hostnqn" 2>/dev/null | tr -d '\n')
        [[ "$got" == "$want" ]] || continue
        # Also confirm this controller is bound to our subsystem
        # (skip stale entries from other tests on the same box).
        local sub
        sub=$(cat "$ctrl/subsysnqn" 2>/dev/null | tr -d '\n')
        [[ "$sub" == "$SUBNQN" ]] || continue
        basename "$ctrl"
        return 0
    done
    return 1
}

nvme_login_host() {
    local host_nqn="$1"
    nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
        -n "$SUBNQN" --hostnqn "$host_nqn" \
        > "${TEST_DIR}/nvme-connect-${host_nqn##*:}.log" 2>&1
}

nvme_login_both() {
    nvme_login_host "$HOST_NQN_A" \
        || { log_error "nvme connect (A) failed"; cat "${TEST_DIR}/nvme-connect-host-mc-a.log" >&2; return 1; }
    nvme_login_host "$HOST_NQN_B" \
        || { log_error "nvme connect (B) failed"; cat "${TEST_DIR}/nvme-connect-host-mc-b.log" >&2; return 1; }
    # Both controllers should be visible in sysfs by the time
    # `nvme connect` returns, but give the udev rule a tick to
    # populate /dev/nvmeXn<NSID> nodes.
    sleep 2
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

    # Final sweep: re-verify every alive file end-to-end. The in-loop
    # read_verify is random, so files written late may never have been
    # re-read; this matches the single-initiator harness's final pass.
    local fp fv fsize ftmp="$TEST_DIR/scratch-$suffix.final" fchecked=0
    for fp in "${ALIVE_PATHS[@]}"; do
        (( ${FILE_VERSIONS[$fp]:-0} > 0 )) || continue
        fv=${FILE_VERSIONS[$fp]}; fsize=${FILE_SIZES[$fp]}
        regen_local "$fp" "$fv" "$fsize" "$ftmp"
        if ! cmp -s "$fp" "$ftmp"; then
            log_error "init $suffix final_verify: content mismatch at $fp v=$fv size=$fsize"
            exit 1
        fi
        fchecked=$(( fchecked + 1 ))
    done
    log_info "  [init $suffix] final verify OK ($fchecked files)"
    mc_op_stats_dump
    exit 0
}

# The shared LUN's /dev/sdX for an iface = the device that appeared
# after the grant+rescan that isn't in the initiator's file-op set.
iface_new_device() {
    local iface="$1"; shift
    local d
    while IFS= read -r d; do
        [[ -n "$d" ]] || continue
        [[ " $* " == *" $d "* ]] && continue
        echo "$d"; return 0
    done < <(resolve_initiator_devices "$iface")
    return 1
}

# Persistent-Reservation fuzz (iSCSI only; soft-skips without sg3-utils).
# After the concurrent file-op phase, attach a shared *raw* LUN to both
# initiators (dynamic `iscsi users grant` + rescan — the same
# REPORTED LUNS DATA CHANGED path the CSI driver uses) and run a seeded
# sequence of PR rounds: A registers + reserves a random type, an
# *unregistered* B's WRITE must come back RESERVATION CONFLICT (true for
# every PR type when B isn't a registrant), then a coin-flip either has
# B preempt A or A release — after which B's WRITE must succeed. The
# dedicated test-multi-initiator.sh covers one fixed sequence; this adds
# randomized types + preempt-vs-release ordering on top.
run_pr_fuzz() {
    [[ "$TRANSPORT" != "iscsi" ]] && { log_info "PR fuzz: skipped (nvmetcp — see test-nvmetcp-multi-initiator.sh)"; return 0; }
    if ! command -v sg_persist >/dev/null 2>&1 || ! command -v sg_write_same >/dev/null 2>&1; then
        log_warn "PR fuzz: sg3-utils (sg_persist/sg_write_same) missing — skipped"
        return 0
    fi
    log_info "PR fuzz: attaching shared raw LUN to both initiators..."
    local shared="vol-mc-shared"
    if ! "$CLI_PATH" --config "$TEST_CONFIG" volume create "$shared" --size 64M --backend local >/dev/null 2>&1; then
        log_error "PR fuzz: create $shared failed"; return 1
    fi
    "$CLI_PATH" --config "$TEST_CONFIG" iscsi users grant "$CHAP_USER_A" --volume "$shared" >/dev/null 2>&1
    "$CLI_PATH" --config "$TEST_CONFIG" iscsi users grant "$CHAP_USER_B" --volume "$shared" >/dev/null 2>&1
    iscsiadm -m session --rescan >/dev/null 2>&1
    sleep 2
    local dev_a dev_b
    dev_a=$(iface_new_device "$IFACE_A" "${RW_DEVS_A[@]}")
    dev_b=$(iface_new_device "$IFACE_B" "${RW_DEVS_B[@]}")
    if [[ -z "$dev_a" || -z "$dev_b" ]]; then
        log_error "PR fuzz: shared LUN device not found (A=${dev_a:-none} B=${dev_b:-none})"
        return 1
    fi
    log_info "PR fuzz: shared dev A=$dev_a B=$dev_b"
    local types=(1 3 5 6) rounds rc=0 r
    rounds=$(( $(mc_rng_u32 "pr-rounds" 3) + 2 ))   # 2..4
    for (( r=0; r<rounds; r++ )); do
        local rtype="${types[$(mc_rng_u32 "pr-type-$r" 4)]}"
        sg_persist --out --clear --param-rk=0xa1a1 "$dev_a" >/dev/null 2>&1 || true
        if ! sg_persist --out --register --param-sark=0xa1a1 "$dev_a" >/dev/null 2>&1; then
            log_error "PR fuzz: A register failed (round $r)"; rc=1; break
        fi
        if ! sg_persist --out --reserve --param-rk=0xa1a1 --prout-type="$rtype" "$dev_a" >/dev/null 2>&1; then
            log_error "PR fuzz: A reserve type=$rtype failed (round $r)"; rc=1; break
        fi
        # Unregistered B's write must conflict under every type.
        local out
        out=$(sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" 2>&1 || true)
        if ! echo "$out" | grep -qiE "Reservation conflict|reservation_conflict|sense.*0x18"; then
            log_error "PR fuzz: B write NOT blocked while A holds type=$rtype (round $r)"
            echo "$out" | head -3 | sed 's/^/    /' >&2
            rc=1; break
        fi
        local flip=$(mc_rng_u32 "pr-flip-$r" 2)
        if (( flip == 0 )); then
            sg_persist --out --register --param-sark=0xb2b2 "$dev_b" >/dev/null 2>&1
            if ! sg_persist --out --preempt --param-rk=0xb2b2 --param-sark=0xa1a1 --prout-type="$rtype" "$dev_b" >/dev/null 2>&1; then
                log_error "PR fuzz: B preempt failed (round $r type=$rtype)"; rc=1; break
            fi
            if ! sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" >/dev/null 2>&1; then
                log_error "PR fuzz: B write failed after preempt (round $r)"; rc=1; break
            fi
            sg_persist --out --clear --param-rk=0xb2b2 "$dev_b" >/dev/null 2>&1 || true
        else
            if ! sg_persist --out --release --param-rk=0xa1a1 --prout-type="$rtype" "$dev_a" >/dev/null 2>&1; then
                log_error "PR fuzz: A release failed (round $r type=$rtype)"; rc=1; break
            fi
            if ! sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" >/dev/null 2>&1; then
                log_error "PR fuzz: B write failed after A released (round $r)"; rc=1; break
            fi
            sg_persist --out --clear --param-rk=0xa1a1 "$dev_a" >/dev/null 2>&1 || true
        fi
        mc_log_op pr_round rtype="$rtype" resolve="$( (( flip == 0 )) && echo preempt || echo release )"
    done
    sg_persist --out --clear --param-rk=0xa1a1 "$dev_a" >/dev/null 2>&1 || true
    sg_persist --out --clear --param-rk=0xb2b2 "$dev_b" >/dev/null 2>&1 || true
    # The shared LUN is left for the cleanup trap (iSCSI logout + rm -rf)
    # to reclaim: an in-test `volume destroy` of a volume still mapped to
    # two live sessions can block on those sessions, and we don't need it
    # gone mid-run. Run AFTER the integrity gates so `system verify`
    # never sees this raw, FS-less volume.
    if (( rc != 0 )); then return 1; fi
    log_info "PR fuzz OK ($rounds rounds)"
}

# Post-run integrity gates against the state both concurrent streams
# built: pool/page-table + storage consistency, the BLAKE3 audit chain,
# and a stats dump (dedup ratio > 1 with the dup-corpus content class).
run_integrity_gates() {
    log_info "Integrity gates: system gc + verify + audit verify + stats..."
    local out
    "$CLI_PATH" --config "$TEST_CONFIG" system gc >/dev/null 2>&1 || true
    if ! out=$("$CLI_PATH" --config "$TEST_CONFIG" system verify 2>&1); then
        log_error "integrity: system verify failed:"
        echo "$out" | tail -20 >&2
        return 1
    fi
    # audit verify exit codes: 0 valid, 1 break, 2 IO, 3 plain-mode.
    "$CLI_PATH" --config "$TEST_CONFIG" system audit verify >/dev/null 2>&1
    local arc=$?
    if (( arc == 1 || arc == 2 )); then
        log_error "integrity: audit chain verify failed (rc=$arc)"
        return 1
    fi
    "$CLI_PATH" --config "$TEST_CONFIG" system stats 2>&1 | sed 's/^/  stats: /' || true
    log_info "Integrity gates OK (audit verify rc=$arc)"
}

main() {
    echo "========================================"
    echo "thurvsa Monte Carlo Multi-Initiator Test"
    echo "========================================"
    echo ""
    check_prerequisites
    assign_ports_mc
    create_test_config
    start_daemon_mc
    create_volumes
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        setup_chap_users
        iscsi_login_both
    else
        nvme_login_both
    fi
    setup_initiator_devices A || exit 1
    setup_initiator_devices B || exit 1
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

    if ! run_integrity_gates; then
        log_fail "Integrity gates failed (gc/verify/audit)"
        exit 1
    fi

    if ! run_pr_fuzz; then
        log_fail "Persistent-reservation fuzz failed"
        exit 1
    fi

    if ! mc_assert_daemon_healthy "${TEST_DIR}/daemon.log" "${DAEMON_PID:-}"; then
        log_fail "Daemon health check failed (crash or panic)"
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
