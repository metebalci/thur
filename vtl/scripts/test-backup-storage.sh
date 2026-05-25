#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL End-to-End Backup Workflow Test (cloud)
#
# Same shape as test-backup-workflow.sh, but instead of pointing at the
# `local` backend, this borrows a backend definition from
# /etc/thurvtl/thurvtl.yaml so the upload pipeline, HEAD-then-PUT
# dedup, manifest backup, and refetch-on-eviction paths actually fire
# against real cloud.
#
# Selection: set THURVTL_TEST_BACKEND to the name of an entry under
# `cloud.backends:` in /etc/thurvtl/thurvtl.yaml. The script copies
# that entry verbatim into the test config and appends a per-run
# sub-prefix so test data is isolated and trivially purgeable.
#
# Refusals:
#   - Backend `type: local`         (defeats the purpose)
#   - Backend `retention_mode != none` (test cleanup can't delete locked objects)
#
# Cleanup: always purges the test sub-prefix from the bucket on exit
# (even on failure), unless --keep-cloud is passed.
#
# Stress / scale runs: bump the fixture via THURVTL_FIXTURE_MB (env,
# MiB per cartridge, default 8). The chunk-count and dedup assertions
# scale with the fixture; the manifest-backup wait window grows with
# it. Cloud round-trips dominate runtime — bigger fixture = longer
# test = real egress $$$. Run the larger sizes opt-in.
#   THURVTL_TEST_BACKEND=primary THURVTL_FIXTURE_MB=512 ./vtl/scripts/test-backup-storage.sh
#
# Prerequisites:
#   - mtx, mt-st, open-iscsi, tar, lsscsi, curl   (same as test-backup-workflow.sh)
#   - yq                                            (yaml extraction)
#   - The cloud CLI matching the backend type:
#       s3    -> aws       (sudo apt-get install awscli  OR  pip install awscli)
#       gcs   -> gcloud    (https://cloud.google.com/sdk)
#       azure -> az        (https://learn.microsoft.com/cli/azure/install-azure-cli)
#   - Cloud credentials in env (same chain the daemon uses).
#   - Root/sudo (iSCSI + /dev/stN).
#
# Usage (invoke from repo root):
#   THURVTL_TEST_BACKEND=primary ./vtl/scripts/test-backup-storage.sh [OPTIONS]
#
# NOTE on credentials: from a fresh checkout, drop your maintainer
# cloud creds into `$REPO/private/thur.env` (KEY=VAL per line,
# AWS_* / GOOGLE_* / AZURE_* / per-backend `auth: env` names like
# AISTOR_*) and your backend entry in `$REPO/private/storage-backends.yaml`.
# The script auto-sources thur.env at startup and defaults
# THURVTL_SOURCE_BACKENDS to private/storage-backends.yaml, so you
# don't need either piece installed under /etc or /var/lib — every
# read happens out of the repo, every write under /tmp.
#
# NOTE on sudo: do NOT prefix with sudo — the script self-elevates
# via `sudo KEY=VAL ... "$0"`, forwarding the cloud-relevant env
# vars one by one (sudo-rs on Ubuntu 26.04+ silently ignores `-E`
# regardless of the SETENV sudoers tag, so explicit pass-through is
# the only portable path). If you must run as root directly, set the
# env vars in root's shell first.
#
# Options:
#   --release             Use ./target/release/ binaries (default is ./target/debug/)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --keep-data           Don't clean up local test data directory
#   --keep-iscsi          Don't disconnect iSCSI session after tests
#   --keep-cloud          Don't purge the test sub-prefix from the bucket
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private cloud credentials if the file exists.
# `set -a` auto-exports every KEY=VAL so the subsequent `sudo -E`
# carries them across; `set +a` restores normal scoping. Skipped on
# packaged installs (no private/ dir) — operators put credentials in
# /etc/thurvtl/thurvtl.env there, picked up by the systemd unit. This
# block has to run BEFORE self-elevation so the env is populated
# when sudo -E forwards it.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding the cloud-relevant env vars as
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
            AWS_*|GOOGLE_*|GCS_*|AZURE_*|AISTOR_*|WASABI_*|MINIO_*|THURVTL_*)
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
# requires no host-side setup; override with THURVTL_SOURCE_BACKENDS
# to point at a packaged install path (e.g. /var/lib/thurvtl/...).
#
# The daemon's *own* YAML config (data_dir, ports, IQN, disk_cache
# tuning) is generated fresh under $TEST_DIR/test.yaml — the script
# never reads /etc/thurvtl/thurvtl.yaml. So everything below the
# storage-backends.yaml read happens entirely under /tmp.
SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
TEST_DIR="/tmp/test-backup-storage-$$"

# Fixture-size knob (env: THURVTL_FIXTURE_MB, MiB per cartridge,
# floor 8). Default keeps the smoke run quick (~8 MiB ≈ a handful of
# chunks). Bump for stress/scale runs:
#   THURVTL_FIXTURE_MB=512 ./vtl/scripts/test-backup-storage.sh    # ~64 chunks
#   THURVTL_FIXTURE_MB=1024 ./vtl/scripts/test-backup-storage.sh   # ~128 chunks
# Storage round-trips dominate runtime — bigger fixture = longer test
# = real egress $$$. Run the larger sizes opt-in, not on every commit.
FIXTURE_MB="${THURVTL_FIXTURE_MB:-8}"
if (( FIXTURE_MB < 8 )); then
    FIXTURE_MB=8
fi
# Manifest-backup wait window scales with fixture so a larger upload
# tail isn't false-failed. 120s baseline + 1s/MiB over 8, capped at
# 600s (a 512 MiB run gets ~624 → 600).
MANIFEST_WAIT_SECS=$((120 + (FIXTURE_MB > 8 ? FIXTURE_MB - 8 : 0)))
if (( MANIFEST_WAIT_SECS > 600 )); then MANIFEST_WAIT_SECS=600; fi
# Cross-cartridge dedup ceiling: tape 2 should add no more than this
# many net-new chunks beyond tape 1's count. Loose at small fixtures
# (boundary effects dominate); tight at large fixtures (5% of the
# first-tape chunk count, with a floor of 2). Replaces the smoke
# test's <2x guard which is trivially satisfied.
DEDUP_NEW_CHUNKS_MAX_PCT=5
DEDUP_NEW_CHUNKS_FLOOR=2

# Chunking mode for the test cartridges. Defaults to FastCDC (the
# product's default). Override to `fixed` to isolate the iSCSI/SCSI
# write hot path from FastCDC overhead — at avg 8 MiB cuts under
# random data FastCDC fires near its `min` (1 MiB), generating ~5x
# more chunk seals/persists than expected. Fixed at, e.g., 128 MiB
# yields a handful of chunks per cartridge, making the streaming-
# write throughput easier to reason about.
FIXTURE_CHUNKING="${THURVTL_FIXTURE_CHUNKING:-fastcdc}"
case "$FIXTURE_CHUNKING" in
    fastcdc|fixed) ;;
    *) echo "[ERROR] THURVTL_FIXTURE_CHUNKING must be 'fastcdc' or 'fixed', got '$FIXTURE_CHUNKING'"; exit 1 ;;
esac
# Chunk size knob — interpretation depends on FIXTURE_CHUNKING:
#   - fixed: literal chunk size in MiB (default 128).
#   - fastcdc: avg chunk size in MiB; min/max derived as avg/8 and
#     avg*4 by the CLI's --chunk-size-mb mapping (default 8).
if [[ -n "$THURVTL_FIXTURE_CHUNK_SIZE_MB" ]]; then
    FIXTURE_CHUNK_SIZE_MB="$THURVTL_FIXTURE_CHUNK_SIZE_MB"
elif [[ "$FIXTURE_CHUNKING" == "fixed" ]]; then
    FIXTURE_CHUNK_SIZE_MB=128
else
    FIXTURE_CHUNK_SIZE_MB=8
fi

# Minimum chunks expected per cartridge. Both modes get a conservative
# floor (avg-cut count / 2) that absorbs random-data CDC variance for
# FastCDC and rounds-down expected count for Fixed. Clipped at 1 so a
# small fixture in single-chunk territory still passes.
MIN_CHUNKS_EXPECTED=$(( FIXTURE_MB / (FIXTURE_CHUNK_SIZE_MB * 2) ))
if (( MIN_CHUNKS_EXPECTED < 1 )); then MIN_CHUNKS_EXPECTED=1; fi

# Test-side dedup mode. The script always passes the resolved value
# explicitly (`--dedup local` / `--dedup global`) so the test is
# invariant to CLI-default drift.
#   THURVTL_FIXTURE_DEDUP=0  (default) → `--dedup local`: each
#     cartridge owns a private `chunks/<barcode>/...` namespace,
#     "dedup observed" assertion is skipped (just verifies both
#     namespaces non-empty).
#   THURVTL_FIXTURE_DEDUP=1            → `--dedup global`: shared
#     per-backend pool, "dedup observed across cartridges" runs the
#     ratio math.
FIXTURE_DEDUP="${THURVTL_FIXTURE_DEDUP:-0}"
case "$FIXTURE_DEDUP" in
    0|1) ;;
    *) echo "[ERROR] THURVTL_FIXTURE_DEDUP must be 0 or 1, got '$FIXTURE_DEDUP'"; exit 1 ;;
esac
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
KEEP_ISCSI=0
KEEP_CLOUD=0
DAEMON_PID=""
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
TAPE_DEVICE=""
NOREWIND_DEVICE=""

# Resolved from source yaml
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
        --keep-cloud) KEEP_CLOUD=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

# ---------------------------------------------------------------------------
# Storage helpers are sourced from scripts/lib/test-helpers.sh (cloud_list /
# cloud_wait_for_key / verify_cloud_creds / cloud_purge_test_prefix /
# cloud_cli_for_type). Lifted in 2026-05-13.
# ---------------------------------------------------------------------------

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

    if [[ $KEEP_CLOUD -eq 0 && -n "$BACKEND_TYPE" && -n "$TEST_PREFIX" ]]; then
        log_info "Purging cloud test prefix: ${BACKEND_BUCKET:-?}/${TEST_PREFIX}"
        cloud_purge_test_prefix
    elif [[ $KEEP_CLOUD -eq 1 && -n "$TEST_PREFIX" ]]; then
        log_warn "Keeping cloud test prefix: ${BACKEND_BUCKET:-?}/${TEST_PREFIX}"
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

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

resolve_backend() {
    if [[ -z "$THURVTL_TEST_BACKEND" ]]; then
        log_error "THURVTL_TEST_BACKEND is not set."
        echo "Set it to the name of an entry in $SOURCE_BACKENDS"
        echo "Example: sudo THURVTL_TEST_BACKEND=primary $0"
        exit 1
    fi
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Cannot read source backends file: $SOURCE_BACKENDS"
        echo "Override with THURVTL_SOURCE_BACKENDS=<path>/storage-backends.yaml"
        exit 1
    fi
    if ! command -v yq >/dev/null 2>&1; then
        log_error "yq is required to parse $SOURCE_BACKENDS"
        exit 1
    fi

    local exists
    exists=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" || "$exists" == "null" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' not found in $SOURCE_BACKENDS"
        echo "Available backends:"
        yq -r '.storage.backends | keys | .[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi

    BACKEND_TYPE=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
    BACKEND_BUCKET=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
    ORIG_PREFIX=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")
    # If the backend has `auth: { type: env, ... }` carrying explicit
    # env-var names for the access key / secret, capture them so the
    # cred probe (verify_cloud_creds) can target the same credentials
    # the daemon will use. Otherwise the probe falls back to whatever
    # AWS_* are already in the environment, which on a multi-S3-flavored
    # setup (real AWS + MinIO + AIStor) means the probe accidentally
    # tests the wrong service. Empty when the backend uses the default
    # AWS credential chain.
    BACKEND_AUTH_AKID_ENV=$(yq -r "
        .storage.backends.\"$THURVTL_TEST_BACKEND\".auth
        | select(.type == \"env\") | .access_key_id_env // \"\"
    " "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(yq -r "
        .storage.backends.\"$THURVTL_TEST_BACKEND\".auth
        | select(.type == \"env\") | .secret_access_key_env // \"\"
    " "$SOURCE_BACKENDS")
    local retention
    retention=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")

    if [[ "$BACKEND_TYPE" == "local" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' has type 'local' — use test-backup-workflow.sh for local-backend coverage."
        exit 1
    fi

    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$THURVTL_TEST_BACKEND' has retention_mode='$retention' — refusing to run."
        echo "Object Lock / retention policy would prevent the test from purging its own data."
        echo "Use a separate non-locked backend for testing."
        exit 1
    fi

    RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
    local prefix_clean="${ORIG_PREFIX%/}"
    if [[ -n "$prefix_clean" ]]; then
        TEST_PREFIX="${prefix_clean}/test-runs/${RUN_ID}/"
    else
        TEST_PREFIX="test-runs/${RUN_ID}/"
    fi

    log_info "Source backend:    $THURVTL_TEST_BACKEND (type=$BACKEND_TYPE)"
    log_info "Bucket/container:  ${BACKEND_BUCKET}${BACKEND_CONTAINER}"
    log_info "Test sub-prefix:   $TEST_PREFIX"
}

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
        [mtx]="sudo apt-get install mtx"
        [mt]="sudo apt-get install mt-st"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [tar]="(install via your package manager)"
        [lsscsi]="sudo apt-get install lsscsi"
        [curl]="sudo apt-get install curl"
        [yq]="sudo apt-get install yq  OR  pip install yq  (kislyuk/yq — the jq-based wrapper)"
    )
    for tool in mtx mt iscsiadm tar lsscsi curl yq; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    local cli; cli=$(cloud_cli_for_type)
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

    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running."
            echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
            exit 1
        fi
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration (cloud backend cloned from $SOURCE_BACKENDS)..."
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

    # Build the test config. `disk_cache.disk_free_min_gb: 0` — test
    # data_dir is under /tmp, which is commonly a small tmpfs on dev
    # boxes; the production 5 GB free-floor default would block every
    # chunk-seal that calls try_reserve, surfacing as SCSI NOT READY
    # 04/07 on the host (which then fails tar -cf with I/O error and
    # looks like a data-path bug). The `cloud.backends:` block embeds
    # one backend extracted from the operator's $SOURCE_BACKENDS file
    # with its prefix overridden to $TEST_PREFIX.
    mkdir -p "$TEST_DIR/data"
    local backend_json
    backend_json=$(yq -c \
        ".storage.backends.\"$THURVTL_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS")
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
disk_cache:
  disk_free_min_gb: 0
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

# `cartridge create` is daemon-routed (admin socket): must run AFTER
# start_daemon so THURVTL_ADMIN_SOCKET is in scope.
create_cartridges() {
    local dedup_mode="local"
    if [[ "$FIXTURE_DEDUP" == "1" ]]; then
        dedup_mode="global"
    fi
    log_info "Creating cartridges TAPE01L8 / TAPE02L8 on backend testbackend (chunking=$FIXTURE_CHUNKING, size=${FIXTURE_CHUNK_SIZE_MB} MiB, dedup=$dedup_mode)..."
    for bc in TAPE01L8 TAPE02L8; do
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$bc" \
            --lto-generation 8 --backend testbackend \
            --chunking "$FIXTURE_CHUNKING" --chunk-size-mb "$FIXTURE_CHUNK_SIZE_MB" \
            --dedup "$dedup_mode" >/dev/null; then
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
        curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null && { log_info "Daemon ready"; return 0; }
        sleep 1
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

stop_daemon() {
    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
}

connect_iscsi() {
    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    sleep 3

    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; exit 1; }
    log_info "Changer device: $CHANGER_DEVICE"

    TAPE_DEVICE=$(lsscsi | awk '/tape/{print $NF}' | head -1)
    [[ -n "$TAPE_DEVICE" ]] || { log_error "Tape device not found"; lsscsi; exit 1; }
    NOREWIND_DEVICE=$(echo "$TAPE_DEVICE" | sed 's|/dev/st|/dev/nst|')
    log_info "Tape device: $TAPE_DEVICE (no-rewind: $NOREWIND_DEVICE)"

    log_info "Warming up SCSI path with mtx status..."
    mtx -f "$CHANGER_DEVICE" status >"${TEST_DIR}/mtx-initial-status.txt" 2>&1 || true
    mt -f "$NOREWIND_DEVICE" status >"${TEST_DIR}/mt-initial-status.txt" 2>&1 || true
}

disconnect_iscsi() {
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete >/dev/null 2>&1 || true
        ISCSI_CONNECTED=0
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

# Deterministic fixture (same seed -> same bytes, so dedup can be observed).
# Size driven by $FIXTURE_MB (default 8). At avg 8 MiB FastCDC cuts
# this yields roughly FIXTURE_MB/8 chunks per cartridge.
make_fixture() {
    local dir="$1"
    local seed="$2"
    mkdir -p "$dir"
    echo "fixture seed=$seed mb=$FIXTURE_MB" > "$dir/marker.txt"
    local bytes=$((FIXTURE_MB * 1024 * 1024))
    openssl enc -aes-256-ctr -pass "pass:$seed" -nosalt \
        -in <(head -c "$bytes" /dev/zero) -out "$dir/seeded.bin" 2>/dev/null
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
    make_fixture "$fixture" "shared-seed"
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$fixture" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
}

test_unload_first_tape() {
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || return 1
    mtx -f "$CHANGER_DEVICE" status 2>&1 | grep -q "Data Transfer Element 0:Empty" || return 1
}

test_chunks_uploaded_to_cloud() {
    log_info "Waiting for upload pipeline to drain (manifest backup is the tail signal, up to ${MANIFEST_WAIT_SECS}s)..."
    if ! cloud_wait_for_key "manifests/TAPE01L8/manifest-latest.json" "$MANIFEST_WAIT_SECS"; then
        log_error "Manifest backup for TAPE01L8 never appeared in cloud"
        return 1
    fi
    local chunk_count
    chunk_count=$(cloud_list "chunks/" | wc -l)
    if (( chunk_count < MIN_CHUNKS_EXPECTED )); then
        log_error "Only $chunk_count chunk object(s) under ${TEST_PREFIX}chunks/ — expected >= $MIN_CHUNKS_EXPECTED for ${FIXTURE_MB} MiB fixture"
        return 1
    fi
    log_info "Cloud has $chunk_count chunk object(s) under test prefix (fixture: ${FIXTURE_MB} MiB, expected >= ${MIN_CHUNKS_EXPECTED})"
}

test_load_second_tape_same_data() {
    mtx -f "$CHANGER_DEVICE" load 2 0 >/dev/null 2>&1 || return 1
}

test_write_second_tape_same_fixture() {
    # Same seed -> same plaintext bytes -> chunks should HEAD-skip on the bound backend
    # (cross-cartridge dedup within a backend).
    local fixture="$TEST_DIR/fixture2"
    make_fixture "$fixture" "shared-seed"
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    tar -C "$fixture" -cf "$NOREWIND_DEVICE" . || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
}

test_dedup_across_cartridges() {
    mtx -f "$CHANGER_DEVICE" unload 2 0 >/dev/null 2>&1 || return 1
    log_info "Waiting for second cartridge's manifest backup..."
    if ! cloud_wait_for_key "manifests/TAPE02L8/manifest-latest.json" "$MANIFEST_WAIT_SECS"; then
        log_error "Manifest backup for TAPE02L8 never appeared in cloud"
        return 1
    fi

    # When the cartridges are created with `--dedup local` (FIXTURE_DEDUP=0,
    # the test default), there is by design no cross-cartridge sharing —
    # each cartridge owns a private `chunks/<barcode>/...` namespace.
    # Skip the assertion in that mode; just verify both cartridges'
    # chunks landed.
    if [[ "$FIXTURE_DEDUP" != "1" ]]; then
        local t1_chunks t2_chunks
        t1_chunks=$(cloud_list "chunks/TAPE01L8/" | wc -l)
        t2_chunks=$(cloud_list "chunks/TAPE02L8/" | wc -l)
        log_info "Dedup disabled — TAPE01L8 chunks: $t1_chunks, TAPE02L8 chunks: $t2_chunks (no sharing expected)"
        if (( t1_chunks == 0 || t2_chunks == 0 )); then
            log_error "One or both per-cartridge chunk namespaces are empty under dedup-off"
            return 1
        fi
        return 0
    fi

    # tar streams aren't byte-identical (timestamps in headers), but the
    # bulk of the bytes — the seeded payload — should dedup. The
    # ceiling for net-new chunks contributed by tape 2 is the larger of
    # `first_tape_chunks * DEDUP_NEW_CHUNKS_MAX_PCT%` and
    # `DEDUP_NEW_CHUNKS_FLOOR` (boundary effects dominate at small
    # fixtures; the percentage tightens as the fixture grows).
    local total_chunks first_tape_chunks new_chunks ceiling
    total_chunks=$(cloud_list "chunks/" | wc -l)
    first_tape_chunks=$(yq -r '.chunks | length' "$TEST_DIR/data/tapes/TAPE01L8/manifest.json")
    new_chunks=$((total_chunks - first_tape_chunks))
    ceiling=$(( first_tape_chunks * DEDUP_NEW_CHUNKS_MAX_PCT / 100 ))
    if (( ceiling < DEDUP_NEW_CHUNKS_FLOOR )); then
        ceiling=$DEDUP_NEW_CHUNKS_FLOOR
    fi
    log_info "First-tape chunks: $first_tape_chunks   Total cloud chunks: $total_chunks   New from tape 2: $new_chunks (ceiling $ceiling)"
    if (( new_chunks > ceiling )); then
        log_error "Cross-cartridge dedup weak: tape 2 added $new_chunks net new chunks (ceiling $ceiling = max(${DEDUP_NEW_CHUNKS_MAX_PCT}% of $first_tape_chunks, $DEDUP_NEW_CHUNKS_FLOOR))"
        return 1
    fi
}

test_swap_back_to_first_tape() {
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || return 1
}

test_read_first_tape_lists_match() {
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local listing="$TEST_DIR/listing1.txt"
    tar -tf "$NOREWIND_DEVICE" 2>/dev/null | sort > "$listing"
    local expected="$TEST_DIR/expected1.txt"
    (cd "$TEST_DIR/fixture1" && find . -type f | sort) > "$expected"
    diff -u "$expected" <(grep -v '/$' "$listing")
}

test_extract_first_tape_matches_byte_for_byte() {
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local out="$TEST_DIR/extracted1"
    mkdir -p "$out"
    tar -C "$out" -xf "$NOREWIND_DEVICE" || return 1
    diff -r "$TEST_DIR/fixture1" "$out"
}

# Stops the daemon, wipes the local content-addressed pool, restarts the
# daemon, re-mounts the cartridge, and reads it. If the bytes still match,
# refetch-on-eviction works end-to-end.
test_refetch_after_local_wipe() {
    log_info "Unloading tape and stopping daemon for local-cache wipe..."
    mtx -f "$CHANGER_DEVICE" unload 1 0 >/dev/null 2>&1 || return 1
    disconnect_iscsi
    stop_daemon

    log_info "Wiping local chunk pool at $TEST_DIR/data/chunks/"
    rm -rf "$TEST_DIR/data/chunks/"

    log_info "Restarting daemon..."
    start_daemon
    connect_iscsi

    log_info "Re-loading tape 1 and reading via tar (cold pool — every chunk should refetch)..."
    mtx -f "$CHANGER_DEVICE" load 1 0 >/dev/null 2>&1 || return 1
    mt -f "$NOREWIND_DEVICE" rewind || return 1
    local out="$TEST_DIR/extracted1-refetched"
    mkdir -p "$out"

    # Time the extract and report effective throughput. Useful at any
    # fixture size; essential when bumping THURVTL_FIXTURE_MB to
    # spot regressions in the cloud-prefetch path.
    local t0 t1 elapsed bytes mb_per_s
    t0=$(date +%s)
    tar -C "$out" -xf "$NOREWIND_DEVICE" || return 1
    t1=$(date +%s)
    elapsed=$((t1 - t0))
    if (( elapsed < 1 )); then elapsed=1; fi
    bytes=$(du -sb "$out" | awk '{print $1}')
    mb_per_s=$(( bytes / 1024 / 1024 / elapsed ))
    log_info "Refetch: ${bytes} bytes extracted in ${elapsed}s (~${mb_per_s} MiB/s)"

    diff -r "$TEST_DIR/fixture1" "$out"
}

main() {
    echo "================================================"
    echo "Thur VTL End-to-End Backup Workflow Test (cloud)"
    echo "================================================"
    echo "Fixture: ${FIXTURE_MB} MiB per cartridge   Min chunks expected: ${MIN_CHUNKS_EXPECTED}"
    echo "Chunking: ${FIXTURE_CHUNKING} (${FIXTURE_CHUNK_SIZE_MB} MiB)"
    echo "Dedup ceiling: max(${DEDUP_NEW_CHUNKS_MAX_PCT}% of first-tape chunks, ${DEDUP_NEW_CHUNKS_FLOOR}) net-new chunks from tape 2"
    echo ""

    resolve_backend
    check_prerequisites
    verify_cloud_creds || {
        echo ""
        echo "Common cause: cloud creds aren't in this shell's env."
        echo "Set them in your user shell, then run (without sudo prefix):"
        echo "  THURVTL_TEST_BACKEND=$THURVTL_TEST_BACKEND $0 $*"
        echo "(the script self-elevates via 'sudo KEY=VAL ... \$0' and forwards cloud-prefix env vars)"
        exit 1
    }
    assign_ports
    create_test_config
    start_daemon               # exports THURVTL_ADMIN_SOCKET; required before any cartridge op
    create_cartridges          # daemon-routed: cartridge create binds to backend testbackend
    connect_iscsi

    echo ""
    echo "Running cloud-backed backup-workflow tests..."
    echo "---------------------------------------------"
    echo ""

    run_test "load tape 1 from slot 1 to drive 0"     test_load_first_tape
    run_test "tar archive fixture to tape 1"          test_write_first_tape
    run_test "unload tape 1"                          test_unload_first_tape
    run_test "chunks + manifest landed in cloud"      test_chunks_uploaded_to_cloud
    run_test "load tape 2"                            test_load_second_tape_same_data
    run_test "tar archive same fixture to tape 2"     test_write_second_tape_same_fixture
    run_test "dedup observed across cartridges"       test_dedup_across_cartridges
    run_test "swap back to tape 1"                    test_swap_back_to_first_tape
    run_test "tar -t lists tape 1 contents"           test_read_first_tape_lists_match
    run_test "tar -x tape 1 matches fixture"          test_extract_first_tape_matches_byte_for_byte
    run_test "refetch from cloud after local wipe"    test_refetch_after_local_wipe

    echo "================================================"
    echo "Test Summary"
    echo "================================================"
    echo "Backend:       $THURVTL_TEST_BACKEND ($BACKEND_TYPE)"
    echo "Test prefix:   $TEST_PREFIX"
    echo "Total tests:   $((PASSED + FAILED))"
    echo "Passed:        $PASSED"
    echo "Failed:        $FAILED"
    echo ""

    if [[ $FAILED -eq 0 ]]; then
        log_pass "All cloud-backed backup-workflow tests passed"
        exit 0
    else
        log_fail "$FAILED test(s) failed"
        echo ""
        echo "Debug:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data:  ${TEST_DIR}"
        echo "  - Cloud:      ${BACKEND_BUCKET}${BACKEND_CONTAINER}/${TEST_PREFIX}"
        exit 1
    fi
}

main
