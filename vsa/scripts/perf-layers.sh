#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Pipeline-Layer Throughput Harness
#
# Attribution counterpart of test-pipeline-layers.sh: instead of asserting
# behaviour per layer, this script measures throughput per layer and
# prints a comparison table so the cost of each added pipeline stage is
# visible. Four rows × two fixtures = eight measurements.
#
#   L1 in-process       : PageCache -> VolumeWriter -> ChunkPool ->
#                         LocalBackend (no iSCSI, no daemon)
#                         -> core/sbc/examples/perf_volume_write
#   L2 in-process+cloud : same as L1, configured backend (named in
#                         cloud-backends.json). Default is the
#                         self-managed `local-test` entry the script
#                         creates under $TEST_DIR/local-cloud.
#                         -> core/sbc/examples/perf_volume_cloud
#   L3 iscsi-raw        : daemon + iSCSI + dd to /dev/sdX (raw block,
#                         bypasses any filesystem)
#   L4 iscsi-ext4       : daemon + iSCSI + mkfs.ext4 + mount + dd to a
#                         file + sync + umount (full guest-fs path)
#
# Fixtures (run once per row each):
#   random        : /dev/urandom-seeded 1 GiB file (defeats dedup +
#                   compression -> raw ceiling)
#   compressible  : 512 MiB zeros + 512 MiB repeating ABCDEFGHIJKLMNOP
#                   (matches test-pipeline-layers.sh and the in-process
#                   examples' compressible mode)
#
# Default backend is `local-test` (auto-created at
# $TEST_DIR/local-cloud); override with THURVSA_PERF_BACKEND=<name>
# pointing at an entry in $REPO/private/cloud-backends.json (or
# THURVSA_SOURCE_BACKENDS). Cloud-side cleanup is best-effort via
# cloud_purge_test_prefix when a remote backend is used.
#
# Usage (invoke from repo root):
#   ./vsa/scripts/perf-layers.sh [OPTIONS]
#   THURVSA_PERF_BACKEND=aistor-none ./vsa/scripts/perf-layers.sh
#
# Options:
#   --release             Use ./target/release/ binaries (default: debug)
#   --daemon-path PATH    Path to thurvsad binary
#   --cli-path PATH       Path to thurvsa binary
#   --only ROW            Run a single row (1..4); omit to run all four
#   --total-mb N          Override fixture size (default: 1024)
#   --keep-data           Don't clean up test data dirs
#   --keep-cloud          Don't purge cloud test prefixes (remote only)
#

SCRIPT_DIR_RAW="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR_RAW}/../.." && pwd)"

if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
ONLY_ROW=""
TOTAL_MB=1024
KEEP_DATA=0
KEEP_CLOUD=0
SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/cloud-backends.json}"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --only) ONLY_ROW="$2"; shift 2 ;;
        --total-mb) TOTAL_MB="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-cloud) KEEP_CLOUD=1; shift ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

[[ -z "$DAEMON_PATH" ]] && DAEMON_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvsad"
[[ -z "$CLI_PATH" ]] && CLI_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvsa"
PERF_VOLUME_WRITE="${REPO_DIR}/target/${BUILD_PROFILE}/examples/perf_volume_write"
PERF_VOLUME_CLOUD="${REPO_DIR}/target/${BUILD_PROFILE}/examples/perf_volume_cloud"

if [[ ! -x "$DAEMON_PATH" || ! -x "$CLI_PATH" ]]; then
    log_error "Missing daemon ($DAEMON_PATH) or cli ($CLI_PATH). Run: cargo build${BUILD_PROFILE:+ --release}"
    exit 1
fi
if [[ ! -x "$PERF_VOLUME_WRITE" || ! -x "$PERF_VOLUME_CLOUD" ]]; then
    log_error "Missing perf examples. Run: cargo build${BUILD_PROFILE:+ --release} -p core-block --examples"
    exit 1
fi

PERF_BACKEND_NAME="${THURVSA_PERF_BACKEND:-local-test}"

PERF_BASE_DIR="/tmp/test-vsa-perf-layers-$$"
mkdir -p "$PERF_BASE_DIR"
if [[ -n "$SUDO_USER" ]]; then
    chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$PERF_BASE_DIR"
fi
LOCAL_CLOUD_DIR="$PERF_BASE_DIR/local-cloud"
mkdir -p "$LOCAL_CLOUD_DIR"
PERF_BACKENDS_FILE="$PERF_BASE_DIR/perf-backends.json"
FIXTURE_RANDOM="$PERF_BASE_DIR/perf-random-${TOTAL_MB}m.bin"
FIXTURE_COMPRESSIBLE="$PERF_BASE_DIR/perf-compressible-${TOTAL_MB}m.bin"
FIXTURE_BYTES=$(( TOTAL_MB * 1024 * 1024 ))

# Resolve the chosen backend's coordinates. Two paths:
#   1. local-test (default) -> we synthesize a Local entry and write a
#      stand-alone perf-backends.json. No verify_cloud_creds.
#   2. anything else -> must be a real entry in SOURCE_BACKENDS; we
#      reuse the same cred/probe plumbing test-pipeline-layers.sh uses.
# In both cases the perf-backends.json we write is what L2 reads and
# what the daemon's <data_dir>/cloud-backends.json is seeded from for
# L3 / L4.
if [[ "$PERF_BACKEND_NAME" == "local-test" ]]; then
    BACKEND_TYPE="local"
    BACKEND_BUCKET=""
    BACKEND_ENDPOINT=""
    BACKEND_REGION=""
    BACKEND_ACCOUNT=""
    BACKEND_CONTAINER=""
    BACKEND_AUTH_AKID_ENV=""
    BACKEND_AUTH_SECRET_ENV=""
    ORIG_PREFIX=""
    cat > "$PERF_BACKENDS_FILE" <<EOFJ
{
  "version": 1,
  "backends": {
    "local-test": { "type": "local", "root_dir": "$LOCAL_CLOUD_DIR" }
  }
}
EOFJ
    log_info "VSA perf-layers backend = local-test (self-managed at $LOCAL_CLOUD_DIR)"
else
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "THURVSA_PERF_BACKEND=$PERF_BACKEND_NAME but SOURCE_BACKENDS=$SOURCE_BACKENDS unreadable"
        exit 1
    fi
    BACKEND_TYPE=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".type" "$SOURCE_BACKENDS")
    if [[ "$BACKEND_TYPE" == "null" || -z "$BACKEND_TYPE" ]]; then
        log_error "backend '$PERF_BACKEND_NAME' not found in $SOURCE_BACKENDS"
        exit 1
    fi
    BACKEND_BUCKET=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".container // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_AKID_ENV=$(jq -r "
        .backends.\"$PERF_BACKEND_NAME\".auth
        | select(.type == \"env\") | .access_key_id_env // \"\"
    " "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(jq -r "
        .backends.\"$PERF_BACKEND_NAME\".auth
        | select(.type == \"env\") | .secret_access_key_env // \"\"
    " "$SOURCE_BACKENDS")
    RETENTION=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".retention_mode // \"none\"" "$SOURCE_BACKENDS")
    ORIG_PREFIX=$(jq -r ".backends.\"$PERF_BACKEND_NAME\".prefix // \"\"" "$SOURCE_BACKENDS")
    if [[ "$RETENTION" != "none" ]]; then
        log_error "backend '$PERF_BACKEND_NAME' has retention_mode=$RETENTION; perf-layers refuses (purge would fail)"
        exit 1
    fi
    verify_cloud_creds || exit 1
    log_info "VSA perf-layers backend = $PERF_BACKEND_NAME (type=$BACKEND_TYPE bucket=${BACKEND_BUCKET}${BACKEND_CONTAINER})"
fi

PERF_LINES=()

TEST_DIR=""
TEST_CONFIG=""
ISCSI_PORT=""
HTTP_PORT=""
DAEMON_PID=""
ISCSI_CONNECTED=0
RW_DEVICE=""
MOUNT_POINT=""
TEST_PREFIX=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"

# Pre-generate the two host-side fixture files used by L3 / L4. /dev/urandom
# is ~25 MiB/s on this box; 1 GiB takes ~40s — wasteful to repeat per row.
# Both files persist across rows under $PERF_BASE_DIR/.
prepare_fixtures() {
    if [[ ! -f "$FIXTURE_RANDOM" ]]; then
        log_info "generating fixture: $FIXTURE_RANDOM (${TOTAL_MB} MiB urandom)"
        dd if=/dev/urandom of="$FIXTURE_RANDOM" bs=1M count="$TOTAL_MB" \
            iflag=fullblock status=none
    fi
    if [[ ! -f "$FIXTURE_COMPRESSIBLE" ]]; then
        log_info "generating fixture: $FIXTURE_COMPRESSIBLE (50% zeros + 50% ABCDE pattern, ${TOTAL_MB} MiB)"
        local half_mb=$(( TOTAL_MB / 2 ))
        dd if=/dev/zero of="$FIXTURE_COMPRESSIBLE" bs=1M count="$half_mb" status=none
        yes 'ABCDEFGHIJKLMNOP' | head -c $(( half_mb * 1024 * 1024 )) \
            >> "$FIXTURE_COMPRESSIBLE"
    fi
}

# Per-row daemon scaffold — copied from test-pipeline-layers.sh:152 (the
# `row_dir_setup` / `row_dir_cleanup` / `start_daemon` triad). Each L3 /
# L4 run brings a fresh daemon so per-fixture chunk pool state doesn't
# contaminate the next measurement.
row_dir_setup() {
    local row_id="$1" fixture="$2"
    TEST_DIR="$PERF_BASE_DIR/row${row_id}-${fixture}"
    TEST_CONFIG="$TEST_DIR/config.yaml"
    MOUNT_POINT="$TEST_DIR/mnt"
    mkdir -p "$TEST_DIR/data" "$MOUNT_POINT"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local run_id; run_id="row${row_id}-${fixture}-$(date +%Y%m%d-%H%M%S)-$$"
    TEST_PREFIX="${ORIG_PREFIX}perf-layers/${run_id}/"
    ISCSI_PORT=""; HTTP_PORT=""
    assign_ports
}

row_dir_cleanup() {
    if [[ -n "$MOUNT_POINT" ]] && mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
    if [[ $ISCSI_CONNECTED -eq 1 && -n "$ISCSI_PORT" ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
        ISCSI_CONNECTED=0
    fi
    if [[ $KEEP_CLOUD -eq 0 && "$BACKEND_TYPE" != "local" && -n "$TEST_PREFIX" ]]; then
        cloud_purge_test_prefix
    fi
    if [[ $KEEP_DATA -eq 0 && -n "$TEST_DIR" && -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR"
    fi
    TEST_DIR=""; TEST_CONFIG=""; MOUNT_POINT=""
    ISCSI_PORT=""; HTTP_PORT=""; DAEMON_PID=""; ISCSI_CONNECTED=0
    RW_DEVICE=""; TEST_PREFIX=""
}

# Seed the daemon's cloud-backends.json from our perf-backends.json,
# folding in the per-run prefix so concurrent runs don't collide on
# remote backends. For local-test the prefix is a no-op (LocalBackend
# ignores it).
make_config() {
    cat > "$TEST_CONFIG" <<EOFCFG
data_dir: "$TEST_DIR/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"
disk_cache:
  disk_free_min_gb: 0
storage:
  compression:
    algorithm: none
  backends:
    $PERF_BACKEND_NAME: $(jq -c --arg name "$PERF_BACKEND_NAME" --arg prefix "$TEST_PREFIX" '.backends[$name] + { prefix: $prefix }' "$PERF_BACKENDS_FILE")
EOFCFG
}

start_daemon() {
    log_info "starting daemon at HTTP $HTTP_PORT / iSCSI $ISCSI_PORT"
    "$DAEMON_PATH" --config "$TEST_CONFIG" >"$TEST_DIR/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local tries=0
    while (( tries < 30 )); do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            return 0
        fi
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            log_error "daemon died at boot — see $TEST_DIR/daemon.log"
            tail -20 "$TEST_DIR/daemon.log" | sed 's/^/  /'
            return 1
        fi
        sleep 1
        tries=$((tries + 1))
    done
    log_error "daemon failed to become ready in 30 s"
    return 1
}

create_volume() {
    local name="$1"
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$name" \
        --size $(( TOTAL_MB * 4 ))M --backend "$PERF_BACKEND_NAME" --dedup local >/dev/null \
        || { log_error "volume create $name failed"; return 1; }
}

connect_iscsi_disk() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 \
        || { log_error "iscsi discovery failed"; return 1; }
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1 \
        || { log_error "iscsi login failed"; return 1; }
    ISCSI_CONNECTED=1
    sleep 3
    local row; row=$(lsscsi -g | awk '/THUR VSA/{print; exit}')
    [[ -n "$row" ]] || { log_error "no THUR VSA device found"; lsscsi -g; return 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    log_info "VSA LUN -> $RW_DEVICE"
}

# Wait for at least one chunk under the configured backend's
# perf-layers prefix. For local-test we instead poll the on-disk pool
# directory since LocalBackend writes synchronously and no list helper
# is needed.
wait_for_chunks() {
    local timeout=600
    local elapsed=0
    if [[ "$BACKEND_TYPE" == "local" ]]; then
        local pool="$TEST_DIR/data/chunks"
        while (( elapsed < timeout )); do
            if [[ -d "$pool" ]] && find "$pool" -type f -name '*.chunk' 2>/dev/null | grep -q .; then
                return 0
            fi
            sleep 2
            elapsed=$((elapsed + 2))
        done
        return 1
    fi
    while (( elapsed < timeout )); do
        local count; count=$(cloud_list "" | grep -cv 'manifests/' || true)
        if (( count > 0 )); then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# L3: raw block write with O_DIRECT. bs=1M is the largest size the
# iSCSI initiator + scsi layer will hand the daemon as a single READ16
# / WRITE16 — matches the iSCSI ceiling without filesystem indirection.
write_raw() {
    local src="$1"
    dd if="$src" of="$RW_DEVICE" bs=1M count="$TOTAL_MB" \
        oflag=direct status=none conv=fsync \
        || { log_error "dd raw to $RW_DEVICE failed"; return 1; }
}

# L4: mkfs.ext4 + mount + cp + sync + umount. fsync via `sync` after
# the write so the cost of journal flush + ext4 commit lands in the
# host-side wall clock, not lazily across the cloud-wait phase.
write_ext4() {
    local src="$1"
    mkfs.ext4 -F -q "$RW_DEVICE" >/dev/null 2>&1 \
        || { log_error "mkfs.ext4 failed"; return 1; }
    mount "$RW_DEVICE" "$MOUNT_POINT" \
        || { log_error "mount failed"; return 1; }
    cp "$src" "$MOUNT_POINT/data.bin" \
        || { log_error "cp to ext4 failed"; umount "$MOUNT_POINT" 2>/dev/null; return 1; }
    sync
    umount "$MOUNT_POINT" \
        || { log_error "umount failed"; return 1; }
}

# --------------------------------------------------------------------
# Rows. Each row implements its own (random, compressible) loop because
# L1 / L2 take fixture-as-arg-to-binary while L3 / L4 swap host-side
# fixture files. Keeping the loop inside each row avoids a fragile
# row-fn-of-row-fn indirection.
# --------------------------------------------------------------------

run_l1() {
    for fixture in random compressible; do
        log_test "L1 in-process ($fixture)"
        local tmp; tmp=$(mktemp -d "$PERF_BASE_DIR/l1-${fixture}-XXXX")
        local t0 t1 t2
        t0=$(date +%s%N)
        "$PERF_VOLUME_WRITE" "$TOTAL_MB" 256 "$fixture" \
            > "$tmp/run.log" 2>&1 || {
            log_error "L1 $fixture failed (see $tmp/run.log)"
            tail -10 "$tmp/run.log" | sed 's/^/  /'
            [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
            continue
        }
        t1=$(date +%s%N); t2=$t1
        perf_summary 1 in-process "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
    done
}

run_l2() {
    for fixture in random compressible; do
        log_test "L2 in-process+cloud ($fixture, backend=$PERF_BACKEND_NAME)"
        local tmp; tmp=$(mktemp -d "$PERF_BASE_DIR/l2-${fixture}-XXXX")
        local t0 t1 t2
        t0=$(date +%s%N)
        "$PERF_VOLUME_CLOUD" "$PERF_BACKENDS_FILE" "$PERF_BACKEND_NAME" \
            "$TOTAL_MB" 256 "$fixture" \
            > "$tmp/run.log" 2>&1 || {
            log_error "L2 $fixture failed (see $tmp/run.log)"
            tail -10 "$tmp/run.log" | sed 's/^/  /'
            [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
            continue
        }
        t1=$(date +%s%N); t2=$t1
        perf_summary 2 in-process+cloud "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
    done
}

run_l3() {
    for fixture in random compressible; do
        log_test "L3 iscsi-raw ($fixture, backend=$PERF_BACKEND_NAME)"
        local src
        if [[ "$fixture" == "random" ]]; then src="$FIXTURE_RANDOM"; else src="$FIXTURE_COMPRESSIBLE"; fi
        row_dir_setup 3 "$fixture"
        make_config
        if ! start_daemon; then row_dir_cleanup; continue; fi
        if ! create_volume "v-l3"; then row_dir_cleanup; continue; fi
        if ! connect_iscsi_disk; then row_dir_cleanup; continue; fi
        local t0 t1 t2
        t0=$(date +%s%N)
        write_raw "$src" || { row_dir_cleanup; continue; }
        t1=$(date +%s%N)
        wait_for_chunks || { log_warn "L3 $fixture: no chunks landed in 600s"; }
        t2=$(date +%s%N)
        perf_summary 3 iscsi-raw "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        row_dir_cleanup
    done
}

run_l4() {
    for fixture in random compressible; do
        log_test "L4 iscsi-ext4 ($fixture, backend=$PERF_BACKEND_NAME)"
        local src
        if [[ "$fixture" == "random" ]]; then src="$FIXTURE_RANDOM"; else src="$FIXTURE_COMPRESSIBLE"; fi
        row_dir_setup 4 "$fixture"
        make_config
        if ! start_daemon; then row_dir_cleanup; continue; fi
        if ! create_volume "v-l4"; then row_dir_cleanup; continue; fi
        if ! connect_iscsi_disk; then row_dir_cleanup; continue; fi
        local t0 t1 t2
        t0=$(date +%s%N)
        write_ext4 "$src" || { row_dir_cleanup; continue; }
        t1=$(date +%s%N)
        wait_for_chunks || { log_warn "L4 $fixture: no chunks landed in 600s"; }
        t2=$(date +%s%N)
        perf_summary 4 iscsi-ext4 "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        row_dir_cleanup
    done
}

trap 'row_dir_cleanup; exit 130' INT TERM

# Pre-generate the host-side fixture files only when a row that needs
# them is actually going to run. Saves ~40s on `--only 1` runs that
# never touch /dev/sdX.
needs_host_fixtures() {
    [[ -z "$ONLY_ROW" ]] && return 0
    [[ "$ONLY_ROW" == "3" || "$ONLY_ROW" == "4" ]]
}
if needs_host_fixtures; then
    prepare_fixtures
fi

run_row_if_selected() {
    local id="$1" name="$2" fn="$3"
    if [[ -n "$ONLY_ROW" && "$ONLY_ROW" != "$id" ]]; then
        log_info "skipping row L$id ($name) — --only=$ONLY_ROW"
        return 0
    fi
    $fn
}

run_row_if_selected 1 in-process       run_l1
run_row_if_selected 2 in-process+cloud run_l2
run_row_if_selected 3 iscsi-raw        run_l3
run_row_if_selected 4 iscsi-ext4       run_l4

echo ""
echo "========================================"
echo "VSA perf-layers summary"
echo "========================================"
if (( ${#PERF_LINES[@]} > 0 )); then
    echo "Per-row perf:"
    for line in "${PERF_LINES[@]}"; do echo "  $line"; done
    perf_table_emit
fi

if [[ $KEEP_DATA -eq 0 ]]; then
    rm -rf "$PERF_BASE_DIR"
fi
exit 0
