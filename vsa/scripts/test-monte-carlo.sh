#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa Monte Carlo Random-Op Test
#
# Runs N seeded random operations against ext4 filesystems mounted on
# two thurvsa volumes (vol-mc + vol-mc-b). Each op picks a random volume
# and a random parent directory (root or any existing dir), so the same
# op stream sweeps per-volume PageCache isolation, multi-LUN-per-session
# SCSI dispatch, and a two-level directory layout. The transport (iSCSI
# or NVMe/TCP) is selectable via --transport; the op generator, content
# model, and verification are transport-agnostic — only the login /
# device-discovery / logout-cycle primitives branch. Op mix is weighted
# to bias file ops and sample transport / mount churn at lower rates —
# `umount_cycle` and `transport_logout_cycle` tear the lower layer down;
# the next file op lazily brings it back. That exposes the daemon to
# "user opens session, does ops, logs out, comes back later, does more
# ops" workloads that the deterministic scripted tests don't reach.
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
#   --quick               200 ops, ~30 MB residual (default: 1000 ops, ~170 MB)
#   --ops N               Override op count
#   --transport T         iscsi (default) or nvmetcp
#   --backend NAME        Use named backend entry (same as THURVSA_TEST_BACKEND)
#   --debug               Use ./target/debug/ binaries (default: ./target/release/)
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
# test-fs-storage.sh.
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

TEST_DIR="/tmp/thurvsa-monte-carlo-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TRANSPORT="iscsi"
NVMETCP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-monte-carlo-test"
ISCSI_CONNECTED=0
NVME_CONNECTED=0
NVME_DEVICE=""

# Two-volume setup: vol-mc + vol-mc-b, each its own LUN/namespace,
# ext4-formatted and mounted under TEST_DIR/mnt-<name>. The path
# generator picks one of MOUNT_POINTS per op so the same op stream
# sweeps per-volume PageCache isolation, per-volume SYNCHRONIZE CACHE
# fencing, and multi-LUN-per-session SCSI dispatch.
VOLUME_NAMES=("vol-mc" "vol-mc-b")
declare -a MOUNT_POINTS=()
declare -a RW_DEVICES=()
declare -a EXT4_MADE=()
RW_SG_DEVICE=""

VOLUME_SIZE_MIB=1024
SEED=""
QUICK=0
OPS=""
BACKEND_NAME="${THURVSA_TEST_BACKEND:-}"

# Auth wrapper. THURVSA_TEST_AUTH=chap enables iSCSI CHAP — the conffile
# carries auth.method: CHAP, a per-run user/secret is added via
# `thurvsa iscsi users add` after daemon start, and iscsi_login sets
# node.session.auth credentials before --login. NVMe-TCP PSK is not
# wired here yet (would need PSK material + nvme connect TLS args);
# CHAP applies to iSCSI transport only.
AUTH_MODE="${THURVSA_TEST_AUTH:-none}"
case "$AUTH_MODE" in
    none|chap) ;;
    *) echo "Unsupported THURVSA_TEST_AUTH='$AUTH_MODE' (expected none|chap)" >&2; exit 1 ;;
esac
CHAP_USER="mc-user-$$"
CHAP_PASS="mc-secret-$(od -An -N12 -tx8 /dev/urandom | tr -d ' \n')"
SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
BACKEND_TYPE=""
TEST_PREFIX=""

# Mount/transport lazy state. Both start "down" and the first file op
# brings them up. MOUNT_UP is binary across all volumes — the harness
# mounts/umounts as a unit so the model stays simple.
TRANSPORT_UP=0
MOUNT_UP=0

# In-memory file model. Keys are absolute paths (under one of the
# MOUNT_POINTS), so the model is volume-agnostic. FILE_VERSIONS[path]=int
# (0 = deleted/never-existed), FILE_SIZES[path]=bytes,
# CONTENT_KEY[path]=opaque-string used as the AES-CTR keystream key (set
# = path on create; preserved across op_rename so content survives the move).
# ALIVE_PATHS is the index for "pick an existing file", rebuilt on delete.
# ALIVE_DIRS parallels it for directories.
declare -A FILE_VERSIONS
declare -A FILE_SIZES
declare -A CONTENT_KEY
declare -a ALIVE_PATHS
declare -a ALIVE_DIRS
NEXT_PATH_INDEX=1
NEXT_DIR_INDEX=1

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --ops) OPS="$2"; shift 2 ;;
        --transport) TRANSPORT="$2"; shift 2 ;;
        --backend) BACKEND_NAME="$2"; shift 2 ;;
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
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

# Op-count rationale: 1000 is the "standard" run (override with --ops N;
# --quick = 200-op smoke). By the rule of three (~3/N), 1000 ops catch
# per-op regressions down to ~0.3% (1-in-333) at 95% confidence and give
# even the rarest ~2%-weighted ops ~20 occurrences -- enough to exercise
# their branches without a multi-thousand-op soak. The deep tail (rare op
# *sequences*, e.g. one rare op right after another) is not caught by a
# bigger single run but by the nightly re-running with a fresh seed:
# independent trajectories compound coverage and grow the reproducible
# seed corpus over time. Use --ops 3000 or more for a pre-release soak.
if [[ $QUICK -eq 1 ]]; then
    : "${OPS:=200}"
    VOLUME_SIZE_MIB=128
else
    : "${OPS:=1000}"
fi

case "$TRANSPORT" in
    iscsi|nvmetcp) ;;
    *) echo "Unknown --transport '$TRANSPORT' (expected iscsi or nvmetcp)"; exit 1 ;;
esac

if [[ "$AUTH_MODE" == "chap" && "$TRANSPORT" == "nvmetcp" ]]; then
    echo "THURVSA_TEST_AUTH=chap is iSCSI-only (NVMe-TCP PSK is not yet wired)" >&2
    exit 1
fi

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    local mp
    for mp in "${MOUNT_POINTS[@]}"; do
        if mountpoint -q "$mp" 2>/dev/null; then
            umount "$mp" 2>/dev/null || true
        fi
    done

    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
    fi
    if [[ $NVME_CONNECTED -eq 1 ]]; then
        nvme_tcp_disconnect
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
    mkdir -p "$TEST_DIR/data/volumes"
    local name mp
    for name in "${VOLUME_NAMES[@]}"; do
        mp="${TEST_DIR}/mnt-${name}"
        mkdir -p "$mp"
        MOUNT_POINTS+=("$mp")
        EXT4_MADE+=(0)
        RW_DEVICES+=("")
    done
    local transport_block
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        transport_block=$'iscsi:\n  listen: "127.0.0.1:'"$ISCSI_PORT"'"'
        if [[ "$AUTH_MODE" == "chap" ]]; then
            transport_block+=$'\n  auth:\n    method: CHAP'
        fi
    else
        transport_block=$'transports: [nvmetcp]\nnvmetcp:\n  listen: "0.0.0.0:'"$NVMETCP_PORT"'"'
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

ensure_volumes() {
    local existing name
    existing=$("$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null || true)
    for name in "${VOLUME_NAMES[@]}"; do
        if echo "$existing" | grep -q "$name"; then
            log_info "Volume $name already present"
            continue
        fi
        log_info "Creating $name (${VOLUME_SIZE_MIB} MiB, backend=$BACKEND_NAME)..."
        "$CLI_PATH" --config "$TEST_CONFIG" volume create "$name" \
            --size "${VOLUME_SIZE_MIB}M" --backend "$BACKEND_NAME" >/dev/null
    done
}

# If THURVSA_TEST_AUTH=chap, provision one CHAP user the harness then
# uses for every iSCSI login. Idempotent: a second call after
# op_daemon_restart sees the user already present and skips.
setup_chap_user() {
    [[ "$AUTH_MODE" != "chap" ]] && return 0
    if "$CLI_PATH" --config "$TEST_CONFIG" iscsi users list 2>/dev/null | grep -q "$CHAP_USER"; then
        return 0
    fi
    log_info "Adding CHAP user $CHAP_USER..."
    # Mandatory admission (VSA): grant every test volume so the
    # harness sees the full LUN set just like the no-CHAP case.
    local volume_args=()
    local v
    for v in "${VOLUME_NAMES[@]}"; do
        volume_args+=("--volume" "$v")
    done
    if ! "$CLI_PATH" --config "$TEST_CONFIG" iscsi users add "$CHAP_USER" \
            --password "$CHAP_PASS" "${volume_args[@]}" >/dev/null 2>&1; then
        log_error "Failed to add CHAP user $CHAP_USER"
        tail -20 "${TEST_DIR}/daemon.log"
        exit 1
    fi
}

# Bring iSCSI session up + resolve one /dev/sdN per volume in LUN
# order. Idempotent. Volume create order = LUN order (registry assigns
# monotonic LUNs), and lsscsi sorts by [H:C:I:L], so the row order
# matches MOUNT_POINTS / VOLUME_NAMES exactly.
_iscsi_login() {
    # Under CHAP, SendTargets discovery itself needs auth — stash creds
    # on the discoverydb entry first.
    if [[ "$AUTH_MODE" == "chap" ]]; then
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -o new >/dev/null 2>&1 || true
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.authmethod -v CHAP >/dev/null 2>&1
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.username -v "$CHAP_USER" >/dev/null 2>&1
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.password -v "$CHAP_PASS" >/dev/null 2>&1
    fi
    if ! iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" --discover >/dev/null 2>&1; then
        log_error "iscsi discovery failed"
        return 1
    fi
    if [[ "$AUTH_MODE" == "chap" ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.authmethod -v CHAP >/dev/null 2>&1
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.username -v "$CHAP_USER" >/dev/null 2>&1
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.password -v "$CHAP_PASS" >/dev/null 2>&1
    fi
    if ! iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1; then
        log_error "iscsi login failed"
        return 1
    fi
    ISCSI_CONNECTED=1
    sleep 2
    local rows i n=${#VOLUME_NAMES[@]}
    for _ in 1 2 3 4 5; do
        rows=$(lsscsi -g | awk '/THUR VSA/ {print}')
        if (( $(echo "$rows" | grep -c .) >= n )); then break; fi
        sleep 1
    done
    if (( $(echo "$rows" | grep -c .) < n )); then
        log_error "iscsi login OK but only $(echo "$rows" | grep -c .) THUR VSA devices appeared (expected $n)"
        lsscsi -g
        return 1
    fi
    for (( i=0; i<n; i++ )); do
        local row dev
        row=$(echo "$rows" | sed -n "$((i+1))p")
        dev=$(echo "$row" | awk '{print $(NF-1)}')
        [[ -b "$dev" ]] || { log_error "$dev is not a block device"; return 1; }
        RW_DEVICES[$i]="$dev"
    done
    # Take the first row's sg path for any SCSI-level utilities; not
    # used in the data path.
    RW_SG_DEVICE=$(echo "$rows" | head -1 | awk '{print $NF}')
}

_iscsi_logout() {
    iscsi_logout_and_delete
    RW_SG_DEVICE=""
}

# Bring NVMe/TCP session up + resolve one /dev/nvmeXn<NSID> per volume.
# NSID = LUN + 1 per the daemon's mapping, so namespace order matches
# volume creation order. Idempotent.
_nvme_login() {
    nvme_tcp_connect || return 1
    local i n=${#VOLUME_NAMES[@]}
    for (( i=0; i<n; i++ )); do
        local dev="/dev/${NVME_DEVICE}n$(( i + 1 ))"
        # The nvme connect attaches every advertised namespace
        # synchronously; the device nodes can lag by a tick.
        for _ in 1 2 3 4 5; do
            [[ -b "$dev" ]] && break
            sleep 0.5
        done
        [[ -b "$dev" ]] || { log_error "$dev did not appear after nvme connect"; return 1; }
        RW_DEVICES[$i]="$dev"
    done
}

_nvme_logout() {
    nvme_tcp_disconnect
    NVME_DEVICE=""
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
    # Must umount every volume before tearing the lower layer down.
    if [[ $MOUNT_UP -eq 1 ]]; then
        local mp
        for mp in "${MOUNT_POINTS[@]}"; do
            umount "$mp" 2>/dev/null || true
        done
        MOUNT_UP=0
    fi
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        _iscsi_logout
    else
        _nvme_logout
    fi
    TRANSPORT_UP=0
    local i
    for (( i=0; i<${#RW_DEVICES[@]}; i++ )); do
        RW_DEVICES[$i]=""
    done
}

# mkfs per volume on first call; mount every volume on every call
# thereafter. Idempotent on already-mounted.
ensure_mounted() {
    transport_login || return 1
    if [[ $MOUNT_UP -eq 1 ]]; then return 0; fi
    local i n=${#VOLUME_NAMES[@]}
    for (( i=0; i<n; i++ )); do
        local dev="${RW_DEVICES[$i]}" mp="${MOUNT_POINTS[$i]}"
        if [[ "${EXT4_MADE[$i]}" -eq 0 ]]; then
            log_info "mkfs.ext4 on $dev (first mount for ${VOLUME_NAMES[$i]})"
            if ! mkfs.ext4 -F -q "$dev" >/dev/null 2>&1; then
                log_error "mkfs.ext4 failed on $dev"
                return 1
            fi
            EXT4_MADE[$i]=1
        fi
        if ! mount "$dev" "$mp" 2>/dev/null; then
            log_error "mount $dev -> $mp failed"
            return 1
        fi
    done
    MOUNT_UP=1
}

umount_only() {
    if [[ $MOUNT_UP -eq 0 ]]; then return 0; fi
    sync
    local mp rc=0
    for mp in "${MOUNT_POINTS[@]}"; do
        if ! umount "$mp" 2>/dev/null; then
            log_error "umount $mp failed"
            rc=1
        fi
    done
    if (( rc != 0 )); then return 1; fi
    MOUNT_UP=0
}

# Pick a mount root from MOUNT_POINTS deterministically per op.
pick_mount_root() {
    local n=${#MOUNT_POINTS[@]}
    local idx
    idx=$(mc_rng_u32 "pick-mount" "$n")
    echo "${MOUNT_POINTS[$idx]}"
}

# Pick a parent directory for a new file/dir. With probability ~20%
# returns the mount root itself; otherwise picks among existing dirs
# under that root. Emits the chosen parent to stdout.
pick_parent_under() {
    local root="$1"
    local matching=() d
    for d in "${ALIVE_DIRS[@]}"; do
        if [[ "$d" == "$root"/* ]]; then
            matching+=("$d")
        fi
    done
    local n=${#matching[@]}
    if (( n == 0 )); then
        echo "$root"
        return
    fi
    local roll
    roll=$(mc_rng_u32 "use-root" 100)
    if (( roll < 20 )); then
        echo "$root"
    else
        local idx
        idx=$(mc_rng_u32 "pick-dir" "$n")
        echo "${matching[$idx]}"
    fi
}

# Path generator. Two-step pattern (mutate global, then printf -v) so
# the increment doesn't get lost in a $(new_path) subshell. Two-level
# scheme: <mount_root>/d-NN/f-NNNNN, or <mount_root>/f-NNNNN if the
# picker lands on the root. Volume + parent-dir selection runs through
# the same RNG keys so the layout is reproducible under --seed.
new_path() {
    NEXT_PATH_INDEX=$(( NEXT_PATH_INDEX + 1 ))
    local mount_root parent
    mount_root=$(pick_mount_root)
    parent=$(pick_parent_under "$mount_root")
    printf -v NEW_PATH '%s/f-%05d' "$parent" "$NEXT_PATH_INDEX"
}

# Rebuild ALIVE_PATHS from FILE_VERSIONS. Called after delete.
rebuild_alive_paths() {
    local new=() p
    for p in "${ALIVE_PATHS[@]}"; do
        if (( ${FILE_VERSIONS[$p]:-0} > 0 )); then
            new+=("$p")
        fi
    done
    ALIVE_PATHS=("${new[@]}")
}

# Drop one entry from ALIVE_DIRS by exact match. Order is preserved
# for everything else; called from op_rmdir after a successful rmdir(2).
drop_alive_dir() {
    local target="$1"
    local new=() d
    for d in "${ALIVE_DIRS[@]}"; do
        [[ "$d" == "$target" ]] || new+=("$d")
    done
    ALIVE_DIRS=("${new[@]}")
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
# The content key is CONTENT_KEY[path] (set = path on create, preserved
# across op_rename so a moved file's bytes don't have to be rewritten),
# falling back to the path itself for backwards compat.
regen_expected() {
    local path="$1" version="$2" size="$3" out="$4"
    local key="${CONTENT_KEY[$path]:-$path}"
    mc_content_to "$key" "$version" "$size" "$out"
}

# ---------------------------------------------------------------------------
# Op handlers — each declares its prereqs at the top.
# ---------------------------------------------------------------------------

op_write_new() {
    ensure_mounted || return 1
    new_path
    local path="$NEW_PATH" size
    # CONTENT_KEY is set to the path at creation; subsequent ops use it
    # so renamed files keep their content key.
    CONTENT_KEY[$path]="$path"
    size=$(mc_pick_size_boundary_biased "size-write-new")
    local tmp="$TEST_DIR/scratch"
    regen_expected "$path" 1 "$size" "$tmp"
    local cp_err
    if ! cp_err=$(cp "$tmp" "$path" 2>&1); then
        # Only ENOSPC is "expected": volume residual filled. For that,
        # nuke a random alive file and retry once. Any other error is
        # a real bug — bail loudly so we don't paper over it.
        if [[ "$cp_err" != *"No space left"* && "$cp_err" != *"ENOSPC"* ]]; then
            log_error "write_new: cp failed (not ENOSPC) at $path size=$size: $cp_err"
            mc_dump_failure
            return 1
        fi
        local victim
        victim=$(pick_existing_path) || { unset 'CONTENT_KEY[$path]'; mc_log_op write_new path="$path" size="$size" status=enospc_no_victim; return 0; }
        rm -f "$victim"
        FILE_VERSIONS[$victim]=0
        unset 'CONTENT_KEY[$victim]'
        rebuild_alive_paths
        if ! cp "$tmp" "$path" 2>/dev/null; then
            unset 'CONTENT_KEY[$path]'
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
    if ! cp "$tmp" "$path" 2>/dev/null; then
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
    if ! cat "$tail_only" >> "$path" 2>/dev/null; then
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
    actual_size=$(stat -c%s "$path" 2>/dev/null || echo "missing")
    if [[ "$actual_size" != "$size" ]]; then
        log_error "read_verify: stat size mismatch at $path: model=$size actual=$actual_size v=$v"
        mc_dump_failure
        return 1
    fi
    local tmp="$TEST_DIR/scratch.expect"
    regen_expected "$path" "$v" "$size" "$tmp"
    if ! cmp -s "$path" "$tmp"; then
        log_error "read_verify: content mismatch at $path (v=$v size=$size)"
        local first_diff
        first_diff=$(cmp "$path" "$tmp" 2>&1 | head -1)
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
    if ! rm -f "$path" 2>/dev/null; then
        log_error "delete: rm failed at $path"
        mc_dump_failure
        return 1
    fi
    FILE_VERSIONS[$path]=0
    unset 'FILE_SIZES[$path]'
    unset 'CONTENT_KEY[$path]'
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
    if ! truncate -s "$new_size" "$path" 2>/dev/null; then
        log_error "truncate: failed at $path new=$new_size"
        mc_dump_failure
        return 1
    fi
    FILE_SIZES[$path]=$new_size
    # version unchanged — CTR keystream prefix property
    mc_log_op truncate path="$path" old="$cur" new="$new_size" v="${FILE_VERSIONS[$path]}"
}

# Grow path for truncate. truncate -s LARGER creates a sparse hole at
# [cur..new_size); we then overwrite that range with the next slice of
# the file's CTR keystream so the model stays "all file bytes are
# CTR(v) of (seed, path, v)" — same invariant as op_append. The grow
# syscall is exercised on the way up; the trailing write makes the
# read-side check stay trivial.
op_truncate_extend() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op truncate_extend status=no_files; return 0; }
    local cur=${FILE_SIZES[$path]}
    local v=${FILE_VERSIONS[$path]}
    local delta
    delta=$(mc_pick_size_boundary_biased "truncate-extend-delta")
    (( delta > 16777216 )) && delta=16777216
    local new_size=$(( cur + delta ))
    if ! truncate -s "$new_size" "$path" 2>/dev/null; then
        mc_log_op truncate_extend path="$path" old="$cur" new="$new_size" status=enospc
        return 0
    fi
    # Regenerate the full expected stream, then dd just the [cur..new_size)
    # tail back into the file.
    local full="$TEST_DIR/scratch.full"
    regen_expected "$path" "$v" "$new_size" "$full"
    if ! dd if="$full" of="$path" \
            bs=4096 iflag=skip_bytes oflag=seek_bytes conv=notrunc \
            skip="$cur" seek="$cur" status=none 2>/dev/null; then
        # The truncate succeeded but the tail-write didn't — likely the
        # sparse-to-allocated promotion hit ENOSPC. Roll the model back
        # to the pre-op size so subsequent verify reads the unchanged
        # prefix correctly; the on-disk file is now larger but
        # zero-filled in the tail, which would otherwise mismatch CTR.
        truncate -s "$cur" "$path" 2>/dev/null || true
        mc_log_op truncate_extend path="$path" old="$cur" new="$new_size" status=enospc_tail
        return 0
    fi
    FILE_SIZES[$path]=$new_size
    # version unchanged — CTR keystream prefix property
    mc_log_op truncate_extend path="$path" old="$cur" new="$new_size" v="$v"
}

op_sync() {
    ensure_mounted || return 1
    sync
    mc_log_op sync
}

# Per-file fdatasync. Distinct from op_sync (which is the process-wide
# sync(2)): this targets one file's FD and lands as a SCSI SYNCHRONIZE
# CACHE / NVMe Flush bounded to that file's pages. Exercises the
# per-file fence claim from the daemon's volume fsync mode.
op_fdatasync_one() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op fdatasync_one status=no_files; return 0; }
    if ! python3 -c '
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
try:
    os.fdatasync(fd)
finally:
    os.close(fd)
' "$path" 2>/dev/null; then
        log_error "fdatasync_one: failed at $path"
        mc_dump_failure
        return 1
    fi
    mc_log_op fdatasync_one path="$path"
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

# Stop and restart the daemon. sync(2) before stop forces the ext4
# journal and dirty pages out via SYNCHRONIZE CACHE / NVMe Flush so the
# daemon's per-volume PageCache flushes pending pages into the chunk
# pool before SIGTERM. umount + transport logout follow so the kernel
# doesn't keep half-broken sessions around. On restart the daemon
# re-discovers volumes from <data_dir>/volumes/ and the next file op
# lazily re-establishes via ensure_mounted -> transport_login.
op_daemon_restart() {
    if [[ $MOUNT_UP -eq 1 ]]; then
        sync || true
        local mp
        for mp in "${MOUNT_POINTS[@]}"; do
            umount "$mp" 2>/dev/null || true
        done
        MOUNT_UP=0
    fi
    if [[ $TRANSPORT_UP -eq 1 ]]; then
        if [[ "$TRANSPORT" == "iscsi" ]]; then
            _iscsi_logout
        else
            _nvme_logout
        fi
        TRANSPORT_UP=0
        local i
        for (( i=0; i<${#RW_DEVICES[@]}; i++ )); do
            RW_DEVICES[$i]=""
        done
    fi
    stop_thur_daemon
    sleep 0.2
    start_daemon
    mc_log_op daemon_restart
}

# Create a fresh directory under a random volume's random parent (the
# mount root, or any existing dir under it). The new dir joins
# ALIVE_DIRS so subsequent new_path / op_mkdir picks can nest into it.
op_mkdir() {
    ensure_mounted || return 1
    NEXT_DIR_INDEX=$(( NEXT_DIR_INDEX + 1 ))
    local mount_root parent dir
    mount_root=$(pick_mount_root)
    parent=$(pick_parent_under "$mount_root")
    printf -v dir '%s/d-%05d' "$parent" "$NEXT_DIR_INDEX"
    if ! mkdir "$dir" 2>/dev/null; then
        log_error "mkdir: failed at $dir"
        mc_dump_failure
        return 1
    fi
    ALIVE_DIRS+=("$dir")
    mc_log_op mkdir path="$dir"
}

# Remove an empty dir. Picks a random dir from ALIVE_DIRS and scans
# forward until one rmdirs cleanly (most dirs in our scheme end up
# non-empty quickly). all_nonempty is logged as a soft status when
# every dir refuses the call.
op_rmdir() {
    ensure_mounted || return 1
    local n=${#ALIVE_DIRS[@]}
    if (( n == 0 )); then
        mc_log_op rmdir status=no_dirs
        return 0
    fi
    local pick
    pick=$(mc_rng_u32 "pick-rmdir" "$n")
    local i d
    for (( i=0; i<n; i++ )); do
        d="${ALIVE_DIRS[$(( (pick + i) % n ))]}"
        if rmdir "$d" 2>/dev/null; then
            drop_alive_dir "$d"
            mc_log_op rmdir path="$d"
            return 0
        fi
    done
    mc_log_op rmdir status=all_nonempty
}

# Rename a file to a new (volume-local) path. Always within the same
# volume so the kernel does rename(2) (atomic), never a cross-fs
# copy+unlink. CONTENT_KEY[dst] inherits from src so the file's content
# survives the move without a rewrite.
op_rename() {
    ensure_mounted || return 1
    local src
    src=$(pick_existing_path) || { mc_log_op rename status=no_files; return 0; }
    NEXT_PATH_INDEX=$(( NEXT_PATH_INDEX + 1 ))
    local src_mount mp parent dst
    for mp in "${MOUNT_POINTS[@]}"; do
        if [[ "$src" == "$mp"/* ]]; then src_mount="$mp"; break; fi
    done
    [[ -z "$src_mount" ]] && { mc_log_op rename src="$src" status=mount_unknown; return 0; }
    parent=$(pick_parent_under "$src_mount")
    printf -v dst '%s/f-%05d' "$parent" "$NEXT_PATH_INDEX"
    if [[ "$dst" == "$src" ]]; then
        # printf collision (extremely rare since NEXT_PATH_INDEX is monotonic).
        mc_log_op rename src="$src" status=same_path
        return 0
    fi
    if ! mv "$src" "$dst" 2>/dev/null; then
        mc_log_op rename src="$src" dst="$dst" status=mv_failed
        return 0
    fi
    FILE_VERSIONS[$dst]="${FILE_VERSIONS[$src]}"
    FILE_SIZES[$dst]="${FILE_SIZES[$src]}"
    CONTENT_KEY[$dst]="${CONTENT_KEY[$src]:-$src}"
    FILE_VERSIONS[$src]=0
    unset 'FILE_SIZES[$src]'
    unset 'CONTENT_KEY[$src]'
    local new=() p
    for p in "${ALIVE_PATHS[@]}"; do
        [[ "$p" == "$src" ]] && continue
        new+=("$p")
    done
    new+=("$dst")
    ALIVE_PATHS=("${new[@]}")
    mc_log_op rename src="$src" dst="$dst"
}

# Mid-file overwrite. Picks a 4 KiB-aligned offset and a
# boundary-biased length, then issues a single dd seek=off conv=notrunc
# write to land as a SCSI WRITE at a specific mid-file LBA range. The
# model side bumps version and rewrites the rest of the file from the
# new keystream — per plan, we accept the full-rewrite cost rather than
# carry a per-segment version map. The mid-file dd is the actual codepath
# exercise; the cp reconciles the model.
op_write_at_offset() {
    ensure_mounted || return 1
    local path
    path=$(pick_existing_path) || { mc_log_op write_at_offset status=no_files; return 0; }
    local cur=${FILE_SIZES[$path]}
    if (( cur < 8192 )); then
        mc_log_op write_at_offset path="$path" status=too_small cur="$cur"
        return 0
    fi
    local v=${FILE_VERSIONS[$path]} new_v=$(( v + 1 ))
    # 4 KiB-aligned offset strictly inside the file.
    local raw_off
    raw_off=$(mc_rng_u32 "wao-offset" "$cur")
    local off=$(( raw_off / 4096 * 4096 ))
    (( off + 4096 > cur )) && off=$(( ((cur - 4096) / 4096) * 4096 ))
    (( off < 0 )) && off=0
    local raw_len
    raw_len=$(mc_pick_size_boundary_biased "wao-len")
    local remaining=$(( cur - off ))
    local len=$(( raw_len < remaining ? raw_len : remaining ))
    local full="$TEST_DIR/scratch.full"
    regen_expected "$path" "$new_v" "$cur" "$full"
    local slice="$TEST_DIR/scratch.slice"
    if ! dd if="$full" of="$slice" bs="$len" count=1 iflag=skip_bytes \
            skip="$off" status=none 2>/dev/null; then
        log_error "write_at_offset: slice extraction failed (off=$off len=$len cur=$cur)"
        mc_dump_failure
        return 1
    fi
    if ! dd if="$slice" of="$path" bs="$len" count=1 \
            oflag=seek_bytes conv=notrunc seek="$off" status=none 2>/dev/null; then
        mc_log_op write_at_offset path="$path" off="$off" len="$len" status=enospc_slice
        return 0
    fi
    if ! cp "$full" "$path" 2>/dev/null; then
        log_error "write_at_offset: full-rewrite reconcile failed at $path"
        mc_dump_failure
        return 1
    fi
    FILE_VERSIONS[$path]=$new_v
    mc_log_op write_at_offset path="$path" off="$off" len="$len" v="$new_v"
}

# ---------------------------------------------------------------------------
# Main op loop.
# ---------------------------------------------------------------------------

# Weights for the random op picker. Must sum to 100; mc_assert_weights
# at startup enforces that. truncate_extend / fdatasync_one / mkdir /
# rmdir / rename / write_at_offset are low-rate correctness-shape ops,
# not throughput drivers.
OP_WEIGHTS=(
    "17:write_new" "12:overwrite" "11:append" "16:read_verify"
    "5:write_at_offset" "6:delete" "3:truncate" "3:truncate_extend"
    "4:sync" "3:fdatasync_one"
    "3:mkdir" "2:rmdir" "3:rename"
    "6:umount_cycle" "4:transport_logout_cycle"
    "2:daemon_restart"
)

run_ops() {
    local n="$1"
    local op
    local progress_every=$(( n / 20 ))
    (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n; MC_OP_INDEX++ )); do
        op=$(mc_pick_weighted op "${OP_WEIGHTS[@]}")
        case "$op" in
            write_new)                op_write_new || return 1 ;;
            overwrite)                op_overwrite || return 1 ;;
            append)                   op_append || return 1 ;;
            write_at_offset)          op_write_at_offset || return 1 ;;
            read_verify)              op_read_verify || return 1 ;;
            delete)                   op_delete || return 1 ;;
            truncate)                 op_truncate || return 1 ;;
            truncate_extend)          op_truncate_extend || return 1 ;;
            sync)                     op_sync || return 1 ;;
            fdatasync_one)            op_fdatasync_one || return 1 ;;
            mkdir)                    op_mkdir || return 1 ;;
            rmdir)                    op_rmdir || return 1 ;;
            rename)                   op_rename || return 1 ;;
            umount_cycle)             op_umount_cycle || return 1 ;;
            transport_logout_cycle)   op_transport_logout_cycle || return 1 ;;
            daemon_restart)           op_daemon_restart || return 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            log_info "[$MC_OP_INDEX/$n] seed=$MC_SEED alive=${#ALIVE_PATHS[@]} dirs=${#ALIVE_DIRS[@]} mount=$MOUNT_UP $TRANSPORT=$TRANSPORT_UP"
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
        if ! cmp -s "$p" "$tmp"; then
            log_error "final_verify: content mismatch at $p (v=$v size=$size)"
            local first_diff
            first_diff=$(cmp "$p" "$tmp" 2>&1 | head -1)
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
    setup_chap_user
    ensure_volumes

    mc_assert_weights "op" "${OP_WEIGHTS[@]}"
    mc_seed_init "$SEED" "$TEST_DIR/ops.log"

    log_info "Running $OPS random ops (transport=$TRANSPORT, volumes=${#VOLUME_NAMES[@]} x ${VOLUME_SIZE_MIB} MiB, backend=$BACKEND_NAME/$BACKEND_TYPE)"
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
    echo ""
    mc_op_stats_dump
    exit 0
}

main
