#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa End-to-End Filesystem Workflow Test (real storage backend)
#
# Same shape as test-fs-iscsi.sh, but instead of pointing at the
# `local` backend, this clones a backend definition from
# /etc/thurvsa/thurvsa.yaml so the upload pipeline, HEAD-then-PUT
# dedup, page-eviction-driven storage refetch, and SYNC-fenced flush
# paths actually fire against a real storage backend.
#
# Selection: set THURVSA_TEST_BACKEND to the name of an entry under
# `storage.backends:` in /etc/thurvsa/thurvsa.yaml. The script copies that
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
#   THURVSA_TEST_BACKEND=primary THURVSA_FIXTURE_MB=128 ./vsa/scripts/test-fs-iscsi-storage.sh
#
# Prerequisites:
#   - sg3-utils, open-iscsi, lsscsi, e2fsprogs, util-linux, tar
#   - yq (kislyuk/yq — the jq-based Python wrapper; uses jq syntax)
#   - Backend CLI matching the backend type:
#       s3    -> aws       (sudo apt-get install awscli  OR  pip install awscli)
#       gcs   -> gcloud    (https://cloud.google.com/sdk)
#       azure -> az        (https://learn.microsoft.com/cli/azure/install-azure-cli)
#   - Storage credentials in env (same chain the daemon uses).
#   - Root/sudo access (iSCSI + /dev/sdX).
#
# Usage (invoke from repo root):
#   THURVSA_TEST_BACKEND=primary ./vsa/scripts/test-fs-iscsi-storage.sh [OPTIONS]
#
# NOTE on credentials: from a fresh checkout, drop your maintainer
# storage credentials into `$REPO/private/thur.env` (KEY=VAL per line,
# AWS_* / GOOGLE_* / AZURE_* / per-backend `auth: env` names like
# AISTOR_*) and your backend entry in `$REPO/private/storage-backends.yaml`.
# The script auto-sources thur.env at startup and defaults
# THURVSA_SOURCE_BACKENDS to private/storage-backends.yaml, so you
# don't need either piece installed under /etc or /var/lib — every
# read happens out of the repo, every write under /tmp.
#
# NOTE on sudo: do NOT prefix with sudo — the script self-elevates
# via `sudo KEY=VAL ... "$0"`, forwarding the backend-relevant env
# vars one by one (sudo-rs on Ubuntu 26.04+ silently ignores `-E`
# regardless of the SETENV sudoers tag, so explicit pass-through is
# the only portable path). If you must run as root directly, set the
# env vars in root's shell first.
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up local test data directory
#   --keep-iscsi          Don't disconnect iSCSI session after tests
#   --keep-storage          Don't purge the test sub-prefix from the bucket
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials if the file exists.
# `set -a` auto-exports every KEY=VAL so the subsequent `sudo -E`
# carries them across; `set +a` restores normal scoping. Skipped on
# packaged installs (no private/ dir) — operators put credentials in
# /etc/thurvsa/thurvsa.env there, picked up by the systemd unit. This
# block has to run BEFORE self-elevation so the env is populated
# when sudo -E forwards it.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding the backend-relevant env vars as
# explicit `KEY=VAL` pairs. `sudo -E` looks tempting but is silently
# ignored on sudo-rs (Ubuntu 26.04+) regardless of the SETENV tag
# in sudoers; explicit forwarding is the only portable path. Pattern-
# based so a new per-backend `auth: env` prefix (AISTOR_, WASABI_,
# etc.) auto-forwards — only a wholly new credential prefix needs a
# one-word `case` addition below.
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

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
# Storage backend definitions live in `<data_dir>/storage-backends.json`
# (daemon-owned). The script extracts the chosen entry from
# $SOURCE_BACKENDS and embeds it under `testbackend` inside the
# generated test config. Default points at the maintainer-private
# `private/storage-backends.yaml` so running from a fresh checkout
# requires no host-side setup; override with THURVSA_SOURCE_BACKENDS
# to point at a packaged install path (e.g. /var/lib/thurvsa/...).
#
# The daemon's *own* YAML config (data_dir, ports, IQN tuning) is
# generated fresh under $TEST_DIR/config.yaml — the script never
# reads /etc/thurvsa/thurvsa.yaml. So everything below the
# storage-backends.yaml read happens entirely under /tmp.
SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
TEST_DIR="/tmp/thurvsa-test-fs-iscsi-storage-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
KEEP_DATA=0
KEEP_ISCSI=0
KEEP_STORAGE=0
DAEMON_PID=""
ISCSI_CONNECTED=0
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-cloud"
FIXTURE_MB="${THURVSA_FIXTURE_MB:-8}"
if (( FIXTURE_MB < 8 )); then FIXTURE_MB=8; fi
# Volume size headroom over the fixture: ~3x so ext4 metadata + journal
# + the tar stream all fit comfortably without forcing the test to
# stress the volume cap.
VOLUME_SIZE_MIB=$(( FIXTURE_MB * 3 + 16 ))
FIXTURE_DIR="${TEST_DIR}/fixture"
FIXTURE_TAR="${TEST_DIR}/fixture.tar"
FIXTURE_HASH_BEFORE="${TEST_DIR}/fixture-hash-before.txt"
FIXTURE_HASH_AFTER="${TEST_DIR}/fixture-hash-after.txt"
RW_DEVICE=""

BACKEND_TYPE=""
BACKEND_BUCKET=""
BACKEND_ENDPOINT=""
BACKEND_REGION=""
BACKEND_ACCOUNT=""
BACKEND_CONTAINER=""
ORIG_PREFIX=""
TEST_PREFIX=""
RUN_ID=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --keep-storage) KEEP_STORAGE=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

# ---------------------------------------------------------------------------
# Storage helpers are sourced from scripts/lib/test-helpers.sh (storage_list /
# storage_wait_for_key / verify_storage_creds / storage_purge_test_prefix /
# storage_cli_for_type). Lifted in 2026-05-13.
# ---------------------------------------------------------------------------

cleanup() {
    local rc=$?
    log_info "Cleaning up..."

    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi

    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
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

# ---------------------------------------------------------------------------
# Backend resolution
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
        echo "Override with THURVSA_SOURCE_BACKENDS=<path>/storage-backends.yaml"
        exit 1
    fi
    if ! command -v yq >/dev/null 2>&1; then
        log_error "yq is required to parse $SOURCE_BACKENDS"
        exit 1
    fi

    local exists
    exists=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" || "$exists" == "null" ]]; then
        log_error "Backend '$THURVSA_TEST_BACKEND' not found in $SOURCE_BACKENDS"
        echo "Available backends:"
        yq -r '.storage.backends | keys | .[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi

    BACKEND_TYPE=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
    BACKEND_BUCKET=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
    ORIG_PREFIX=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")
    # If the backend has `auth: { type: env, ... }` carrying explicit
    # env-var names, capture them so the cred probe and cleanup target
    # the same credentials the daemon will use. See VTL twin script
    # for the full rationale (mixing real-AWS with MinIO/AIStor in one
    # daemon needs explicit per-backend auth on the non-AWS one).
    BACKEND_AUTH_AKID_ENV=$(yq -r "
        .storage.backends.\"$THURVSA_TEST_BACKEND\".auth
        | select(.type == \"env\") | .access_key_id_env // \"\"
    " "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(yq -r "
        .storage.backends.\"$THURVSA_TEST_BACKEND\".auth
        | select(.type == \"env\") | .secret_access_key_env // \"\"
    " "$SOURCE_BACKENDS")
    local retention
    retention=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")

    if [[ "$BACKEND_TYPE" == "local" ]]; then
        log_error "Backend '$THURVSA_TEST_BACKEND' has type 'local' — use test-fs-iscsi.sh for local-backend coverage."
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
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [mkfs.ext4]="sudo apt-get install e2fsprogs"
        [fsck.ext4]="sudo apt-get install e2fsprogs"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [tar]="(present on every distro)"
        [curl]="sudo apt-get install curl"
        [yq]="sudo apt-get install yq  OR  pip install yq  (kislyuk/yq — the jq-based wrapper)"
    )
    for tool in iscsiadm lsscsi mkfs.ext4 fsck.ext4 mount umount tar curl yq; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    local cli; cli=$(storage_cli_for_type)
    if [[ -n "$cli" ]] && ! command -v "$cli" >/dev/null 2>&1; then
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

    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service is not running."
        echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
        exit 1
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration (storage backend cloned from $SOURCE_BACKENDS)..."
    mkdir -p "$TEST_DIR/data/volumes" "$MOUNT_POINT"

    local backend_json
    backend_json=$(yq -c \
        ".storage.backends.\"$THURVSA_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS")
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
    DAEMON_LOG_MODE=append start_thur_daemon
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

connect_iscsi() {
    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    sleep 3
    local row
    row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
    [[ -n "$row" ]] || { log_error "No THUR VSA device found"; lsscsi -g; exit 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; exit 1; }
    log_info "thurvsa LUN -> $RW_DEVICE"
}

disconnect_iscsi() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete  >/dev/null 2>&1 || true
        ISCSI_CONNECTED=0
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

# ---------------------------------------------------------------------------
# Phase A — write workload
# ---------------------------------------------------------------------------

phase_a_format_mount_extract() {
    log_info "[Phase A] mkfs.ext4 + mount + tar xf + sync"
    mkfs.ext4 -F -q "$RW_DEVICE"
    mount "$RW_DEVICE" "$MOUNT_POINT"
    tar -xf "$FIXTURE_TAR" -C "$MOUNT_POINT"
    sync
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_BEFORE"
    log_info "[Phase A] hashed $(wc -l < "$FIXTURE_HASH_BEFORE") files"
    umount "$MOUNT_POINT"
    log_info "[Phase A] umounted cleanly"
}

phase_b_assert_storage_objects() {
    log_info "[Phase B] Asserting chunk objects landed in storage..."
    # Async-upload health gate. Catches the upload-worker-snapshot
    # regression where a backend instantiated by runtime `volume
    # create` is invisible to the worker, every PUT silently no-op's
    # into LocalOnly, and storage_list below still returns >=1 only
    # because the crash-recovery scan replays LocalOnly markers on the
    # NEXT daemon start. Asserting on the warn line catches it before
    # phase C masks it.
    if grep -qE "backend '[^']+' unknown" "${TEST_DIR}/daemon.log"; then
        log_error "[Phase B] upload-worker logged 'backend unknown' — async upload path is dropping PUTs"
        grep -E "backend '[^']+' unknown" "${TEST_DIR}/daemon.log" | head -5 | sed 's/^/    /'
        return 1
    fi
    # backend_bytes_written must track host writes — a flat counter
    # means uploads no-op'd. Read via admin socket (live cache).
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
    local count
    count=$(storage_list "" | wc -l)
    if (( count < 1 )); then
        log_error "[Phase B] No objects found under ${BACKEND_BUCKET}/${TEST_PREFIX}"
        return 1
    fi
    log_info "[Phase B] Found $count object(s) under test prefix"
    return 0
}

phase_c_restart_and_verify() {
    log_info "[Phase C] Disconnecting iSCSI, stopping daemon, restarting..."
    disconnect_iscsi
    stop_daemon
    start_daemon
    connect_iscsi

    log_info "[Phase C] fsck.ext4 -fn (must be clean)"
    if ! fsck.ext4 -fn "$RW_DEVICE"; then
        log_error "fsck.ext4 reported inconsistency on $RW_DEVICE"
        return 1
    fi
    mount "$RW_DEVICE" "$MOUNT_POINT"
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
    echo "thurvsa Filesystem Workflow Test (real storage backend)"
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
    assign_ports
    create_test_config
    start_daemon
    ensure_volume
    connect_iscsi
    generate_fixture

    echo ""
    log_test "Phase A — mkfs.ext4 + tar xf + sync + hash"
    if phase_a_format_mount_extract; then log_pass "Phase A"; else log_fail "Phase A"; exit 1; fi
    echo ""
    log_test "Phase B — assert chunk objects landed in storage"
    if phase_b_assert_storage_objects; then log_pass "Phase B"; else log_fail "Phase B"; exit 1; fi
    echo ""
    log_test "Phase C — restart daemon + iSCSI + fsck + diff hashes"
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
