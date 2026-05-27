#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa End-to-End Filesystem Workflow Test (NVMe/TCP + storage backend)
#
# Same shape as test-fs-iscsi-storage.sh, but driven through Linux nvme-cli +
# nvme_tcp instead of open-iscsi. Clones a backend definition from
# private/storage-backends.json (or $THURVSA_SOURCE_BACKENDS) so the
# upload pipeline, HEAD-then-PUT dedup, page-eviction-driven storage
# refetch, and SYNC-fenced flush paths actually fire against a real
# storage backend through the NVMe/TCP transport.
#
# Selection: set THURVSA_TEST_BACKEND to the name of an entry under
# `backends:` in the source storage-backends.json. The script copies that
# entry verbatim into the test config and appends a per-run sub-prefix
# so test data is isolated and trivially purgeable.
#
# Refusals:
#   - Backend `type: local`         (defeats the purpose)
#   - Backend `retention_mode != none` (test cleanup can't delete locked objects)
#
# Cleanup: always purges the test sub-prefix from the bucket on exit
# (even on failure), unless --keep-storage is passed.
#
# Stress / scale: bump the fixture via THURVSA_FIXTURE_MB (env, MiB,
# default 8). Storage round-trips dominate runtime — bigger fixture =
# longer test = real egress $$$.
#   THURVSA_TEST_BACKEND=primary THURVSA_FIXTURE_MB=128 \
#     ./vsa/scripts/test-fs-nvmetcp-storage.sh
#
# Prerequisites:
#   - nvme-cli         (sudo apt-get install nvme-cli)
#   - nvme_tcp kernel module (sudo modprobe nvme_tcp)
#   - e2fsprogs, util-linux, tar (usually present)
#   - jq (parses private/storage-backends.json)
#   - Backend CLI matching the backend type:
#       s3    -> aws       (sudo apt-get install awscli)
#       gcs   -> gcloud    (https://cloud.google.com/sdk)
#       azure -> az        (https://learn.microsoft.com/cli/azure/install-azure-cli)
#   - Storage credentials in env (same chain the daemon uses).
#   - Root/sudo access (nvme connect + raw /dev/nvmeXn1 access require root).
#
# Usage (invoke from repo root):
#   THURVSA_TEST_BACKEND=primary ./vsa/scripts/test-fs-nvmetcp-storage.sh [OPTIONS]
#
# NOTE on credentials: from a fresh checkout, drop your maintainer
# storage credentials into `$REPO/private/thur.env` (KEY=VAL per line,
# AWS_* / GOOGLE_* / AZURE_* / per-backend `auth: env` names like
# AISTOR_*) and your backend entry in `$REPO/private/storage-backends.json`.
# The script auto-sources thur.env at startup and defaults
# THURVSA_SOURCE_BACKENDS to private/storage-backends.json.
#
# NOTE on sudo: do NOT prefix with sudo — the script self-elevates
# via `sudo KEY=VAL ... "$0"`, forwarding the backend-relevant env
# vars one by one (sudo-rs on Ubuntu 26.04+ silently ignores `-E`
# regardless of the SETENV sudoers tag).
#
# Options:
#   --release             Use ./target/release/ binaries (default: debug)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up local test data directory
#   --keep-nvme           Don't disconnect NVMe session after tests
#   --keep-storage          Don't purge the test sub-prefix from the bucket
#   --nvmetcp-port PORT   Override NVMe/TCP port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials if the file exists.
# Must run BEFORE self-elevation so the env is populated when sudo
# forwards it.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding backend-relevant env vars as
# explicit `KEY=VAL` pairs. `sudo -E` is silently ignored on sudo-rs
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

SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.json}"
TEST_DIR="/tmp/thurvsa-test-fs-nvmetcp-storage-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
NVMETCP_PORT=""
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-fs-cloud-test"
KEEP_NVME=0
KEEP_STORAGE=0
NVME_CONNECTED=0
NVME_DEVICE=""
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-cloud"
FIXTURE_MB="${THURVSA_FIXTURE_MB:-8}"
if (( FIXTURE_MB < 8 )); then FIXTURE_MB=8; fi
VOLUME_SIZE_MIB=$(( FIXTURE_MB * 3 + 16 ))
FIXTURE_DIR="${TEST_DIR}/fixture"
FIXTURE_TAR="${TEST_DIR}/fixture.tar"
FIXTURE_HASH_BEFORE="${TEST_DIR}/fixture-hash-before.txt"
FIXTURE_HASH_AFTER="${TEST_DIR}/fixture-hash-after.txt"

YQ_FLAVOR=""
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
        --keep-nvme) KEEP_NVME=1; shift ;;
        --keep-storage) KEEP_STORAGE=1; shift ;;
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

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi

    if [[ $NVME_CONNECTED -eq 1 && $KEEP_NVME -eq 0 ]]; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    fi

    stop_thur_daemon

    if [[ $KEEP_STORAGE -eq 0 && -n "$BACKEND_TYPE" && -n "$TEST_PREFIX" ]]; then
        log_info "Purging storage test prefix: ${BACKEND_BUCKET:-?}/${TEST_PREFIX}"
        storage_purge_test_prefix
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

free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

assign_ports_nvme() {
    [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(free_port)
    [[ -z "$HTTP_PORT"    ]] && HTTP_PORT=$(free_port)
    log_info "Using NVMe/TCP port $NVMETCP_PORT, HTTP port $HTTP_PORT"
}

# ---------------------------------------------------------------------------
# Backend resolution (cloned from test-fs-iscsi-storage.sh — same shape, NVMe
# variant has its own copy to keep both scripts self-contained)
# ---------------------------------------------------------------------------

resolve_backend() {
    if [[ -z "$THURVSA_TEST_BACKEND" ]]; then
        log_error "THURVSA_TEST_BACKEND is not set."
        echo "Set it to the name of an entry in $SOURCE_BACKENDS"
        echo "Example: THURVSA_TEST_BACKEND=primary $0"
        exit 1
    fi
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Cannot read source backends file: $SOURCE_BACKENDS"
        echo "Override with THURVSA_SOURCE_BACKENDS=<path>/storage-backends.json"
        exit 1
    fi
    if ! command -v jq >/dev/null 2>&1; then
        log_error "jq is required to parse $SOURCE_BACKENDS"
        exit 1
    fi

    local exists
    exists=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" || "$exists" == "null" ]]; then
        log_error "Backend '$THURVSA_TEST_BACKEND' not found in $SOURCE_BACKENDS"
        echo "Available backends:"
        jq -r '.backends | keys | .[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi

    BACKEND_TYPE=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
    BACKEND_BUCKET=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
    ORIG_PREFIX=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_AKID_ENV=$(jq -r "
        .backends.\"$THURVSA_TEST_BACKEND\".auth
        | select(.type == \"env\") | .access_key_id_env // \"\"
    " "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(jq -r "
        .backends.\"$THURVSA_TEST_BACKEND\".auth
        | select(.type == \"env\") | .secret_access_key_env // \"\"
    " "$SOURCE_BACKENDS")
    local retention
    retention=$(jq -r ".backends.\"$THURVSA_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")

    if [[ "$BACKEND_TYPE" == "local" ]]; then
        log_error "Backend '$THURVSA_TEST_BACKEND' has type 'local' — use test-fs-nvmetcp.sh for local-backend coverage."
        exit 1
    fi
    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$THURVSA_TEST_BACKEND' has retention_mode='$retention' — refusing to run."
        echo "Object Lock / retention policy would prevent the test from purging its own data."
        exit 1
    fi

    RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
    local prefix_clean="${ORIG_PREFIX%/}"
    if [[ -n "$prefix_clean" ]]; then
        TEST_PREFIX="${prefix_clean}/test-runs/${RUN_ID}/"
    else
        TEST_PREFIX="test-runs/${RUN_ID}/"
    fi

    log_info "Source backend:    $THURVSA_TEST_BACKEND (type=$BACKEND_TYPE)"
    log_info "Bucket/container:  ${BACKEND_BUCKET}${BACKEND_CONTAINER}"
    log_info "Test sub-prefix:   $TEST_PREFIX"
}

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
        [nvme]="sudo apt-get install nvme-cli"
        [mkfs.ext4]="sudo apt-get install e2fsprogs"
        [fsck.ext4]="sudo apt-get install e2fsprogs"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [tar]="(present on every distro)"
        [curl]="sudo apt-get install curl"
        [jq]="sudo apt-get install jq"
        [sha256sum]="sudo apt-get install coreutils"
    )
    for tool in nvme mkfs.ext4 fsck.ext4 mount umount tar curl jq sha256sum; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
        missing+=("nvme_tcp kernel module")
        hints+=("  - nvme_tcp: sudo modprobe nvme_tcp (kernel >= 5.0 required)")
    fi

    local cli; cli=$(storage_cli_for_type)
    if [[ -n "$cli" && "$cli" != "unknown" ]] && ! command -v "$cli" >/dev/null 2>&1; then
        missing+=("$cli")
        case "$cli" in
            aws)    hints+=("  - aws (cleanup + assertions for type=s3): sudo apt-get install awscli") ;;
            gcloud) hints+=("  - gcloud (cleanup + assertions for type=gcs): https://cloud.google.com/sdk") ;;
            az)     hints+=("  - az (cleanup + assertions for type=azure): https://learn.microsoft.com/cli/azure/install-azure-cli") ;;
        esac
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        echo "Install hints:"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if ! lsmod | grep -q '^nvme_tcp\b'; then
        log_info "Loading nvme_tcp kernel module"
        modprobe nvme_tcp || { log_error "Failed to load nvme_tcp"; exit 1; }
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration (storage backend cloned from $SOURCE_BACKENDS)..."
    mkdir -p "$TEST_DIR/data/volumes" "$MOUNT_POINT"

    local backend_json
    backend_json=$(jq -c \
        ".backends.\"$THURVSA_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS")
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

transport: nvmetcp

http:
  listen: "127.0.0.1:$HTTP_PORT"

nvmetcp:
  listen: "0.0.0.0:$NVMETCP_PORT"

audit:
  enabled: true

storage:
  backends:
    testbackend: $backend_json
EOFCONFIG

    if ! grep -q '^data_dir:' "$TEST_CONFIG"; then
        log_error "Generated test config is missing data_dir:"
        cat "$TEST_CONFIG" | sed 's/^/  /'
        exit 1
    fi
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad (NVMe/TCP)..."
    RUST_LOG="info,nvme_tcp=debug" \
        "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in $(seq 1 30); do
        if ss -tln 2>/dev/null | grep -q ":$NVMETCP_PORT\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $NVMETCP_PORT)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon failed to bind port $NVMETCP_PORT"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

stop_daemon() {
    stop_thur_daemon
}

ensure_volume() {
    if "$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null | grep -q "$VOLUME_NAME"; then
        log_info "Volume $VOLUME_NAME already present (reusing across restarts)"
        return 0
    fi
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB on testbackend)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size "${VOLUME_SIZE_MIB}M" --backend testbackend --dedup local >/dev/null
}

connect_nvme() {
    log_info "Connecting via nvme-cli..."
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
    if [[ -z "$NVME_DEVICE" ]]; then
        log_error "Could not locate the connected NVMe controller"
        return 1
    fi
    local block="/dev/${NVME_DEVICE}n1"
    [[ -b "$block" ]] || { log_error "$block is not a block device"; return 1; }
    log_info "thurvsa namespace -> $block"
}

disconnect_nvme() {
    if [[ $NVME_CONNECTED -eq 1 ]]; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
        NVME_CONNECTED=0
        sleep 1
    fi
}

generate_fixture() {
    log_info "Generating ${FIXTURE_MB} MiB fixture tree (mixed text + random)..."
    mkdir -p "$FIXTURE_DIR/text" "$FIXTURE_DIR/random"
    local text_files=$(( FIXTURE_MB * 4 ))    # ~16 KiB each
    local rand_files=$(( FIXTURE_MB / 2 ))    # 256 KiB each
    [[ $rand_files -lt 1 ]] && rand_files=1
    for i in $(seq 1 "$text_files"); do
        head -c 16384 /dev/urandom | base64 > "$FIXTURE_DIR/text/text-$i.txt"
    done
    for i in $(seq 1 "$rand_files"); do
        dd if=/dev/urandom of="$FIXTURE_DIR/random/blob-$i.bin" bs=64K count=4 status=none
    done
    tar -cf "$FIXTURE_TAR" -C "$FIXTURE_DIR" .
    log_info "Fixture tar at $FIXTURE_TAR ($(stat -c%s "$FIXTURE_TAR") bytes)"
}

phase_a_format_mount_extract() {
    local block="/dev/${NVME_DEVICE}n1"
    log_info "[Phase A] mkfs.ext4 + mount + tar xf + sync on $block"
    if ! mkfs.ext4 -F -q "$block"; then
        log_error "[Phase A] mkfs.ext4 failed on $block"
        return 1
    fi
    mount "$block" "$MOUNT_POINT"
    tar -xf "$FIXTURE_TAR" -C "$MOUNT_POINT"
    sync
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_BEFORE"
    log_info "[Phase A] hashed $(wc -l < "$FIXTURE_HASH_BEFORE") files"
    umount "$MOUNT_POINT"
    log_info "[Phase A] umounted cleanly"
}

phase_b_assert_storage_objects() {
    log_info "[Phase B] Asserting chunk objects landed in storage..."
    # Async-upload health gate — same shape as the iSCSI storage test.
    # See vsa/scripts/test-fs-iscsi-storage.sh phase_b for rationale.
    if grep -qE "backend '[^']+' unknown" "${TEST_DIR}/daemon.log"; then
        log_error "[Phase B] upload-worker logged 'backend unknown' — async upload path is dropping PUTs"
        grep -E "backend '[^']+' unknown" "${TEST_DIR}/daemon.log" | head -5 | sed 's/^/    /'
        return 1
    fi
    local info_json host_bw backend_bw
    info_json=$("$CLI_PATH" --config "$TEST_CONFIG" volume info "$VOLUME_NAME" --json 2>/dev/null) || {
        log_error "[Phase B] volume info '$VOLUME_NAME' failed"
        return 1
    }
    host_bw=$(echo "$info_json" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("runtime",{}).get("host_bytes_written",0))')
    backend_bw=$(echo "$info_json" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("runtime",{}).get("backend_bytes_written",0))')
    log_info "[Phase B] runtime counters: host_bytes_written=$host_bw  backend_bytes_written=$backend_bw"
    if [[ "${host_bw:-0}" -le 0 ]]; then
        log_error "[Phase B] host_bytes_written=$host_bw — host writes never reached the daemon"
        return 1
    fi
    if [[ "${backend_bw:-0}" -le 0 ]]; then
        log_error "[Phase B] backend_bytes_written=$backend_bw with host_bytes_written=$host_bw — uploads silently dropped"
        return 1
    fi
    # SYNCHRONIZE CACHE / NVMe Flush on umount drains in-flight
    # uploads, but the storage bucket may take a beat to be listable.
    # storage_wait_for_key blocks until at least one object shows up.
    if ! storage_wait_for_key "" 60; then
        log_error "[Phase B] No objects appeared under ${BACKEND_BUCKET}/${TEST_PREFIX} within 60s"
        return 1
    fi
    local count
    count=$(storage_list "" | wc -l)
    log_info "[Phase B] Found $count object(s) under test prefix"
    return 0
}

phase_c_restart_and_verify() {
    log_info "[Phase C] Disconnecting NVMe, stopping daemon, restarting..."
    disconnect_nvme
    stop_daemon
    sync && echo 3 > /proc/sys/vm/drop_caches
    start_daemon
    if ! connect_nvme; then
        log_error "[Phase C] reconnect failed"
        return 1
    fi

    local block="/dev/${NVME_DEVICE}n1"
    log_info "[Phase C] fsck.ext4 -fn (must be clean)"
    if ! fsck.ext4 -fn "$block"; then
        log_error "fsck.ext4 reported inconsistency on $block"
        return 1
    fi
    mount "$block" "$MOUNT_POINT"
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_AFTER"
    umount "$MOUNT_POINT"
    if diff -q "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" >/dev/null; then
        log_info "[Phase C] all files round-tripped byte-for-byte across restart"
        return 0
    fi
    log_error "[Phase C] file hashes differ across restart"
    diff -u "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" | head -40
    return 1
}

main() {
    echo "========================================"
    echo "thurvsa Filesystem Workflow Test (NVMe/TCP + storage backend)"
    echo "========================================"
    echo ""

    resolve_backend
    check_prerequisites
    verify_storage_creds || {
        echo ""
        echo "Common cause: storage credentials aren't in this shell's env."
        echo "Set them in your user shell, then run (without sudo prefix):"
        echo "  THURVSA_TEST_BACKEND=$THURVSA_TEST_BACKEND $0 $*"
        exit 1
    }
    assign_ports_nvme
    create_test_config
    start_daemon
    ensure_volume
    if ! connect_nvme; then
        log_fail "Could not establish NVMe/TCP session"
        tail -30 "$TEST_DIR/daemon.log"
        exit 1
    fi
    generate_fixture

    echo ""
    log_test "Phase A — mkfs.ext4 + tar xf + sync + hash"
    if phase_a_format_mount_extract; then log_pass "Phase A"; else log_fail "Phase A"; exit 1; fi
    echo ""
    log_test "Phase B — assert chunk objects landed in storage"
    if phase_b_assert_storage_objects; then log_pass "Phase B"; else log_fail "Phase B"; exit 1; fi
    echo ""
    log_test "Phase C — restart daemon + NVMe reconnect + fsck + diff hashes"
    if phase_c_restart_and_verify; then log_pass "Phase C"; else log_fail "Phase C"; exit 1; fi

    echo ""
    echo "========================================"
    echo "All workflow phases passed against ${BACKEND_TYPE} backend"
    echo "========================================"
    echo "Artifacts:"
    echo "  - Daemon log: ${TEST_DIR}/daemon.log"
    echo "  - Fixture:    ${FIXTURE_TAR}"
    echo "  - Hashes:     ${FIXTURE_HASH_BEFORE} / ${FIXTURE_HASH_AFTER}"
    exit 0
}

main
