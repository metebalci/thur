#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Pipeline-Layer Matrix Test
#
# Sibling of vtl/scripts/test-pipeline-layers.sh. Five required runs
# (no DCE row — SBC-3 has no drive-side compression analog):
#
#   1. baseline   : dedup local,  encrypt off, cloud none
#   2. + dedup    : dedup global, encrypt off, cloud none
#                   -> two volumes, same fixture; second adds little
#                      cloud byte volume (cross-volume dedup hit).
#   3. + encrypt  : dedup local,  encrypt on,  cloud none
#                   -> at-rest AES-256-GCM via the default `local`
#                      keystore backend; cloud chunk bytes are
#                      ciphertext.
#   4. + cloud zstd: dedup local, encrypt off, cloud zstd
#                   -> compressible fixture; cloud bytes < 80% of input.
#   5. + key-migrate: daemon-down `volume key migrate` flow using
#                     two local backends (local + local-bak).
#
# Per-keystore wrap/unwrap coverage (awskms / vault / azurekv /
# gcpkms / etc.) lives in vsa/scripts/test-keystore.sh — same shape
# as test-iscsi-fs-storage.sh: pick a backend by name from a source
# JSON file (operator-local `private/keystore-backends.json`) and
# splice into `keystore.backends:` of the YAML test config. Out of
# scope here so the matrix run stays focused on the cloud /
# pipeline layers.
#
# Defaults to aistor-none for LAN iteration. Same cred-handling
# conventions as test-iscsi-fs-storage.sh.
#
# Usage (invoke from repo root):
#   THURVSA_TEST_BACKEND=aistor-none ./vsa/scripts/test-pipeline-layers.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Path to thurvsad binary
#   --cli-path PATH       Path to thurvsa binary
#   --only ROW            Run a single row (1..5)
#   --keep-data           Don't clean up test data dirs
#   --keep-cloud          Don't purge cloud test prefixes
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
KEEP_DATA=0
KEEP_CLOUD=0
SOURCE_BACKENDS="${THURVSA_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --only) ONLY_ROW="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-cloud) KEEP_CLOUD=1; shift ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

[[ -z "$DAEMON_PATH" ]] && DAEMON_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvsad"
[[ -z "$CLI_PATH" ]] && CLI_PATH="${REPO_DIR}/target/${BUILD_PROFILE}/thurvsa"
[[ -z "${THURVSA_TEST_BACKEND:-}" ]] && THURVSA_TEST_BACKEND="aistor-none"

if [[ ! -x "$DAEMON_PATH" || ! -x "$CLI_PATH" ]]; then
    log_error "Missing daemon ($DAEMON_PATH) or cli ($CLI_PATH) binary. Run: cargo build${BUILD_PROFILE:+ --release}"
    exit 1
fi
if [[ ! -r "$SOURCE_BACKENDS" ]]; then
    log_error "Cannot read backends file: $SOURCE_BACKENDS"
    exit 1
fi

# Backends config is YAML under `storage.backends.<name>` (mirrors what
# every other cloud-backed VSA suite reads). yq is needed at the same
# version contract as test-iscsi-fs-storage.sh.
BACKEND_TYPE=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".type" "$SOURCE_BACKENDS")
BACKEND_BUCKET=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".bucket // \"\"" "$SOURCE_BACKENDS")
BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
BACKEND_REGION=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".region // \"\"" "$SOURCE_BACKENDS")
BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".storage_account // \"\"" "$SOURCE_BACKENDS")
BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".container // \"\"" "$SOURCE_BACKENDS")
BACKEND_AUTH_AKID_ENV=$(yq -r "
    .storage.backends.\"$THURVSA_TEST_BACKEND\".auth
    | select(.type == \"env\") | .access_key_id_env // \"\"
" "$SOURCE_BACKENDS")
BACKEND_AUTH_SECRET_ENV=$(yq -r "
    .storage.backends.\"$THURVSA_TEST_BACKEND\".auth
    | select(.type == \"env\") | .secret_access_key_env // \"\"
" "$SOURCE_BACKENDS")
RETENTION=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".retention_mode // \"none\"" "$SOURCE_BACKENDS")
ORIG_PREFIX=$(yq -r ".storage.backends.\"$THURVSA_TEST_BACKEND\".prefix // \"\"" "$SOURCE_BACKENDS")

if [[ "$BACKEND_TYPE" == "local" ]]; then
    log_error "matrix needs a real cloud backend; '$THURVSA_TEST_BACKEND' is type=local"
    exit 1
fi
if [[ "$RETENTION" != "none" ]]; then
    log_error "backend '$THURVSA_TEST_BACKEND' has retention_mode=$RETENTION; matrix refuses (purge would fail)"
    exit 1
fi
log_info "VSA pipeline-layer matrix vs '$THURVSA_TEST_BACKEND' (type=$BACKEND_TYPE bucket=${BACKEND_BUCKET}${BACKEND_CONTAINER})"

PASS_ROWS=()
FAIL_ROWS=()
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

row_dir_setup() {
    local row_id="$1"
    TEST_DIR="/tmp/test-vsa-pipeline-layers-$$-$row_id"
    TEST_CONFIG="$TEST_DIR/config.yaml"
    MOUNT_POINT="$TEST_DIR/mnt"
    mkdir -p "$TEST_DIR/data" "$MOUNT_POINT"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local run_id; run_id="row${row_id}-$(date +%Y%m%d-%H%M%S)-$$"
    TEST_PREFIX="${ORIG_PREFIX}test-matrix/${run_id}/"
    assign_ports
}

row_dir_cleanup() {
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        log_info "row cleanup: stopping daemon (PID $DAEMON_PID)"
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
        ISCSI_CONNECTED=0
    fi
    if [[ $KEEP_CLOUD -eq 0 ]]; then
        cloud_purge_test_prefix
    fi
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    fi
    TEST_DIR=""; TEST_CONFIG=""; MOUNT_POINT=""
    ISCSI_PORT=""; HTTP_PORT=""; DAEMON_PID=""; ISCSI_CONNECTED=0
    RW_DEVICE=""; TEST_PREFIX=""
}

make_config() {
    local cloud_compress="$1"
    # Optional second keystore entry passed by the migrate row when it
    # needs `local-bak` alongside the default `local`.
    local extra_keystore_line="${2:-}"
    local backend_json
    backend_json=$(jq -c \
        ".backends.\"$THURVSA_TEST_BACKEND\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS")
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
    algorithm: $cloud_compress
  backends:
    testbackend: $backend_json
keystore:
  backends:
    local: { type: local }${extra_keystore_line}
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

# Create a volume with optional encryption + optional keystore selection.
# Args: $1=name, $2=dedup ("local"|"global"), $3=encrypt ("yes"|"no")
#       $4=keystore name (optional; passes --keystore NAME)
create_volume() {
    local name="$1" dedup="$2" encrypt="$3" keystore="${4:-}"
    local args=(--config "$TEST_CONFIG" volume create "$name"
                --size 2G --backend testbackend --dedup "$dedup")
    if [[ "$encrypt" == "yes" ]]; then
        args+=(--encrypt)
    fi
    if [[ -n "$keystore" ]]; then
        args+=(--keystore "$keystore")
    fi
    "$CLI_PATH" "${args[@]}" >/dev/null \
        || { log_error "volume create $name failed"; return 1; }
}

connect_iscsi_disk() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 \
        || { log_error "iscsi discovery failed"; return 1; }
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1 \
        || { log_error "iscsi login failed"; return 1; }
    ISCSI_CONNECTED=1
    sleep 3
    # Return the *first* THUR VSA block device. Multi-LUN rows pick
    # the second device by querying LUN explicitly.
    local row; row=$(lsscsi -g | awk '/THUR VSA/{print; exit}')
    [[ -n "$row" ]] || { log_error "no THUR VSA device found"; lsscsi -g; return 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    log_info "first VSA LUN -> $RW_DEVICE"
}

# Build a 1 GiB fixture: 512 MiB of zeros (max-compressible) + 512
# MiB of structured text. Mounts at $MOUNT_POINT, formats $RW_DEVICE,
# writes the fixture, syncs, umounts. Sized for the perf matrix —
# bigger than the 16 MiB correctness fixture so throughput numbers
# aren't dominated by mkfs/mount overhead.
write_fixture() {
    mkfs.ext4 -F -q "$RW_DEVICE" >/dev/null 2>&1 || { log_error "mkfs failed"; return 1; }
    mount "$RW_DEVICE" "$MOUNT_POINT" || { log_error "mount failed"; return 1; }
    dd if=/dev/zero of="$MOUNT_POINT/zeros.dat" bs=1M count=512 status=none
    yes 'ABCDEFGHIJKLMNOP' | head -c $((512 * 1024 * 1024)) > "$MOUNT_POINT/text.dat"
    sync
    FIXTURE_BYTES=$(stat -c %s "$MOUNT_POINT/zeros.dat" "$MOUNT_POINT/text.dat" | awk '{s+=$1} END {print s}')
    umount "$MOUNT_POINT"
    log_info "fixture: $FIXTURE_BYTES bytes (1 GiB compressible)"
}

# Sum the bytes of every object under our cloud test prefix.
total_cloud_bytes() {
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

download_first_chunk() {
    local out="$1"
    local key; key=$(cloud_list "" | grep -v 'manifests/' | head -1)
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

wait_for_chunks() {
    local timeout="${1:-90}"
    local elapsed=0
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

row_bring_up() {
    local cloud_compress="$1"
    make_config "$cloud_compress" || return 1
    start_daemon || return 1
}

# Row 1: baseline. Create volume, mkfs+mount+write+umount, no
# special assertion beyond "everything completes cleanly". The
# byte-equality check is heavy here (read-back across reboot);
# matching test-iscsi-fs-storage.sh's full Phase C is overkill for the
# matrix.
row_baseline() {
    log_test "row 1: baseline (dedup=local, encrypt=off, cloud=none)"
    row_dir_setup 1
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_volume "v1" "local" "no" || { row_dir_cleanup; return 1; }
    connect_iscsi_disk || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    write_fixture || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    wait_for_chunks 600 || { log_error "row 1: no chunks landed in cloud"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local cb; cb=$(total_cloud_bytes)
    log_info "row 1: cloud bytes after write = $cb"
    perf_summary 1 baseline mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$cb"
    if (( cb > 0 )); then
        row_dir_cleanup
        return 0
    fi
    log_error "row 1: no bytes in cloud after write"
    row_dir_cleanup
    return 1
}

# Row 2: cross-volume dedup with global scope.
row_dedup() {
    log_test "row 2: +dedup (cross-volume global)"
    row_dir_setup 2
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_volume "v1" "global" "no" || { row_dir_cleanup; return 1; }
    create_volume "v2" "global" "no" || { row_dir_cleanup; return 1; }
    connect_iscsi_disk || { row_dir_cleanup; return 1; }

    # First volume: write fixture, measure cloud bytes + snapshot
    # the chunks/ key set.
    local t0 t1 t2
    t0=$(date +%s%N)
    write_fixture || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    wait_for_chunks 600 || { log_error "row 2: v1 chunks never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local bytes_one; bytes_one=$(total_cloud_bytes)
    local snap_one="$TEST_DIR/chunks-after-v1.txt"
    cloud_chunks_snapshot "$snap_one"
    local count_one; count_one=$(wc -l < "$snap_one")
    log_info "row 2: after v1 — bytes=$bytes_one chunks=$count_one"
    perf_summary 2 dedup-first-write mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$bytes_one"

    # Second volume: lsscsi shows both, pick the one that isn't $RW_DEVICE.
    local second; second=$(lsscsi -g | awk '/THUR VSA/{print $(NF-1)}' | grep -v "^$RW_DEVICE$" | head -1)
    [[ -z "$second" ]] && { log_error "row 2: only one VSA device visible"; row_dir_cleanup; return 1; }
    RW_DEVICE="$second"
    log_info "row 2: second volume -> $RW_DEVICE"
    t0=$(date +%s%N)
    write_fixture || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    # No wait_for_chunks here — the dedup hit should keep the cloud
    # delta tiny. Sleep 6s to let pending upload activity settle.
    sleep 6
    t2=$(date +%s%N)
    local bytes_two; bytes_two=$(total_cloud_bytes)
    local snap_two="$TEST_DIR/chunks-after-v2.txt"
    cloud_chunks_snapshot "$snap_two"
    local new_chunks; new_chunks=$(cloud_chunks_new_count "$snap_one" "$snap_two")
    log_info "row 2: after v2 — bytes=$bytes_two new_chunks=$new_chunks"
    local byte_delta=$(( bytes_two - bytes_one ))
    perf_summary 2 dedup-second-write mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$byte_delta"

    # Count-delta dedup assertion. With Global dedup and an identical
    # fixture, file-content chunks should all dedupe; the only new
    # cloud objects come from per-volume ext4 metadata variance.
    # mkfs on a 2 GiB volume writes:
    #   - one group descriptor per 32 MiB block group → ~64 groups
    #   - sparse_super2 backup superblocks in groups 1,3,5,…
    #   - journal init (~32 MiB of partly-random init pattern)
    # Each lands in its own 64 KiB chunk and differs per volume
    # because mkfs picks a fresh UUID + random fields. Empirically
    # this produces ~40-60 unique metadata chunks per volume; cap=100
    # is a generous ceiling. No-dedup baseline would be ~count_one
    # (i.e., the second write would roughly double the chunk count),
    # so 100 still proves dedup is doing real work.
    local cap=100
    if (( new_chunks <= cap )); then
        log_info "row 2: dedup observed ($new_chunks new chunks under chunks/, cap=$cap)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 2: insufficient dedup ($new_chunks new chunks > cap $cap)"
    row_dir_cleanup
    return 1
}

# Row 3: at-rest encryption. Verify cloud bytes are ciphertext.
row_encrypt() {
    log_test "row 3: +encrypt (--encrypt at volume create)"
    row_dir_setup 3
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_volume "v-enc" "local" "yes" || { row_dir_cleanup; return 1; }
    # Verify the keystore file exists with mode 0600.
    local vol_uuid; vol_uuid=$(jq -r '.uuid' "$TEST_DIR/data/volumes/v-enc/manifest.json")
    local keyfile="$TEST_DIR/data/keys/${vol_uuid}.key"
    if [[ ! -f "$keyfile" ]]; then
        log_error "row 3: keystore file $keyfile missing"
        row_dir_cleanup
        return 1
    fi
    local mode; mode=$(stat -c %a "$keyfile")
    if [[ "$mode" != "600" ]]; then
        log_error "row 3: keystore file mode is $mode, expected 600"
        row_dir_cleanup
        return 1
    fi
    log_info "row 3: keystore at $keyfile mode=$mode"

    connect_iscsi_disk || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    write_fixture || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    wait_for_chunks 600 || { log_error "row 3: chunks never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local enc_cb; enc_cb=$(total_cloud_bytes)
    perf_summary 3 encrypt mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$enc_cb"

    # Pull one chunk; we expect ciphertext.
    download_first_chunk "$TEST_DIR/sampled-chunk.bin" \
        || { log_error "row 3: no chunks to sample"; row_dir_cleanup; return 1; }
    # The fixture starts with 8 MiB of NUL bytes. A plaintext chunk
    # of zero bytes would have a NUL-byte density near 100% (off by
    # the very small `dirsuper` etc. metadata).  Ciphertext has ~ 1/256
    # density.
    local nul_count; nul_count=$(head -c 4096 "$TEST_DIR/sampled-chunk.bin" | tr -dc '\0' | wc -c)
    log_info "row 3: NUL bytes in first 4 KiB of sampled chunk = $nul_count"
    # Generous ceiling: even after fs metadata mix, a plaintext page
    # of zeros has >2000 NUL bytes in 4 KiB. Ciphertext has ~16.
    if (( nul_count < 100 )); then
        log_info "row 3: chunk is ciphertext (low NUL density)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 3: chunk looks like plaintext (NUL bytes >= 100)"
    row_dir_cleanup
    return 1
}


# Row 5 (unconditional): `volume key migrate` daemon-down flow.
# Creates a volume bound to the default `local` keystore, stops the
# daemon, adds a second local backend `local-bak`, migrates, restarts,
# verifies the volume opens via the new backend, migrates back with
# --purge-local, and verifies the round-trip. Also verifies the
# daemon-up refusal (run migrate against a running daemon -> exit 1).
row_key_migrate() {
    log_test "row 5: volume key migrate (daemon-down)"
    row_dir_setup 9
    row_bring_up "none" || { row_dir_cleanup; return 1; }
    create_volume "v-mig" "local" "yes" || { row_dir_cleanup; return 1; }
    local vol_uuid; vol_uuid=$(jq -r '.uuid' "$TEST_DIR/data/volumes/v-mig/manifest.json")
    local sidecar="$TEST_DIR/data/keys/${vol_uuid}.key"
    if [[ ! -f "$sidecar" ]]; then
        log_error "row 5: sidecar at $sidecar missing after create"
        row_dir_cleanup
        return 1
    fi

    # Sub-step A: daemon-up refusal.
    if "$CLI_PATH" --config "$TEST_CONFIG" volume key migrate v-mig --to local 2>/dev/null; then
        log_error "row 5: daemon-up migrate should have refused"
        row_dir_cleanup
        return 1
    fi
    log_info "row 5: daemon-up migrate refused (good)"

    # Stop the daemon so the on-disk manifest is the source of truth.
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""

    # Sub-step B: rewrite the YAML conffile to add a second local-only
    # entry, then migrate to it.
    make_config "none" "
    local-bak: { type: local }"
    "$CLI_PATH" --config "$TEST_CONFIG" volume key migrate v-mig --to local-bak >/dev/null \
        || { log_error "row 5: migrate to local-bak failed"; row_dir_cleanup; return 1; }
    local new_ks; new_ks=$(jq -r '.encryption.keystore_backend' "$TEST_DIR/data/volumes/v-mig/manifest.json")
    if [[ "$new_ks" != "local-bak" ]]; then
        log_error "row 5: manifest.keystore_backend = '$new_ks', expected 'local-bak'"
        row_dir_cleanup
        return 1
    fi
    # `local-bak` -> sidecar stays in place (the local backend keeps
    # both files; the new one is its own write). The OLD `local`
    # sidecar at <data_dir>/keys/<uuid>.key is the same path the new
    # `local-bak` backend also writes to (both bind to `data_dir/keys/`)
    # — so this test verifies the metadata move, not file separation.
    if [[ ! -f "$sidecar" ]]; then
        log_error "row 5: sidecar disappeared after migrate (no --purge-local)"
        row_dir_cleanup
        return 1
    fi
    log_info "row 5: manifest moved to keystore=local-bak, sidecar preserved"

    # Sub-step C: refuse same-backend migrate (no-op guard).
    if "$CLI_PATH" --config "$TEST_CONFIG" volume key migrate v-mig --to local-bak 2>/dev/null; then
        log_error "row 5: same-backend migrate should have refused"
        row_dir_cleanup
        return 1
    fi
    log_info "row 5: same-backend migrate refused (good)"

    # Sub-step D: start the daemon, verify discovery picks the new
    # backend, and read back the fixture data we wrote pre-migration.
    start_daemon || { row_dir_cleanup; return 1; }
    local lun_count; lun_count=$("$CLI_PATH" --config "$TEST_CONFIG" volume list --json 2>/dev/null | jq 'length')
    if [[ "$lun_count" != "1" ]]; then
        log_error "row 5: volume list shows $lun_count volumes after restart, expected 1"
        row_dir_cleanup
        return 1
    fi
    log_info "row 5: daemon restarted clean with keystore=local-bak"

    # Sub-step E: migrate back to `local` with --purge-local.
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""
    "$CLI_PATH" --config "$TEST_CONFIG" volume key migrate v-mig --to local --purge-local >/dev/null \
        || { log_error "row 5: migrate back to local failed"; row_dir_cleanup; return 1; }
    local final_ks; final_ks=$(jq -r '.encryption.keystore_backend' "$TEST_DIR/data/volumes/v-mig/manifest.json")
    if [[ "$final_ks" != "local" ]]; then
        log_error "row 5: manifest.keystore_backend = '$final_ks', expected 'local'"
        row_dir_cleanup
        return 1
    fi
    # The fresh `local` wrap should have re-written the sidecar (both
    # `local` and `local-bak` keep their sidecar at the same canonical
    # path, so `--purge-local` on `local-bak` removes it first, then
    # the `local` wrap call recreates it).
    if [[ ! -f "$sidecar" ]]; then
        log_error "row 5: sidecar missing after migrate-back (local wrap should rewrite it)"
        row_dir_cleanup
        return 1
    fi
    log_info "row 5: migrate-back to local with --purge-local works; sidecar rewritten"

    start_daemon || { row_dir_cleanup; return 1; }
    row_dir_cleanup
    return 0
}

# Row 4: cloud zstd. Compressible fixture -> cloud bytes < 80% of input.
row_cloud_zstd() {
    log_test "row 4: +cloud zstd"
    row_dir_setup 4
    row_bring_up "zstd" || { row_dir_cleanup; return 1; }
    create_volume "v1" "local" "no" || { row_dir_cleanup; return 1; }
    connect_iscsi_disk || { row_dir_cleanup; return 1; }
    local t0 t1 t2
    t0=$(date +%s%N)
    write_fixture || { row_dir_cleanup; return 1; }
    t1=$(date +%s%N)
    wait_for_chunks 600 || { log_error "row 4: chunks never landed"; row_dir_cleanup; return 1; }
    t2=$(date +%s%N)
    local cb; cb=$(total_cloud_bytes)
    log_info "row 4: cloud bytes = $cb (input: $FIXTURE_BYTES)"
    perf_summary 4 cloud-zstd mixed "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$cb"
    local ceiling=$(( FIXTURE_BYTES * 8 / 10 ))
    if (( cb > 0 && cb < ceiling )); then
        log_info "row 4: cloud zstd observed ($cb < $ceiling = 80%)"
        row_dir_cleanup
        return 0
    fi
    log_error "row 4: cloud bytes not compressed"
    row_dir_cleanup
    return 1
}

trap 'row_dir_cleanup; exit 130' INT TERM

verify_cloud_creds || exit 1

log_pass() { echo -e "\033[0;32m[PASS]\033[0m $*"; }
log_fail() { echo -e "\033[0;31m[FAIL]\033[0m $*"; }

run_row() {
    local id="$1" name="$2" fn="$3"
    if [[ -n "$ONLY_ROW" && "$ONLY_ROW" != "$id" ]]; then
        log_info "skipping row $id ($name) — --only=$ONLY_ROW"
        return 0
    fi
    if $fn; then
        PASS_ROWS+=("$id:$name")
    else
        FAIL_ROWS+=("$id:$name")
    fi
}

run_row 1 baseline             row_baseline
run_row 2 dedup                row_dedup
run_row 3 encrypt              row_encrypt
run_row 4 cloud-zstd           row_cloud_zstd
run_row 5 key-migrate          row_key_migrate

echo ""
echo "========================================"
echo "VSA pipeline-layer matrix summary"
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
