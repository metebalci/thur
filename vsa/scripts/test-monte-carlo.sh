#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa Monte Carlo Random-Op Test
#
# Runs N seeded random operations against an ext4 filesystem mounted on
# a thurvsa volume. The transport (iSCSI or NVMe/TCP) is selectable via
# --transport; the op generator, content model, and verification are
# transport-agnostic — only the login / device-discovery / logout-cycle
# primitives branch. Op mix is weighted to bias file ops and sample
# transport / mount churn at lower rates — `umount_cycle` and
# `transport_logout_cycle` tear the lower layer down; the next file op
# lazily brings it back. That exposes the daemon to "user opens session,
# does ops, logs out, comes back later, does more ops" workloads that
# the deterministic scripted tests don't reach.
#
# Size distribution is boundary-biased: most random tests draw uniformly
# and end up clustered in "multi-chunk" sizes, never touching the
# interesting <page / page-boundary / exactly-one-chunk cases. We pick
# from buckets that span sub-sector (1 B .. 4 KiB), sub-page (4 KiB ..
# 64 KiB), 1-4 pages, many-chunks, and a small (5%) tail of multi-MiB.
#
# Content model: every file's bytes are AES-CTR keystream of a key
# derived from (seed, path, version). Because CTR is a keystream, the
# bytes at [0..N] for size N are the same prefix as for any larger
# size at the same version. That means:
#   - append never bumps version: extending the file to a new total
#     size matches the regenerated stream up to the old length, and
#     the new tail bytes are just the next slice of the same stream.
#   - truncate never bumps version: shrinking is a prefix of the same
#     stream.
#   - overwrite bumps version: full replacement under a new keystream.
# read_verify regenerates the expected content into /tmp and `cmp`s
# against the mounted file. On any mismatch the harness dumps the seed
# and the last 50 ops so the failure is reproducible via --seed.
#
# Backend selection: defaults to an inline local backend. Set
# THURVSA_TEST_BACKEND=<name> (or --backend <name>) to pick an entry
# from a backends YAML (defaulting to private/storage-backends.yaml,
# override via THURVSA_SOURCE_BACKENDS). The named backend's `prefix`
# is overridden per-run so test data is isolated and purged on cleanup.
#
# Prerequisites:
#   - e2fsprogs, util-linux, openssl
#   - iSCSI mode:  sg3-utils, open-iscsi, lsscsi; iscsid running
#                  (sudo systemctl enable --now iscsid)
#   - NVMe/TCP mode: nvme-cli; nvme_tcp kernel module
#                    (sudo modprobe nvme_tcp)
#   - Root/sudo access
#   - For non-local backends: yq, the matching backend CLI, valid credentials
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-monte-carlo.sh [OPTIONS]
#
# Self-elevates via sudo (NOPASSWD sudoers entry required).
#
# Options:
#   --seed N              Reproduce a prior run (default: pick from /dev/urandom)
#   --quick               200 ops, ~30 MB residual (default: 3000 ops, ~500 MB)
#   --ops N               Override op count
#   --transport T         iscsi (default) or nvmetcp
#   --backend NAME        Use named backend entry (same as THURVSA_TEST_BACKEND)
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvsad path
#   --cli-path PATH       Override thurvsa path
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (iscsi mode only)
#   --nvmetcp-port PORT   Override NVMe/TCP port (nvmetcp mode only)
#   --http-port PORT      Override HTTP port
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials before self-elevation
# so they're in scope to forward across sudo. Same convention as
# test-fs-iscsi-storage.sh.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding backend-relevant env vars as
# explicit KEY=VAL pairs. `sudo -E` is silently ignored on sudo-rs
# (Ubuntu 26.04+); explicit forwarding is the only portable path.
if [[ $EUID -ne 0 ]]; then
    forward=()
    for v in $(compgen -A variable); do
        case "$v" in
            AWS_*|GOOGLE_*|GCS_*|AZURE_*|AISTOR_*|WASABI_*|MINIO_*|THURVSA_*)
                [[ -n "${!v}" ]] && forward+=("$v=${!v}")
                ;;
        esac
    done
    echo "[INFO] Re-executing under sudo with ${#forward[@]} env vars forwarded..."
    exec sudo "${forward[@]}" "$0" "$@"
fi

source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/thurvsa-monte-carlo-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TRANSPORT="iscsi"
ISCSI_PORT=""
NVMETCP_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-monte-carlo-test"
KEEP_DATA=0
DAEMON_PID=""
ISCSI_CONNECTED=0
NVME_CONNECTED=0
NVME_DEVICE=""
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-mc"
VOLUME_SIZE_MIB=1024
SEED=""
QUICK=0
OPS=""
BACKEND_NAME="${THURVSA_TEST_BACKEND:-}"
SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
BACKEND_TYPE=""
TEST_PREFIX=""
RW_DEVICE=""
RW_SG_DEVICE=""

# Mount/transport lazy state. Both start "down" and the first file op
# brings them up.
TRANSPORT_UP=0
MOUNT_UP=0

# In-memory file model. FILE_VERSIONS[path]=int (0 = deleted/never-existed),
# FILE_SIZES[path]=bytes. ALIVE_PATHS is the index for "pick an existing
# file", rebuilt on delete.
declare -A FILE_VERSIONS
declare -A FILE_SIZES
declare -a ALIVE_PATHS
NEXT_PATH_INDEX=1

while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --ops) OPS="$2"; shift 2 ;;
        --transport) TRANSPORT="$2"; shift 2 ;;
        --backend) BACKEND_NAME="$2"; shift 2 ;;
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ $QUICK -eq 1 ]]; then
    : "${OPS:=200}"
    VOLUME_SIZE_MIB=128
else
    : "${OPS:=3000}"
fi

case "$TRANSPORT" in
    iscsi|nvmetcp) ;;
    *) echo "Unknown --transport '$TRANSPORT' (expected iscsi or nvmetcp)"; exit 1 ;;
esac

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi

    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
    fi
    if [[ $NVME_CONNECTED -eq 1 ]]; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    fi

    stop_thur_daemon

    # Purge backend test prefix if we wrote to a real backend.
    if [[ -n "$BACKEND_NAME" && "$BACKEND_TYPE" != "local" && -n "$TEST_PREFIX" ]]; then
        storage_purge_test_prefix 2>/dev/null || true
    fi

    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
        log_info "Op log: $TEST_DIR/ops.log"
    fi

    exit $rc
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE, transport: $TRANSPORT)..."
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
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [nvme]="sudo apt-get install nvme-cli"
        [mkfs.ext4]="sudo apt-get install e2fsprogs"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [openssl]="(usually present)"
        [curl]="sudo apt-get install curl"
        [systemctl]="(systemd — usually present)"
        [cmp]="(diffutils — usually present)"
    )
    local tools=(mkfs.ext4 mount umount openssl curl systemctl cmp)
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        tools+=(iscsiadm lsscsi)
    else
        tools+=(nvme)
    fi
    for tool in "${tools[@]}"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done
    if [[ -n "$BACKEND_NAME" ]] && ! command -v yq >/dev/null 2>&1; then
        missing+=("yq")
        hints+=("  - yq: sudo apt-get install yq  (needed to read $SOURCE_BACKENDS)")
    fi

    if [[ "$TRANSPORT" == "nvmetcp" ]]; then
        if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
            missing+=("nvme_tcp kernel module")
            hints+=("  - nvme_tcp: sudo modprobe nvme_tcp (kernel >= 5.0 required)")
        fi
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if [[ "$TRANSPORT" == "iscsi" ]]; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running."
            echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
            exit 1
        fi
    else
        if ! lsmod | grep -q '^nvme_tcp\b'; then
            log_info "Loading nvme_tcp kernel module"
            modprobe nvme_tcp || { log_error "Failed to load nvme_tcp"; exit 1; }
        fi
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

# Pick transport+http ports. Mirrors test-helpers.sh assign_ports but
# the transport port var name depends on TRANSPORT.
assign_ports_mc() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        [[ -z "$ISCSI_PORT" ]] && ISCSI_PORT=$(pick_free_port)
    else
        [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(pick_free_port)
    fi
    [[ -z "$HTTP_PORT" ]] && HTTP_PORT=$(pick_free_port)
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        log_info "Using iSCSI port $ISCSI_PORT, HTTP port $HTTP_PORT"
    else
        log_info "Using NVMe/TCP port $NVMETCP_PORT, HTTP port $HTTP_PORT"
    fi
}

# Build the test conffile. If BACKEND_NAME is empty, declare just an
# inline local backend. Otherwise pull the named entry from
# $SOURCE_BACKENDS, rewrite its prefix to TEST_PREFIX for cleanup
# isolation, and inject it.
create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes" "$MOUNT_POINT"
    local transport_block
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        transport_block=$'iscsi:\n  listen: "127.0.0.1:'"$ISCSI_PORT"'"'
    else
        transport_block=$'transport: nvmetcp\nnvmetcp:\n  listen: "0.0.0.0:'"$NVMETCP_PORT"'"'
    fi
    if [[ -z "$BACKEND_NAME" ]]; then
        BACKEND_NAME="local"
        BACKEND_TYPE="local"
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
EOFCONFIG
        log_info "Backend: inline local at $TEST_DIR/local-backend"
        return 0
    fi

    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Backend source YAML not found or unreadable: $SOURCE_BACKENDS"
        echo "Set THURVSA_SOURCE_BACKENDS=<path>/backends.yaml or use --backend without setting one."
        exit 1
    fi
    local exists
    exists=$(yq -r ".storage.backends.\"$BACKEND_NAME\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" ]]; then
        log_error "Backend '$BACKEND_NAME' not found in $SOURCE_BACKENDS"
        echo "Available:"
        yq -r '.storage.backends | keys[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi
    BACKEND_TYPE=$(yq -r ".storage.backends.\"$BACKEND_NAME\".type" "$SOURCE_BACKENDS")
    local retention
    retention=$(yq -r ".storage.backends.\"$BACKEND_NAME\".retention_mode // \"none\"" "$SOURCE_BACKENDS")
    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$BACKEND_NAME' has retention_mode='$retention' — refusing (test would create undeletable junk)."
        exit 1
    fi
    TEST_PREFIX="monte-carlo/run-$$/$(date +%s)/"

    # Globals for the storage_purge_test_prefix helper.
    BACKEND_BUCKET=$(yq -r ".storage.backends.\"$BACKEND_NAME\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$BACKEND_NAME\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(yq -r ".storage.backends.\"$BACKEND_NAME\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$BACKEND_NAME\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$BACKEND_NAME\".container // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_AKID_ENV=$(yq -r "(.storage.backends.\"$BACKEND_NAME\".auth.access_key_id_env // \"\")" "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(yq -r "(.storage.backends.\"$BACKEND_NAME\".auth.secret_access_key_env // \"\")" "$SOURCE_BACKENDS")

    local backend_yaml
    backend_yaml=$(yq -y \
        ".storage.backends.\"$BACKEND_NAME\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS" | sed 's/^/      /')

    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
$transport_block
storage:
  backends:
    $BACKEND_NAME:
$backend_yaml
EOFCONFIG
    log_info "Backend: $BACKEND_NAME (type=$BACKEND_TYPE, prefix=$TEST_PREFIX)"
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad ($TRANSPORT)..."
    local probe_port
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        probe_port="$ISCSI_PORT"
    else
        probe_port="$NVMETCP_PORT"
    fi
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..60}; do
        if ss -tln 2>/dev/null | grep -q ":$probe_port\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $probe_port)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

ensure_volume() {
    if "$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null | grep -q "$VOLUME_NAME"; then
        log_info "Volume $VOLUME_NAME already present"
        return 0
    fi
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB, backend=$BACKEND_NAME)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size "${VOLUME_SIZE_MIB}M" --backend "$BACKEND_NAME" >/dev/null
}

# Bring iSCSI session up + resolve /dev/sdN. Idempotent.
_iscsi_login() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 || true
    if ! iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1; then
        log_error "iscsi login failed"
        return 1
    fi
    ISCSI_CONNECTED=1
    sleep 2
    local row
    for _ in 1 2 3 4 5; do
        row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
        [[ -n "$row" ]] && break
        sleep 1
    done
    [[ -n "$row" ]] || { log_error "iscsi login OK but no THUR VSA device appeared"; lsscsi -g; return 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    RW_SG_DEVICE=$(echo "$row" | awk '{print $NF}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; return 1; }
}

_iscsi_logout() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete >/dev/null 2>&1 || true
    ISCSI_CONNECTED=0
    RW_SG_DEVICE=""
}

# Bring NVMe/TCP session up + resolve /dev/nvmeXn1. Idempotent.
_nvme_login() {
    if ! nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
            -n "$SUBNQN" --hostnqn "$HOST_NQN" \
            > "$TEST_DIR/nvme-connect.log" 2>&1; then
        log_error "nvme connect failed"
        cat "$TEST_DIR/nvme-connect.log"
        return 1
    fi
    NVME_CONNECTED=1
    NVME_DEVICE=$(nvme list-subsys -o json 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
target = '$SUBNQN'
def walk(d):
    if isinstance(d, dict):
        if d.get('NQN') == target:
            for p in d.get('Paths', []):
                if 'Name' in p:
                    return p['Name']
        for v in d.values():
            r = walk(v)
            if r:
                return r
    elif isinstance(d, list):
        for v in d:
            r = walk(v)
            if r:
                return r
    return None
name = walk(data)
print(name or '')
" 2>/dev/null)
    if [[ -z "$NVME_DEVICE" ]]; then
        NVME_DEVICE=$(ls -1 /dev/nvme*n1 2>/dev/null \
            | sort -V | tail -1 | xargs -n1 basename | sed 's/n1$//')
    fi
    [[ -n "$NVME_DEVICE" ]] || { log_error "could not locate the connected NVMe controller"; return 1; }
    RW_DEVICE="/dev/${NVME_DEVICE}n1"
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; return 1; }
}

_nvme_logout() {
    nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    NVME_CONNECTED=0
    NVME_DEVICE=""
    # Give the kernel a moment to tear down /dev/nvmeXn1.
    sleep 1
}

# Transport-agnostic wrappers. Idempotent.
transport_login() {
    if [[ $TRANSPORT_UP -eq 1 ]]; then return 0; fi
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        _iscsi_login || return 1
    else
        _nvme_login || return 1
    fi
    TRANSPORT_UP=1
}

transport_logout() {
    if [[ $TRANSPORT_UP -eq 0 ]]; then return 0; fi
    # Must umount before tearing the lower layer down.
    if [[ $MOUNT_UP -eq 1 ]]; then
        umount "$MOUNT_POINT" 2>/dev/null || true
        MOUNT_UP=0
    fi
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        _iscsi_logout
    else
        _nvme_logout
    fi
    TRANSPORT_UP=0
    RW_DEVICE=""
}

# mkfs on first call, mount on every call after. Idempotent on
# already-mounted.
EXT4_MADE=0
ensure_mounted() {
    transport_login || return 1
    if [[ $MOUNT_UP -eq 1 ]]; then return 0; fi
    if [[ $EXT4_MADE -eq 0 ]]; then
        log_info "mkfs.ext4 on $RW_DEVICE (first mount)"
        if ! mkfs.ext4 -F -q "$RW_DEVICE" >/dev/null 2>&1; then
            log_error "mkfs.ext4 failed on $RW_DEVICE"
            return 1
        fi
        EXT4_MADE=1
    fi
    if ! mount "$RW_DEVICE" "$MOUNT_POINT" 2>/dev/null; then
        log_error "mount $RW_DEVICE failed"
        return 1
    fi
    MOUNT_UP=1
}

umount_only() {
    if [[ $MOUNT_UP -eq 0 ]]; then return 0; fi
    sync
    if ! umount "$MOUNT_POINT" 2>/dev/null; then
        log_error "umount failed"
        return 1
    fi
    MOUNT_UP=0
}

# Path generator. Two-step pattern (mutate global, then printf -v) so the
# increment doesn't get lost in a $(new_path) subshell. Flat paths under
# the mount root — no subdir to mkdir.
new_path() {
    NEXT_PATH_INDEX=$(( NEXT_PATH_INDEX + 1 ))
    printf -v NEW_PATH '/%05d' "$NEXT_PATH_INDEX"
}

# Rebuild ALIVE_PATHS from FILE_VERSIONS. Called after delete.
rebuild_alive_paths() {
    local new=()
    for p in "${ALIVE_PATHS[@]}"; do
        if (( ${FILE_VERSIONS[$p]:-0} > 0 )); then
            new+=("$p")
        fi
    done
    ALIVE_PATHS=("${new[@]}")
}

# Pick one of ALIVE_PATHS, deterministically from MC_SEED + MC_OP_INDEX.
# Echoes the path on stdout; returns 1 if no alive paths.
pick_existing_path() {
    local n=${#ALIVE_PATHS[@]}
    (( n > 0 )) || return 1
    local idx
    idx=$(mc_rng_u32 "pick-path" "$n")
    echo "${ALIVE_PATHS[$idx]}"
}

# Regenerate the expected content for (path, version, size) into $1.
# Pure function — does not touch the mount.
regen_expected() {
    local path="$1" version="$2" size="$3" out="$4"
    mc_content_to "$path" "$version" "$size" "$out"
}

# ---------------------------------------------------------------------------
# Op handlers — each declares its prereqs at the top.
# ---------------------------------------------------------------------------

op_write_new() {
    ensure_mounted || return 1
    new_path
    local path="$NEW_PATH" size
    size=$(mc_pick_size_boundary_biased "size-write-new")
    local tmp="$TEST_DIR/scratch"
    regen_expected "$path" 1 "$size" "$tmp"
    local cp_err
    if ! cp_err=$(cp "$tmp" "$MOUNT_POINT$path" 2>&1); then
        # Only ENOSPC is "expected": volume residual filled. For that,
        # nuke a random alive file and retry once. Any other error is
        # a real bug — bail loudly so we don't paper over it.
        if [[ "$cp_err" != *"No space left"* && "$cp_err" != *"ENOSPC"* ]]; then
            log_error "write_new: cp failed (not ENOSPC) at $path size=$size: $cp_err"
            mc_dump_failure
            return 1
        fi
        local victim
        victim=$(pick_existing_path) || { mc_log_op write_new path="$path" size="$size" status=enospc_no_victim; return 0; }
        rm -f "$MOUNT_POINT$victim"
        FILE_VERSIONS[$victim]=0
        rebuild_alive_paths
        if ! cp "$tmp" "$MOUNT_POINT$path" 2>/dev/null; then
            mc_log_op write_new path="$path" size="$size" status=enospc_after_evict victim="$victim"
            return 0
        fi
    fi
    FILE_VERSIONS[$path]=1
    FILE_SIZES[$path]=$size
    ALIVE_PATHS+=("$path")
    mc_log_op write_new path="$path" size="$size" v=1
}

op_overwrite() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { op_write_new; return $?; }
    local old_v=${FILE_VERSIONS[$path]} new_v size
    new_v=$(( old_v + 1 ))
    size=$(mc_pick_size_boundary_biased "size-overwrite")
    local tmp="$TEST_DIR/scratch"
    regen_expected "$path" "$new_v" "$size" "$tmp"
    if ! cp "$tmp" "$MOUNT_POINT$path" 2>/dev/null; then
        mc_log_op overwrite path="$path" size="$size" v="$new_v" status=enospc
        return 0
    fi
    FILE_VERSIONS[$path]=$new_v
    FILE_SIZES[$path]=$size
    mc_log_op overwrite path="$path" size="$size" v="$new_v" old_size="${FILE_SIZES[$path]}"
}

op_append() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { op_write_new; return $?; }
    local v=${FILE_VERSIONS[$path]}
    local cur_size=${FILE_SIZES[$path]}
    # Cap append delta to keep total file size manageable. Bias small.
    local delta_bucket
    delta_bucket=$(mc_pick_weighted "append-bucket" "30:tiny" "30:small" "25:medium" "15:large")
    local delta
    case "$delta_bucket" in
        tiny)   delta=$(( $(mc_rng_u32 "delta-size" 4095) + 1 )) ;;
        small)  delta=$(( $(mc_rng_u32 "delta-size" 60000) + 4096 )) ;;
        medium) delta=$(( $(mc_rng_u32 "delta-size" 196608) + 65536 )) ;;
        large)  delta=$(( $(mc_rng_u32 "delta-size" 1048576) + 262144 )) ;;
    esac
    local new_size=$(( cur_size + delta ))
    local full="$TEST_DIR/scratch.full"
    local tail_only="$TEST_DIR/scratch.tail"
    regen_expected "$path" "$v" "$new_size" "$full"
    # Slice the [cur_size..new_size] tail. dd skip in bytes is portable
    # via iflag=skip_bytes (coreutils 8+).
    if ! dd if="$full" of="$tail_only" bs=4096 iflag=skip_bytes "skip=$cur_size" status=none 2>/dev/null; then
        log_error "append: dd tail-slice failed (cur=$cur_size new=$new_size)"
        mc_dump_failure
        return 1
    fi
    if ! cat "$tail_only" >> "$MOUNT_POINT$path" 2>/dev/null; then
        mc_log_op append path="$path" delta="$delta" new_size="$new_size" status=enospc
        return 0
    fi
    FILE_SIZES[$path]=$new_size
    # version unchanged — CTR keystream prefix property
    mc_log_op append path="$path" delta="$delta" new_size="$new_size" v="$v"
}

op_read_verify() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op read_verify status=no_files; return 0; }
    local v=${FILE_VERSIONS[$path]}
    local size=${FILE_SIZES[$path]}
    local actual_size
    actual_size=$(stat -c%s "$MOUNT_POINT$path" 2>/dev/null || echo "missing")
    if [[ "$actual_size" != "$size" ]]; then
        log_error "read_verify: stat size mismatch at $path: model=$size actual=$actual_size v=$v"
        mc_dump_failure
        return 1
    fi
    local tmp="$TEST_DIR/scratch.expect"
    regen_expected "$path" "$v" "$size" "$tmp"
    if ! cmp -s "$MOUNT_POINT$path" "$tmp"; then
        log_error "read_verify: content mismatch at $path (v=$v size=$size)"
        local first_diff
        first_diff=$(cmp "$MOUNT_POINT$path" "$tmp" 2>&1 | head -1)
        log_error "  first divergence: $first_diff"
        mc_dump_failure
        return 1
    fi
    mc_log_op read_verify path="$path" size="$size" v="$v"
}

op_delete() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op delete status=no_files; return 0; }
    if ! rm -f "$MOUNT_POINT$path" 2>/dev/null; then
        log_error "delete: rm failed at $path"
        mc_dump_failure
        return 1
    fi
    FILE_VERSIONS[$path]=0
    unset 'FILE_SIZES[$path]'
    rebuild_alive_paths
    mc_log_op delete path="$path"
}

op_truncate() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op truncate status=no_files; return 0; }
    local cur=${FILE_SIZES[$path]}
    if (( cur < 2 )); then
        mc_log_op truncate path="$path" status=too_small cur="$cur"
        return 0
    fi
    local new_size
    new_size=$(mc_rng_u32 "truncate-size" "$cur")
    if ! truncate -s "$new_size" "$MOUNT_POINT$path" 2>/dev/null; then
        log_error "truncate: failed at $path new=$new_size"
        mc_dump_failure
        return 1
    fi
    FILE_SIZES[$path]=$new_size
    # version unchanged — CTR keystream prefix property
    mc_log_op truncate path="$path" old="$cur" new="$new_size" v="${FILE_VERSIONS[$path]}"
}

op_sync() {
    ensure_mounted || return 1
    sync
    mc_log_op sync
}

op_umount_cycle() {
    if [[ $MOUNT_UP -eq 0 ]]; then
        mc_log_op umount_cycle status=already_unmounted
        return 0
    fi
    if ! umount_only; then
        return 1
    fi
    mc_log_op umount_cycle
}

op_transport_logout_cycle() {
    if [[ $TRANSPORT_UP -eq 0 ]]; then
        mc_log_op transport_logout_cycle transport="$TRANSPORT" status=already_logged_out
        return 0
    fi
    transport_logout
    mc_log_op transport_logout_cycle transport="$TRANSPORT"
}

# ---------------------------------------------------------------------------
# Main op loop.
# ---------------------------------------------------------------------------

run_ops() {
    local n="$1"
    local op
    local progress_every=$(( n / 20 ))
    (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n; MC_OP_INDEX++ )); do
        op=$(mc_pick_weighted op \
            "22:write_new" "14:overwrite" "14:append" "24:read_verify" \
            "8:delete" "4:truncate" "4:sync" \
            "6:umount_cycle" "4:transport_logout_cycle")
        case "$op" in
            write_new)                op_write_new || return 1 ;;
            overwrite)                op_overwrite || return 1 ;;
            append)                   op_append || return 1 ;;
            read_verify)              op_read_verify || return 1 ;;
            delete)                   op_delete || return 1 ;;
            truncate)                 op_truncate || return 1 ;;
            sync)                     op_sync || return 1 ;;
            umount_cycle)             op_umount_cycle || return 1 ;;
            transport_logout_cycle)   op_transport_logout_cycle || return 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            log_info "[$MC_OP_INDEX/$n] alive=${#ALIVE_PATHS[@]} mount=$MOUNT_UP $TRANSPORT=$TRANSPORT_UP"
        fi
    done
}

# Final pass: re-verify every alive file end-to-end. Catches drift that
# the in-loop read_verify rate left untouched.
final_verify_all() {
    ensure_mounted || return 1
    log_info "Final verify of all ${#ALIVE_PATHS[@]} alive files..."
    local p v size tmp="$TEST_DIR/scratch.final"
    local checked=0
    for p in "${ALIVE_PATHS[@]}"; do
        (( ${FILE_VERSIONS[$p]:-0} > 0 )) || continue
        v=${FILE_VERSIONS[$p]}
        size=${FILE_SIZES[$p]}
        regen_expected "$p" "$v" "$size" "$tmp"
        if ! cmp -s "$MOUNT_POINT$p" "$tmp"; then
            log_error "final_verify: content mismatch at $p (v=$v size=$size)"
            local first_diff
            first_diff=$(cmp "$MOUNT_POINT$p" "$tmp" 2>&1 | head -1)
            log_error "  first divergence: $first_diff"
            mc_dump_failure
            return 1
        fi
        checked=$(( checked + 1 ))
    done
    log_info "Final verify OK ($checked files)"
}

main() {
    echo "========================================"
    echo "thurvsa Monte Carlo Random-Op Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports_mc
    create_test_config
    start_daemon
    ensure_volume

    mc_seed_init "$SEED" "$TEST_DIR/ops.log"

    log_info "Running $OPS random ops (transport=$TRANSPORT, volume=${VOLUME_SIZE_MIB} MiB, backend=$BACKEND_NAME/$BACKEND_TYPE)"
    if ! run_ops "$OPS"; then
        log_fail "Op loop aborted on failure"
        exit 1
    fi

    if ! final_verify_all; then
        log_fail "Final verification failed"
        exit 1
    fi

    echo ""
    echo "========================================"
    log_pass "$OPS ops + final verify  (seed=$MC_SEED)"
    echo "========================================"
    echo "Final state:"
    echo "  alive files: ${#ALIVE_PATHS[@]}"
    echo "  reusable reproducer: --seed $MC_SEED --ops $OPS --transport $TRANSPORT"
    echo "  op log: $TEST_DIR/ops.log"
    exit 0
}

main
