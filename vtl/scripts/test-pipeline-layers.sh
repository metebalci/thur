#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Pipeline-Layer Matrix Test
#
# Exercises each pipeline layer in isolation against a single storage
# backend. Five runs:
#
#   1. baseline   : dedup local,  DCE off, encrypt off, storage zstd off
#   2. + dedup    : dedup global, DCE off, encrypt off, storage zstd off
#                   -> assert: second cartridge dedups against first
#   3. + DCE      : dedup local,  DCE on,  encrypt off, storage zstd off
#                   -> assert: chunk bytes on backend < 90% of input size
#                              (drive-compressed before chunking)
#   4. + encrypt  : dedup local,  DCE off, encrypt on,  storage zstd off
#                   -> assert: chunk bytes on backend are ciphertext
#                              (do NOT equal the plaintext fixture)
#   5. + storage zstd: dedup local, DCE off, encrypt off, storage zstd on
#                   -> assert: chunk bytes on backend < 80% of input size
#                              (post-dedup zstd at the storage layer)
#
# Defaults to aistor-none for fast LAN iteration (override via
# THURVTL_TEST_BACKEND). Refuses any backend with retention_mode != none
# because the matrix re-runs need to purge prefixes cleanly.
#
# Bugs found+fixed while bringing this up (2026-05-13), useful as
# documentation for future similar scripts:
#
#   1. `mtx unload` argument order is `unload [SLOT] DRIVE`. An
#      earlier draft had it flipped, so the daemon never saw a
#      MOVE MEDIUM (unload) and the chunk-seal-on-unload trigger
#      never fired. Cartridges stayed in the drive forever, no
#      chunks landed in storage.
#   2. Unit Attention. MOVE MEDIUM (load) queues a 0x28/0x00 UA
#      on the drive. The first SCSI command after load returns
#      the UA and clears it; later commands work normally. The
#      pre-tar SCSI hooks (`MODE SELECT page 0x0F` for DCE,
#      `SECURITY PROTOCOL OUT page 0x10` for encryption) need a
#      `sg_turs` drain on `/dev/sgN` *before* they fire — `mt
#      status` drains UA on `/dev/nstN` only, which is a separate
#      I_T_L nexus from sg_raw's path.
#   3. Bash variables truncate at the first null byte. The MODE
#      SELECT and SPOUT payloads begin with a 0x00, so staging
#      the binary in `payload_bin=$(... | xxd -r -p)` silently
#      produced an empty string. The helpers in
#      `scripts/lib/test-helpers.sh` use process substitution to
#      pipe the binary into sg_raw's `-i` fd directly.
#
# NOTE on credentials: same convention as test-backup-storage.sh — drop
# AWS_* / GOOGLE_* / AZURE_* / AISTOR_* into $REPO/private/thur.env
# and the backend entry into $REPO/private/storage-backends.json. The
# script auto-sources thur.env and defaults THURVTL_SOURCE_BACKENDS
# accordingly.
#
# NOTE on sudo: the script self-elevates via `sudo KEY=VAL ... $0` and
# forwards backend-credential env vars; sudo-rs on Ubuntu 26.04+ ignores `-E`
# so explicit forwarding is mandatory. See test-backup-storage.sh header
# for the long form.
#
# Usage (invoke from repo root):
#   THURVTL_TEST_BACKEND=aistor-none ./vtl/scripts/test-pipeline-layers.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default: debug)
#   --daemon-path PATH    Path to thurvtld binary
#   --cli-path PATH       Path to thurvtl binary
#   --only ROW            Run a single row (1..5); omit to run all 5
#   --keep-data           Don't clean up test data dirs (debug aid)
#   --keep-storage          Don't purge storage test prefixes (debug aid)
#

SCRIPT_DIR_RAW="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR_RAW}/../.." && pwd)"

# Auto-load maintainer-private storage credentials if the file exists.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding the backend-relevant env vars.
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

ONLY_ROW=""
KEEP_STORAGE=0
SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --only) ONLY_ROW="$2"; shift 2 ;;
        --keep-storage) KEEP_STORAGE=1; shift ;;
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

[[ -z "${THURVTL_TEST_BACKEND:-}" ]] && THURVTL_TEST_BACKEND="aistor-none"

require_daemon_binaries thurvtl
if [[ ! -r "$SOURCE_BACKENDS" ]]; then
    log_error "Cannot read backends file: $SOURCE_BACKENDS"
    exit 1
fi

# Parse the chosen backend's coordinates out of storage-backends.yaml
# (same shape test-backup-storage.sh uses, so cred forwarding + probes
# work identically). yq required at the same version contract.
BACKEND_TYPE=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
BACKEND_BUCKET=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
BACKEND_REGION=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
BACKEND_AUTH_AKID_ENV=$(yq -r "
    .storage.backends.\"$THURVTL_TEST_BACKEND\".auth
    | select(.type == \"env\") | .access_key_id_env // \"\"
" "$SOURCE_BACKENDS")
BACKEND_AUTH_SECRET_ENV=$(yq -r "
    .storage.backends.\"$THURVTL_TEST_BACKEND\".auth
    | select(.type == \"env\") | .secret_access_key_env // \"\"
" "$SOURCE_BACKENDS")
RETENTION=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")
ORIG_PREFIX=$(yq -r ".storage.backends.\"$THURVTL_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")

if [[ "$BACKEND_TYPE" == "local" ]]; then
    log_error "matrix needs a real storage backend; '$THURVTL_TEST_BACKEND' is type=local"
    exit 1
fi
if [[ "$RETENTION" != "none" ]]; then
    log_error "backend '$THURVTL_TEST_BACKEND' has retention_mode=$RETENTION; matrix refuses (purge would fail)"
    exit 1
fi
log_info "Pipeline-layer matrix vs '$THURVTL_TEST_BACKEND' (type=$BACKEND_TYPE bucket=${BACKEND_BUCKET}${BACKEND_CONTAINER})"

# Track passed / failed rows for the final summary. We use return
# codes per-row, not exit, so a single failure doesn't skip later
# rows (better signal: "encrypt regressed, dedup still works").
PASS_ROWS=()
FAIL_ROWS=()
PERF_LINES=()

# Per-row state (set in run_row, used by helpers + cleanup).
TEST_DIR=""
TEST_CONFIG=""
ISCSI_PORT=""
HTTP_PORT=""
DAEMON_PID=""
ISCSI_CONNECTED=0
CHANGER_DEVICE=""
TAPE_DEVICE=""
NOREWIND_DEVICE=""
TAPE_SG_DEVICE=""
TEST_PREFIX=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"

# ---------------------------------------------------------------------------
# Per-row infrastructure
# ---------------------------------------------------------------------------

row_dir_setup() {
    local row_id="$1"
    TEST_DIR="/tmp/test-pipeline-layers-$$-$row_id"
    TEST_CONFIG="$TEST_DIR/config.yaml"
    mkdir -p "$TEST_DIR/data"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local run_id; run_id="row${row_id}-$(date +%Y%m%d-%H%M%S)-$$"
    TEST_PREFIX="${ORIG_PREFIX}test-matrix/${run_id}/"
    assign_ports
}

row_dir_cleanup() {
    stop_thur_daemon
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
    fi
    if [[ $KEEP_STORAGE -eq 0 ]]; then
        storage_purge_test_prefix
    else
        log_info "row cleanup: keeping storage prefix $TEST_PREFIX"
    fi
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "row cleanup: keeping $TEST_DIR"
    fi
    TEST_DIR=""; TEST_CONFIG=""; ISCSI_PORT=""; HTTP_PORT=""
    DAEMON_PID=""; ISCSI_CONNECTED=0; CHANGER_DEVICE=""; TAPE_DEVICE=""
    TAPE_SG_DEVICE=""; TEST_PREFIX=""
}

# Generate a daemon config for this row. Caller passes
# storage_compression_algorithm as "none" | "zstd" (we only flip these
# two for the matrix).
make_config() {
    local storage_compress="$1"
    local backend_json
    backend_json=$(jq -c \
        ".backends.\"$THURVTL_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS")
    cat > "$TEST_CONFIG" <<EOFCFG
data_dir: "$TEST_DIR/data"
$(yaml_vtl_library 4 1 8)
http:
  listen: "127.0.0.1:$HTTP_PORT"
$(yaml_iscsi "$TARGET_IQN")
disk_cache:
  disk_free_min_gb: 0
storage:
  compression:
    algorithm: $storage_compress
  backends:
    testbackend: $backend_json
EOFCFG
}

start_daemon() {
    log_info "starting daemon (PID will be tracked) at HTTP $HTTP_PORT / iSCSI $ISCSI_PORT"
    "$DAEMON_PATH" --config "$TEST_CONFIG" >"$TEST_DIR/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local tries=0
    while (( tries < 30 )); do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            return 0
        fi
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            log_error "daemon died at boot — see $TEST_DIR/daemon.log"
            tail -30 "$TEST_DIR/daemon.log" | sed 's/^/  /'
            return 1
        fi
        sleep 1
        tries=$((tries + 1))
    done
    log_error "daemon failed to become ready in 30 s"
    return 1
}

init_library() {
    "$CLI_PATH" --config "$TEST_CONFIG" \
        --user "${SUDO_USER:-root}" \
        library init --slots 4 --drives 1 --lto-generation 8 >/dev/null \
        || { log_error "library init failed"; return 1; }
}

# Create N cartridges with the given dedup-scope. Same chunking +
# chunk-size as test-backup-storage.sh's defaults so the seal pipeline
# behaves identically — fastcdc avoids the fixed-block alignment
# trap that prevented small writes from sealing in earlier iterations.
create_cartridges() {
    local count="$1" dedup="$2"
    for i in $(seq 1 "$count"); do
        local bc; bc=$(printf "TAPE%02dL8" "$i")
        "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$bc" \
            --lto-generation 8 --backend testbackend \
            --chunking fastcdc --chunk-size-mb 8 --dedup "$dedup" >/dev/null \
            || { log_error "cartridge create $bc failed"; return 1; }
    done
}

connect_iscsi() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 \
        || { log_error "iscsi discovery failed"; return 1; }
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1 \
        || { log_error "iscsi login failed"; return 1; }
    ISCSI_CONNECTED=1
    sleep 3
    CHANGER_DEVICE=$(lsscsi -g 2>/dev/null | awk '/mediumx/{print $NF; exit}')
    TAPE_DEVICE=$(lsscsi -g 2>/dev/null | awk '/tape/{print $7; exit}')
    TAPE_SG_DEVICE=$(lsscsi -g 2>/dev/null | awk '/tape/{print $NF; exit}')
    if [[ -z "$CHANGER_DEVICE" || -z "$TAPE_DEVICE" ]]; then
        log_error "could not locate changer/tape devices"
        lsscsi -g
        return 1
    fi
    # Use the no-rewind device for actual writes — same convention as
    # test-backup-storage.sh; rewinding between tar segments is the
    # caller's job, not the kernel's.
    NOREWIND_DEVICE=$(echo "$TAPE_DEVICE" | sed 's|/dev/st|/dev/nst|')
    log_info "changer=$CHANGER_DEVICE tape=$TAPE_DEVICE no-rewind=$NOREWIND_DEVICE sg=$TAPE_SG_DEVICE"
    # Warm up the SCSI path; quiets the first-cmd timing flake.
    mt -f "$NOREWIND_DEVICE" status >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
}

# Build the per-row fixture under /tmp as a directory tree (tar
# straight from disk to tape). 1 GiB total — sized for the perf
# matrix; bigger than the 8 MiB correctness fixture so wall-clock
# throughput numbers aren't dominated by setup overhead.
# Mostly-compressible payload so DCE / storage zstd assertions show
# visible size deltas.
make_fixture() {
    local stage="$TEST_DIR/fixture-stage"
    mkdir -p "$stage"
    yes 'A' | head -c $((512 * 1048576)) > "$stage/a.dat"
    yes 'B' | head -c $((512 * 1048576)) > "$stage/b.dat"
    FIXTURE_BYTES=$(( 1024 * 1048576 ))
    log_info "fixture stage at $stage ($FIXTURE_BYTES B raw, highly compressible)"
}

# Load slot $1 -> drive 0, tar the fixture stage to /dev/nstN, weof,
# unload. Same shape as test-backup-storage.sh's tape write.
tar_to_tape() {
    local slot="$1"
    mtx -f "$CHANGER_DEVICE" load "$slot" 0 2>"$TEST_DIR/mtx-load.err" || {
        log_error "mtx load $slot 0 failed"
        cat "$TEST_DIR/mtx-load.err" | sed 's/^/  /'
        return 1
    }
    sleep 1
    mt -f "$NOREWIND_DEVICE" rewind 2>"$TEST_DIR/mt.err" || {
        log_error "mt rewind failed"; cat "$TEST_DIR/mt.err" | sed 's/^/  /'; return 1
    }
    if ! tar -C "$TEST_DIR/fixture-stage" -cf "$NOREWIND_DEVICE" . 2>"$TEST_DIR/tar.err"; then
        log_error "tar to tape failed:"
        cat "$TEST_DIR/tar.err" | sed 's/^/  /'
        return 1
    fi
    # Sealing the in-progress chunk needs an explicit signal. `mt
    # weof` writes a FILEMARK which the daemon's chunk-seal state
    # machine treats as a flush boundary; then `mt rewind` closes the
    # device handle so unload can fire cleanly without contending
    # with the kernel st driver's lingering open. Same sequence the
    # existing test-backup-storage.sh uses.
    mt -f "$NOREWIND_DEVICE" weof   >/dev/null 2>&1 || true
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    # mtx args: `unload [SLOTNUM] DRIVENUM` — unload cartridge in
    # DRIVENUM back to SLOTNUM. Previously had these flipped which
    # silently failed (no drive at the SLOTNUM position), the daemon
    # never saw MOVE MEDIUM, and the chunk-seal-on-unload trigger
    # never fired.
    mtx -f "$CHANGER_DEVICE" unload "$slot" 0 >"$TEST_DIR/mtx-unload.err" 2>&1 || {
        log_error "mtx unload failed:"; cat "$TEST_DIR/mtx-unload.err" | sed 's/^/  /'; return 1
    }
}

# Sum the bytes of every object under our storage test prefix (chunks +
# manifest backups together). Used to compare against $FIXTURE_BYTES.
total_storage_bytes() {
    local subpath="${1:-}"
    local full="${TEST_PREFIX}${subpath}"
    case "$BACKEND_TYPE" in
        s3)
            local args=()
            [[ -n "$BACKEND_ENDPOINT" ]] && args+=(--endpoint-url "$BACKEND_ENDPOINT")
            [[ -n "$BACKEND_REGION" ]] && args+=(--region "$BACKEND_REGION")
            local aws_overrides=()
            if [[ -n "$BACKEND_AUTH_AKID_ENV" && -n "$BACKEND_AUTH_SECRET_ENV" ]]; then
                aws_overrides=(
                    "AWS_ACCESS_KEY_ID=${!BACKEND_AUTH_AKID_ENV}"
                    "AWS_SECRET_ACCESS_KEY=${!BACKEND_AUTH_SECRET_ENV}"
                )
            fi
            env "${aws_overrides[@]}" aws "${args[@]}" s3 ls "s3://${BACKEND_BUCKET}/${full}" --recursive 2>/dev/null \
                | awk '{ sum += $3 } END { print sum+0 }'
            ;;
        gcs)
            gcloud storage objects list "gs://${BACKEND_BUCKET}/${full}**" --format='value(size)' 2>/dev/null \
                | awk '{ sum += $1 } END { print sum+0 }'
            ;;
        azure)
            az storage blob list \
                --account-name "$BACKEND_ACCOUNT" \
                --container-name "$BACKEND_CONTAINER" \
                --prefix "$full" \
                --auth-mode login \
                --query "[].properties.contentLength" -o tsv 2>/dev/null \
                | awk '{ sum += $1 } END { print sum+0 }'
            ;;
    esac
}

# Pull a single chunk object's bytes into a local file. Used by the
# encrypt row to assert the on-storage bytes don't equal plaintext.
download_first_chunk() {
    local out="$1"
    local key; key=$(storage_list "chunks/" | head -1)
    [[ -z "$key" ]] && return 1
    case "$BACKEND_TYPE" in
        s3)
            local args=()
            [[ -n "$BACKEND_ENDPOINT" ]] && args+=(--endpoint-url "$BACKEND_ENDPOINT")
            [[ -n "$BACKEND_REGION" ]] && args+=(--region "$BACKEND_REGION")
            local aws_overrides=()
            if [[ -n "$BACKEND_AUTH_AKID_ENV" && -n "$BACKEND_AUTH_SECRET_ENV" ]]; then
                aws_overrides=(
                    "AWS_ACCESS_KEY_ID=${!BACKEND_AUTH_AKID_ENV}"
                    "AWS_SECRET_ACCESS_KEY=${!BACKEND_AUTH_SECRET_ENV}"
                )
            fi
            env "${aws_overrides[@]}" aws "${args[@]}" s3 cp "s3://${BACKEND_BUCKET}/${key}" "$out" >/dev/null 2>&1
            ;;
        gcs)
            gcloud storage cp "gs://${BACKEND_BUCKET}/${key}" "$out" >/dev/null 2>&1
            ;;
        azure)
            az storage blob download \
                --account-name "$BACKEND_ACCOUNT" \
                --container-name "$BACKEND_CONTAINER" \
                --name "$key" \
                --file "$out" \
                --auth-mode login >/dev/null 2>&1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Rows
# ---------------------------------------------------------------------------

# Common run scaffolding: setup row dir, init library (daemon-down),
# start daemon, login iSCSI, make fixture. `library init` MUST run
# before the daemon starts because the daemon refuses to boot
# without a library manifest.
row_bring_up() {
    local storage_compress="$1"
    make_config "$storage_compress" || return 1
    start_daemon || return 1
    connect_iscsi || return 1
    make_fixture || return 1
}

# Row 1: baseline. Write a fixture, verify chunks land in storage, no
# special property to assert beyond "data path works". The full
# byte-equality + refetch coverage lives in test-backup-storage.sh —
# the matrix is here to flag *regressions per layer*, not duplicate
# the end-to-end suite.
row_baseline() {
    log_test "row 1: baseline (dedup=local, DCE=off, encrypt=off, storage=none)"
    row_dir_setup 1
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_cartridges 1 "local" || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    tar_to_tape 1 || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE01L8/manifest-latest.json" 600 \
        || { log_error "row 1: manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local cb; cb=$(total_storage_bytes)
    log_info "row 1: storage bytes after write = $cb"
    perf_summary 1 baseline mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$cb"
    if (( cb > 0 )); then
        row_dir_cleanup
        return 0
    fi
    log_error "row 1: no bytes in storage after write"
    row_dir_cleanup
    return 1
}

# Row 2: cross-cartridge dedup. Write the same fixture to two carts;
# verify the second one's storage bytes are small enough to indicate
# cross-cart sharing.
row_dedup() {
    log_test "row 2: +dedup (cross-cartridge global dedup)"
    row_dir_setup 2
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_cartridges 2 "global" || { row_dir_cleanup; return 1; }

    local t0 t1 t2
    t0=$(date +%s%N)
    tar_to_tape 1 || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE01L8/manifest-latest.json" 600 \
        || { log_error "row 2: TAPE01L8 manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local bytes_after_one; bytes_after_one=$(total_storage_bytes)
    local snap_one="$TEST_DIR/chunks-after-tape01.txt"
    storage_chunks_snapshot "$snap_one"
    local count_one; count_one=$(wc -l < "$snap_one")
    log_info "row 2: after TAPE01L8 — bytes=$bytes_after_one chunks=$count_one"
    perf_summary 2 dedup-first-write mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$bytes_after_one"

    t0=$(date +%s%N)
    tar_to_tape 2 || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE02L8/manifest-latest.json" 600 \
        || { log_error "row 2: TAPE02L8 manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local bytes_after_two; bytes_after_two=$(total_storage_bytes)
    local snap_two="$TEST_DIR/chunks-after-tape02.txt"
    storage_chunks_snapshot "$snap_two"
    local new_chunks; new_chunks=$(storage_chunks_new_count "$snap_one" "$snap_two")
    log_info "row 2: after TAPE02L8 — bytes=$bytes_after_two new_chunks=$new_chunks"
    local byte_delta=$(( bytes_after_two - bytes_after_one ))
    perf_summary 2 dedup-second-write mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$byte_delta"

    # Count-delta dedup assertion. Tar of identical fixture content
    # to two cartridges with Global dedup should produce zero new
    # chunk objects — every chunk is a content-addressed dedup hit.
    # Allow 2 as ceiling for any FastCDC boundary edge cases or
    # mid-stream re-chunking quirks; in practice this stays at 0.
    local cap=2
    if (( new_chunks <= cap )); then
        log_info "row 2: dedup observed ($new_chunks new chunks under chunks/, cap=$cap)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 2: insufficient dedup ($new_chunks new chunks > cap $cap)"
    row_dir_cleanup
    return 1
}

# Helper: load a slot's cartridge into drive 0, run an optional
# pre-tar SCSI hook (DCE / SPOUT), tar-write the fixture stage,
# unload. Mirrors tar_to_tape but lets the caller inject MODE SELECT
# / SPOUT between load and the tar. Returns 0 on success.
tar_to_tape_with_hook() {
    local slot="$1"
    local pre_tar_hook="$2"
    mtx -f "$CHANGER_DEVICE" load "$slot" 0 2>"$TEST_DIR/mtx-load.err" || {
        log_error "mtx load $slot 0 failed"
        cat "$TEST_DIR/mtx-load.err" | sed 's/^/  /'
        return 1
    }
    sleep 1
    # MOVE MEDIUM leaves a Unit Attention pending on the drive ("not
    # ready to ready change, medium may have changed"). Every SCSI
    # command after load returns UA *once* and clears it. If the
    # pre-tar hook (MODE SELECT page 0x0F for DCE, SPOUT page 0x10
    # for encryption) is the very first command, it hits the UA and
    # the daemon returns CHECK CONDITION without applying the page.
    # `mt status` drains UA via the /dev/nstN path; sg_raw uses the
    # /dev/sgN passthrough which is a different I_T_L nexus from the
    # daemon's perspective, so we drain *that* path too with sg_turs
    # before invoking the hook.
    mt -f "$NOREWIND_DEVICE" status >/dev/null 2>&1 || true
    if [[ -n "$pre_tar_hook" ]]; then
        sg_turs "$TAPE_SG_DEVICE" >/dev/null 2>&1 || true
        # Drain a second time — some firmwares (and our daemon's
        # multi-event UA queue) post multiple UAs on a single load.
        sg_turs "$TAPE_SG_DEVICE" >/dev/null 2>&1 || true
        $pre_tar_hook || { log_error "pre-tar hook failed: $pre_tar_hook"; return 1; }
    fi
    mt -f "$NOREWIND_DEVICE" rewind 2>"$TEST_DIR/mt.err" || {
        log_error "mt rewind failed"; cat "$TEST_DIR/mt.err" | sed 's/^/  /'; return 1
    }
    if ! tar -C "$TEST_DIR/fixture-stage" -cf "$NOREWIND_DEVICE" . 2>"$TEST_DIR/tar.err"; then
        log_error "tar to tape failed:"
        cat "$TEST_DIR/tar.err" | sed 's/^/  /'
        return 1
    fi
    mt -f "$NOREWIND_DEVICE" weof   >/dev/null 2>&1 || true
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    mtx -f "$CHANGER_DEVICE" unload "$slot" 0 >"$TEST_DIR/mtx-unload.err" 2>&1 || {
        log_error "mtx unload failed:"; cat "$TEST_DIR/mtx-unload.err" | sed 's/^/  /'; return 1
    }
}

# Pre-tar hook: enable drive-side compression.
hook_dce_on() {
    scsi_enable_dce "$TAPE_SG_DEVICE" on
}

# Pre-tar hook: set an AES-256 session key via SPOUT.
hook_set_session_key() {
    local key; key=$(openssl rand -hex 32)
    scsi_set_session_key "$TAPE_SG_DEVICE" "$key"
}

# Row 3: DCE on. Pre-tar, issue MODE SELECT page 0x0F. Assert storage
# bytes are smaller than the fixture (compression worked).
row_dce() {
    log_test "row 3: +DCE (drive-side compression via MODE SELECT page 0x0F)"
    row_dir_setup 3
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_cartridges 1 "local" || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    tar_to_tape_with_hook 1 hook_dce_on || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE01L8/manifest-latest.json" 600 \
        || { log_error "row 3: manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local storage_bytes; storage_bytes=$(total_storage_bytes "chunks/")
    log_info "row 3: chunk bytes-on-storage = $storage_bytes (input: $FIXTURE_BYTES)"
    perf_summary 3 dce mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$storage_bytes"
    # Fixture is 8 MiB of repeating chars — DCE should compress to
    # well under 7 MiB. Generous ceiling at 90% of input.
    local ceiling=$(( FIXTURE_BYTES * 9 / 10 ))
    if (( storage_bytes > 0 && storage_bytes < ceiling )); then
        log_info "row 3: drive compression observed ($storage_bytes < $ceiling = 90% of fixture)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 3: chunk bytes not compressed ($storage_bytes >= $ceiling)"
    row_dir_cleanup
    return 1
}

# Row 4: SPOUT encryption. Set a session key, write, verify on-storage
# bytes are NOT plaintext.
row_encrypt() {
    log_test "row 4: +encrypt (SPOUT session key via SECURITY PROTOCOL OUT)"
    row_dir_setup 4
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_cartridges 1 "local" || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    tar_to_tape_with_hook 1 hook_set_session_key || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE01L8/manifest-latest.json" 600 \
        || { log_error "row 4: manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local enc_storage_bytes; enc_storage_bytes=$(total_storage_bytes "chunks/")
    perf_summary 4 encrypt mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$enc_storage_bytes"
    # Pull one chunk from the backend and check its bytes don't match
    # the plaintext fixture. (We use head bytes; AES-GCM ciphertext
    # is high-entropy and won't share prefixes with a run of 'A's.)
    download_first_chunk "$TEST_DIR/sampled-chunk.bin" \
        || { log_error "row 4: no chunks on storage to sample"; row_dir_cleanup; return 1; }
    # 'A' = 0x41. If the chunk starts with that byte we either have
    # plaintext (regression) or a 1-in-256 chance with random bytes.
    # Sample more for confidence: count runs of 'A' in first 256 B —
    # plaintext has many; ciphertext has ~1.
    local plaintext_a_count
    plaintext_a_count=$(head -c 256 "$TEST_DIR/sampled-chunk.bin" | tr -dc 'A' | wc -c)
    log_info "row 4: 'A' chars in first 256 B of sampled chunk = $plaintext_a_count"
    if (( plaintext_a_count < 10 )); then
        log_info "row 4: chunk is ciphertext (low 'A' density)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 4: chunk looks like plaintext (too many 'A' bytes)"
    row_dir_cleanup
    return 1
}

# Row 5: storage zstd. Write a compressible fixture; verify storage
# bytes are smaller than the input.
row_storage_zstd() {
    log_test "row 5: +storage zstd (storage.compression.algorithm=zstd)"
    row_dir_setup 5
    row_bring_up "zstd" || { row_dir_cleanup; return 1; }
    create_cartridges 1 "local" || { row_dir_cleanup; return 1; }

    local t0 t1 t2
    t0=$(date +%s%N)
    tar_to_tape 1 || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    storage_wait_for_key "manifests/TAPE01L8/manifest-latest.json" 600 \
        || { log_error "row 5: manifest never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local storage_bytes; storage_bytes=$(total_storage_bytes "chunks/")
    log_info "row 5: chunk bytes-on-storage = $storage_bytes (input: $FIXTURE_BYTES)"
    perf_summary 5 storage-zstd mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$storage_bytes"
    local ceiling=$(( FIXTURE_BYTES * 8 / 10 ))
    if (( storage_bytes > 0 && storage_bytes < ceiling )); then
        log_info "row 5: storage compression observed ($storage_bytes < $ceiling = 80% of fixture)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 5: storage bytes not compressed ($storage_bytes >= $ceiling)"
    row_dir_cleanup
    return 1
}

# ---------------------------------------------------------------------------
# Top-level harness
# ---------------------------------------------------------------------------

trap 'row_dir_cleanup; exit 130' INT TERM

# `verify_storage_creds` from test-helpers.sh — uses BACKEND_* globals
# we set above. Fails fast if AWS_ / AISTOR_ / etc are missing.
verify_storage_creds || exit 1

run_row() {
    local id="$1" name="$2" fn="$3"
    if [[ -n "$ONLY_ROW" && "$ONLY_ROW" != "$id" ]]; then
        log_info "skipping row $id ($name) — --only=$ONLY_ROW"
        return 0
    fi
    if $fn; then
        log_pass="$id:$name"; PASS_ROWS+=("$log_pass")
    else
        log_fail="$id:$name"; FAIL_ROWS+=("$log_fail")
    fi
}

log_pass() { echo -e "\033[0;32m[PASS]\033[0m $*"; }
log_fail() { echo -e "\033[0;31m[FAIL]\033[0m $*"; }

run_row 1 baseline    row_baseline
run_row 2 dedup       row_dedup
run_row 3 dce         row_dce
run_row 4 encrypt     row_encrypt
run_row 5 storage-zstd  row_storage_zstd

echo ""
echo "========================================"
echo "Pipeline-layer matrix summary"
echo "========================================"
for r in "${PASS_ROWS[@]}"; do log_pass "$r"; done
for r in "${FAIL_ROWS[@]}"; do log_fail "$r"; done
if (( ${#PERF_LINES[@]} > 0 )); then
    echo ""
    echo "Per-row perf:"
    for line in "${PERF_LINES[@]}"; do echo "  $line"; done
fi
if (( ${#FAIL_ROWS[@]} > 0 )); then
    exit 1
fi
exit 0
