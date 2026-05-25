#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Pipeline-Layer Throughput Harness
#
# Sibling of vsa/scripts/perf-layers.sh. Three rows × two fixtures = six
# measurements; tape has no filesystem layer so there's no L4 equivalent
# of VSA's ext4 row.
#
#   L1 in-process       : Cartridge::write_data -> local pool. No
#                         iSCSI, no daemon.
#                         -> core/smc/examples/perf_write
#   L2 in-process+storage : same as L1, sealed chunks uploaded inline
#                         against the configured backend.
#                         -> core/smc/examples/perf_cart_cloud
#   L3 iscsi-tar        : daemon + iSCSI + library init + cartridge
#                         create + mtx load + `tar -b 256` (128 KiB
#                         blocks) -> /dev/nstN -> mtx unload.
#
# `tar -b 256` is intentional: the default block factor of 20 (=10 KiB)
# is a notorious VTL perf trap on iSCSI (tens of thousands of tiny
# SCSI WRITE6 commands instead of saturating the link). The matching
# tar block factor 256 means 128 KiB per BHS segment, matching the
# iSCSI initiator's preferred MaxBurstLength.
#
# Fixtures (run once per row each):
#   random        : /dev/urandom-seeded 1 GiB file (defeats dedup +
#                   compression -> raw ceiling)
#   compressible  : 512 MiB zeros + 512 MiB repeating ABCDEFGHIJKLMNOP
#                   (matches test-pipeline-layers.sh and the in-process
#                   examples' compressible mode)
#
# Default backend is `local-test` (auto-created at
# $TEST_DIR/local-storage); override with THURVTL_PERF_BACKEND=<name>
# pointing at an entry in $REPO/private/storage-backends.json (or
# THURVTL_SOURCE_BACKENDS).
#
# Usage (invoke from repo root):
#   ./vtl/scripts/perf-layers.sh [OPTIONS]
#   THURVTL_PERF_BACKEND=aistor-none ./vtl/scripts/perf-layers.sh
#
# Options:
#   --release             Use ./target/release/ binaries (default: debug)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --only ROW            Run a single row (1..3); omit to run all three
#   --total-mb N          Override fixture size (default: 1024)
#   --keep-data           Don't clean up test data dirs
#   --keep-storage          Don't purge storage test prefixes (remote only)
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
            AWS_*|GOOGLE_*|GCS_*|AZURE_*|AISTOR_*|WASABI_*|MINIO_*|THURVTL_*)
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
KEEP_STORAGE=0
SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.json}"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --only) ONLY_ROW="$2"; shift 2 ;;
        --total-mb) TOTAL_MB="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-storage) KEEP_STORAGE=1; shift ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

[[ -z "$DAEMON_PATH" ]] && DAEMON_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvtld"
[[ -z "$CLI_PATH" ]] && CLI_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvtl"
PERF_WRITE="${REPO_DIR}/target/${BUILD_PROFILE}/examples/perf_write"
PERF_CART_CLOUD="${REPO_DIR}/target/${BUILD_PROFILE}/examples/perf_cart_cloud"

if [[ ! -x "$DAEMON_PATH" || ! -x "$CLI_PATH" ]]; then
    log_error "Missing daemon ($DAEMON_PATH) or cli ($CLI_PATH). Run: cargo build${BUILD_PROFILE:+ --release}"
    exit 1
fi
if [[ ! -x "$PERF_WRITE" || ! -x "$PERF_CART_CLOUD" ]]; then
    log_error "Missing perf examples. Run: cargo build${BUILD_PROFILE:+ --release} -p core-mediachanger --examples"
    exit 1
fi

PERF_BACKEND_NAME="${THURVTL_PERF_BACKEND:-local-test}"

PERF_BASE_DIR="/tmp/test-vtl-perf-layers-$$"
mkdir -p "$PERF_BASE_DIR"
if [[ -n "$SUDO_USER" ]]; then
    chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$PERF_BASE_DIR"
fi
LOCAL_STORAGE_DIR="$PERF_BASE_DIR/local-storage"
mkdir -p "$LOCAL_STORAGE_DIR"
PERF_BACKENDS_FILE="$PERF_BASE_DIR/perf-backends.json"
FIXTURE_RANDOM="$PERF_BASE_DIR/perf-random-${TOTAL_MB}m.bin"
FIXTURE_COMPRESSIBLE="$PERF_BASE_DIR/perf-compressible-${TOTAL_MB}m.bin"
FIXTURE_BYTES=$(( TOTAL_MB * 1024 * 1024 ))

# Backend resolution mirrors vsa/scripts/perf-layers.sh; same default
# (local-test) and same THURVTL_PERF_BACKEND override semantics.
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
    "local-test": { "type": "local", "root_dir": "$LOCAL_STORAGE_DIR" }
  }
}
EOFJ
    log_info "VTL perf-layers backend = local-test (self-managed at $LOCAL_STORAGE_DIR)"
else
    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "THURVTL_PERF_BACKEND=$PERF_BACKEND_NAME but SOURCE_BACKENDS=$SOURCE_BACKENDS unreadable"
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
    verify_storage_creds || exit 1
    log_info "VTL perf-layers backend = $PERF_BACKEND_NAME (type=$BACKEND_TYPE bucket=${BACKEND_BUCKET}${BACKEND_CONTAINER})"
fi

PERF_LINES=()

TEST_DIR=""
TEST_CONFIG=""
ISCSI_PORT=""
HTTP_PORT=""
DAEMON_PID=""
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
NOREWIND_DEVICE=""
TAPE_SG_DEVICE=""
TEST_PREFIX=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"

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

row_dir_setup() {
    local row_id="$1" fixture="$2"
    TEST_DIR="$PERF_BASE_DIR/row${row_id}-${fixture}"
    TEST_CONFIG="$TEST_DIR/config.yaml"
    mkdir -p "$TEST_DIR/data"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local run_id; run_id="row${row_id}-${fixture}-$(date +%Y%m%d-%H%M%S)-$$"
    TEST_PREFIX="${ORIG_PREFIX}perf-layers/${run_id}/"
    ISCSI_PORT=""; HTTP_PORT=""
    assign_ports
}

row_dir_cleanup() {
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
    if [[ $KEEP_STORAGE -eq 0 && "$BACKEND_TYPE" != "local" && -n "$TEST_PREFIX" ]]; then
        storage_purge_test_prefix
    fi
    if [[ $KEEP_DATA -eq 0 && -n "$TEST_DIR" && -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR"
    fi
    TEST_DIR=""; TEST_CONFIG=""
    ISCSI_PORT=""; HTTP_PORT=""; DAEMON_PID=""; ISCSI_CONNECTED=0
    CHANGER_DEVICE=""; NOREWIND_DEVICE=""; TAPE_SG_DEVICE=""; TEST_PREFIX=""
}

make_config() {
    cat > "$TEST_CONFIG" <<EOFCFG
data_dir: "$TEST_DIR/data"
library:
  num_slots: 4
  num_drives: 1
  lto_generation: 8
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

create_cartridge() {
    local bc="$1"
    "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$bc" \
        --lto-generation 8 --backend "$PERF_BACKEND_NAME" \
        --chunking fastcdc --chunk-size-mb 8 --dedup local >/dev/null \
        || { log_error "cartridge create $bc failed"; return 1; }
}

connect_iscsi() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 \
        || { log_error "iscsi discovery failed"; return 1; }
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1 \
        || { log_error "iscsi login failed"; return 1; }
    ISCSI_CONNECTED=1
    sleep 3
    CHANGER_DEVICE=$(lsscsi -g 2>/dev/null | awk '/mediumx/{print $NF; exit}')
    local tape_dev; tape_dev=$(lsscsi -g 2>/dev/null | awk '/tape/{print $7; exit}')
    TAPE_SG_DEVICE=$(lsscsi -g 2>/dev/null | awk '/tape/{print $NF; exit}')
    if [[ -z "$CHANGER_DEVICE" || -z "$tape_dev" ]]; then
        log_error "could not locate changer/tape devices"; lsscsi -g; return 1
    fi
    NOREWIND_DEVICE=$(echo "$tape_dev" | sed 's|/dev/st|/dev/nst|')
    log_info "changer=$CHANGER_DEVICE no-rewind=$NOREWIND_DEVICE sg=$TAPE_SG_DEVICE"
    mt -f "$NOREWIND_DEVICE" status >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
}

# Wait for at least one chunk to land. Local backend polls the on-disk
# pool; remote backends list the prefix.
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
        local count; count=$(storage_list "" | grep -cv 'manifests/' || true)
        if (( count > 0 )); then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# L3 tape write: load slot 1 -> drive 0, tar -b 256 the fixture file
# directly to the no-rewind device, weof + rewind + unload. tar block
# factor 256 (= 128 KiB) matches the iSCSI initiator's preferred SCSI
# transfer size; the default 20 (10 KiB) caps throughput at ~50 MiB/s
# on this host independent of any downstream cost. Fixture is hardlinked
# into a per-row stage dir so tar reads a directory tree without copying
# the 1 GiB payload.
tar_to_tape() {
    local src="$1"
    local stage="$TEST_DIR/fixture-stage"
    mkdir -p "$stage"
    ln -f "$src" "$stage/data.bin" 2>/dev/null \
        || cp "$src" "$stage/data.bin" \
        || { log_error "stage fixture into $stage failed"; return 1; }
    mtx -f "$CHANGER_DEVICE" load 1 0 2>"$TEST_DIR/mtx-load.err" || {
        log_error "mtx load 1 0 failed"
        cat "$TEST_DIR/mtx-load.err" | sed 's/^/  /'
        return 1
    }
    sleep 1
    mt -f "$NOREWIND_DEVICE" rewind 2>"$TEST_DIR/mt.err" || {
        log_error "mt rewind failed"; cat "$TEST_DIR/mt.err" | sed 's/^/  /'; return 1
    }
    if ! tar -b 256 -C "$stage" -cf "$NOREWIND_DEVICE" . 2>"$TEST_DIR/tar.err"; then
        log_error "tar to tape failed:"
        cat "$TEST_DIR/tar.err" | sed 's/^/  /'
        return 1
    fi
    mt -f "$NOREWIND_DEVICE" weof   >/dev/null 2>&1 || true
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE" unload 1 0 >"$TEST_DIR/mtx-unload.err" 2>&1 || {
        log_error "mtx unload failed:"; cat "$TEST_DIR/mtx-unload.err" | sed 's/^/  /'; return 1
    }
}

# --------------------------------------------------------------------
# Rows
# --------------------------------------------------------------------

run_l1() {
    for fixture in random compressible; do
        log_test "L1 in-process ($fixture)"
        local tmp; tmp=$(mktemp -d "$PERF_BASE_DIR/l1-${fixture}-XXXX")
        local t0 t1 t2
        t0=$(date +%s%N)
        "$PERF_WRITE" fastcdc "$TOTAL_MB" 256 "$fixture" \
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
        log_test "L2 in-process+storage ($fixture, backend=$PERF_BACKEND_NAME)"
        local tmp; tmp=$(mktemp -d "$PERF_BASE_DIR/l2-${fixture}-XXXX")
        local t0 t1 t2
        t0=$(date +%s%N)
        "$PERF_CART_CLOUD" "$PERF_BACKENDS_FILE" "$PERF_BACKEND_NAME" \
            fastcdc "$TOTAL_MB" 256 "$fixture" \
            > "$tmp/run.log" 2>&1 || {
            log_error "L2 $fixture failed (see $tmp/run.log)"
            tail -10 "$tmp/run.log" | sed 's/^/  /'
            [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
            continue
        }
        t1=$(date +%s%N); t2=$t1
        perf_summary 2 in-process+storage "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        [[ $KEEP_DATA -eq 0 ]] && rm -rf "$tmp"
    done
}

run_l3() {
    for fixture in random compressible; do
        log_test "L3 iscsi-tar ($fixture, backend=$PERF_BACKEND_NAME)"
        local src
        if [[ "$fixture" == "random" ]]; then src="$FIXTURE_RANDOM"; else src="$FIXTURE_COMPRESSIBLE"; fi
        row_dir_setup 3 "$fixture"
        make_config
        if ! start_daemon; then row_dir_cleanup; continue; fi
        if ! create_cartridge "TAPE01L8"; then row_dir_cleanup; continue; fi
        if ! connect_iscsi; then row_dir_cleanup; continue; fi
        local t0 t1 t2
        t0=$(date +%s%N)
        tar_to_tape "$src" || { row_dir_cleanup; continue; }
        t1=$(date +%s%N)
        wait_for_chunks || { log_warn "L3 $fixture: no chunks landed in 600s"; }
        t2=$(date +%s%N)
        perf_summary 3 iscsi-tar "$fixture" "$FIXTURE_BYTES" "$t0" "$t1" "$t2"
        row_dir_cleanup
    done
}

trap 'row_dir_cleanup; exit 130' INT TERM

needs_host_fixtures() {
    [[ -z "$ONLY_ROW" ]] && return 0
    [[ "$ONLY_ROW" == "3" ]]
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
run_row_if_selected 2 in-process+storage run_l2
run_row_if_selected 3 iscsi-tar        run_l3

echo ""
echo "========================================"
echo "VTL perf-layers summary"
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
