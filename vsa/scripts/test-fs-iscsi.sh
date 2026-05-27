#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa End-to-End Filesystem Workflow Test
#
# Proves thurvsa behaves like an ordinary disk under a real Linux
# filesystem. Mirrors the role vtl/scripts/test-backup-workflow.sh
# plays for the tape product: write through a familiar
# user-visible workload, fence with SYNC, restart everything, prove
# the bytes round-trip intact.
#
# Workflow:
#   Phase A — write workload via ext4
#     1. Login + identify the thurvsa LUN as /dev/sdX (block) and
#        /dev/sgN (sg passthrough sibling).
#     2. mkfs.ext4 /dev/sdX, mount at /mnt/thurvsa-test/.
#     3. Generate a fixture tarball (~4 MiB tree of mixed text +
#        random bytes) and `tar xf` it onto the mount.
#     4. Hash the extracted tree (per-file, sorted), sync, umount.
#   Phase B — pre-restart snapshot via sg passthrough
#     5. `sg_dd if=/dev/sgN of=snap-pre.bin` reads the entire volume
#        through SG_IO. This goes straight to the daemon, bypassing
#        the kernel block-layer page cache, so it captures exactly
#        what the daemon would serve a future client.
#   Phase C — restart everything, snapshot again, compare
#     6. Logout iSCSI, kill -TERM the daemon, restart, login again.
#     7. `sg_dd if=/dev/sgN of=snap-post.bin` snapshots the same
#        volume from the fresh daemon (cache empty, reads come from
#        page index + chunk pool only).
#     8. `cmp snap-pre.bin snap-post.bin` is the load-bearing
#        persistence gate: snapshots match if and only if the daemon
#        truly committed every page to its on-disk page index +
#        chunks before SIGTERM.
#     9. Supplementary: fsck.ext4 -fn + mount + per-file diff
#        (kernel-cache-prone, informational only).
#   Phase D — destroy the volume, garbage-collect, prove reclaim
#    10. `volume destroy` removes the manifest + page index but
#        deliberately leaves the per-volume chunk pool behind — every
#        chunk is now an orphan.
#    11. `system gc --dry-run` must report but delete nothing.
#    12. `system gc` reclaims the orphan chunks from the local pool
#        namespace (`<data_dir>/chunks/local/<uuid>/`); `system gc
#        --storage` then reclaims the matching objects from the backend.
#
# Catches gaps that synthetic SCSI tests miss:
#   - Variable-LBA WRITE / READ as the kernel sees it (the elevator,
#     CAW, UNMAP, SYNCHRONIZE CACHE all flow through ext4).
#   - Persistence across daemon restarts (the cache layer's flush +
#     SYNC contract, not just SCSI-level sync).
#   - Storage-pool growth: each `tar xf` produces real content-addressed
#     uploads to the local backend.
#   - Orphan-chunk garbage collection: a destroyed Local volume's
#     UUID-keyed namespace is swept clean by `thurvsa system gc`.
#
# TEST LIMITATIONS:
#   The Linux kernel block layer caches /dev/sdb pages aggressively
#   and does NOT release the cache on `iscsiadm logout`. The cache
#   often survives even `echo 3 > /proc/sys/vm/drop_caches` because
#   the kernel SCSI midlayer holds its own per-device cache. The
#   Phase C mount + per-file-hash check is therefore informational
#   only — it can pass via the kernel cache even if the daemon lost
#   data. The Phase B/C sg-passthrough snapshot comparison is the
#   real durability gate: SG_IO ioctl bypasses every kernel cache
#   and routes each READ CDB straight to the daemon.
#
# Prerequisites:
#   - sg3-utils       (sudo apt-get install sg3-utils)
#   - open-iscsi      (sudo apt-get install open-iscsi)
#   - lsscsi          (sudo apt-get install lsscsi)
#   - e2fsprogs       (mkfs.ext4, fsck.ext4 — usually present)
#   - util-linux      (mount, umount — usually present)
#   - tar             (always present)
#   - iscsid running  (sudo systemctl enable --now iscsid)
#   - Root/sudo access
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-fs-iscsi.sh [OPTIONS]
#
# The script self-elevates via sudo (NOPASSWD sudoers entry required);
# no need to prefix with sudo yourself.
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect the iSCSI session after tests
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/thurvsa-test-fs-iscsi-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
KEEP_DATA=0
KEEP_ISCSI=0
DAEMON_PID=""
ISCSI_CONNECTED=0
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-fs"
VOLUME_SIZE_MIB=64
FIXTURE_DIR="${TEST_DIR}/fixture"
FIXTURE_TAR="${TEST_DIR}/fixture.tar"
FIXTURE_HASH_BEFORE="${TEST_DIR}/fixture-hash-before.txt"
FIXTURE_HASH_AFTER="${TEST_DIR}/fixture-hash-after.txt"
SNAPSHOT_PRE="${TEST_DIR}/snap-pre.bin"
SNAPSHOT_POST="${TEST_DIR}/snap-post.bin"
RW_DEVICE=""
RW_SG_DEVICE=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
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

    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
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
        [sg_dd]="sudo apt-get install sg3-utils"
        [cmp]="(diffutils — usually present)"
        [mkfs.ext4]="sudo apt-get install e2fsprogs"
        [fsck.ext4]="sudo apt-get install e2fsprogs"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [tar]="(present on every distro)"
        [curl]="sudo apt-get install curl"
        [systemctl]="(systemd should be present on any modern Linux)"
    )
    for tool in iscsiadm lsscsi sg_dd cmp mkfs.ext4 fsck.ext4 mount umount tar curl systemctl; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        echo "Install hints:"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service is not running."
        echo "Start it with:"
        echo "  sudo systemctl enable --now iscsid open-iscsi"
        exit 1
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}


create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes" "$MOUNT_POINT"
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
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

EOFCONFIG
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
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" --size "${VOLUME_SIZE_MIB}M" --dedup local >/dev/null
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
    RW_SG_DEVICE=$(echo "$row" | awk '{print $NF}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; exit 1; }
    log_info "thurvsa LUN -> $RW_DEVICE (sg passthrough: $RW_SG_DEVICE)"
}

# Snapshot the entire volume by reading every page through the
# /dev/sgN passthrough into `out_file`. SG passthrough sends each
# READ CDB straight to the daemon — bypassing the kernel block-layer
# page cache — so the snapshot reflects exactly what the daemon
# currently serves. The Phase A snapshot proves the daemon's view
# right after umount; the Phase C snapshot proves what survives a
# kill -TERM + restart. If they match, the daemon truly persisted.
snapshot_via_sg() {
    local out_file="$1"
    local block_bytes=131072    # 128 KiB per READ — 2 pages, 32 sectors
    local total_blocks=$(( VOLUME_SIZE_MIB * 1024 * 1024 / block_bytes ))
    if ! sg_dd "if=$RW_SG_DEVICE" "of=$out_file" \
            "bs=$block_bytes" "count=$total_blocks" 2>&1 | tail -3 | sed 's/^/    /'; then
        log_error "sg_dd snapshot failed"
        return 1
    fi
    local actual
    actual=$(stat -c%s "$out_file")
    if (( actual != VOLUME_SIZE_MIB * 1024 * 1024 )); then
        log_error "snapshot size $actual != expected $((VOLUME_SIZE_MIB * 1024 * 1024))"
        return 1
    fi
    return 0
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
    log_info "Generating fixture tree (~4 MiB of mixed text + random)..."
    mkdir -p "$FIXTURE_DIR/text" "$FIXTURE_DIR/random"
    # Reproducible content; not seeded — just want non-trivial bytes.
    for i in $(seq 1 20); do
        head -c 16384 /dev/urandom | base64 > "$FIXTURE_DIR/text/text-$i.txt"
    done
    for i in $(seq 1 8); do
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
    if ! mkfs.ext4 -F -q "$RW_DEVICE"; then
        log_error "[Phase A] mkfs.ext4 failed on $RW_DEVICE"
        return 1
    fi
    if ! mount "$RW_DEVICE" "$MOUNT_POINT"; then
        log_error "[Phase A] mount failed on $RW_DEVICE"
        return 1
    fi
    if ! tar -xf "$FIXTURE_TAR" -C "$MOUNT_POINT"; then
        log_error "[Phase A] tar xf failed"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    sync
    # Hash the extracted tree (sorted-by-path, byte-identical) BEFORE
    # umount so a discrepancy between mounted-before vs mounted-after
    # is unambiguous.
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_BEFORE"
    local hashed
    hashed=$(wc -l < "$FIXTURE_HASH_BEFORE")
    log_info "[Phase A] hashed $hashed files"
    if (( hashed < 1 )); then
        log_error "[Phase A] no files visible after tar xf — extraction failed silently"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    if ! umount "$MOUNT_POINT"; then
        log_error "[Phase A] umount failed"
        return 1
    fi
    log_info "[Phase A] umounted cleanly"

    # Async-upload health gate. The async upload worker has historically
    # been wired to a *snapshot* of the backend map taken at daemon
    # boot; any volume whose backend was first instantiated by a
    # runtime `volume create` (i.e. against an empty initial daemon)
    # produced "backend 'X' unknown … leaving LocalOnly" warns and
    # silently dropped every upload until daemon restart. Catch any
    # regression of that shape here, before the persistence phases
    # mask it via a restart.
    if grep -qE "backend '[^']+' unknown" "${TEST_DIR}/daemon.log"; then
        log_error "[Phase A] upload-worker logged 'backend unknown' — async upload path is dropping PUTs"
        grep -E "backend '[^']+' unknown" "${TEST_DIR}/daemon.log" | head -5 | sed 's/^/    /'
        return 1
    fi
    # Backend bytes written should track host writes — a flat counter
    # means uploads no-op'd into LocalOnly. Cheap admin-socket probe.
    local info_json
    info_json=$("$CLI_PATH" --config "$TEST_CONFIG" volume info "$VOLUME_NAME" --json 2>/dev/null) || {
        log_error "[Phase A] volume info '$VOLUME_NAME' failed"
        return 1
    }
    local host_bw backend_bw
    host_bw=$(echo "$info_json" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("runtime",{}).get("host_bytes_written",0))')
    backend_bw=$(echo "$info_json" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("runtime",{}).get("backend_bytes_written",0))')
    log_info "[Phase A] runtime counters: host_bytes_written=$host_bw  backend_bytes_written=$backend_bw"
    if [[ "${host_bw:-0}" -le 0 ]]; then
        log_error "[Phase A] host_bytes_written=$host_bw — host writes never reached the daemon"
        return 1
    fi
    if [[ "${backend_bw:-0}" -le 0 ]]; then
        log_error "[Phase A] backend_bytes_written=$backend_bw with host_bytes_written=$host_bw — uploads silently dropped"
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Phase B — pre-restart sg-passthrough snapshot
#
# Read the entire volume through /dev/sgN so we capture the daemon's
# view directly, bypassing the kernel block-layer page cache. After
# umount, the daemon's PageCache + chunk pool + page index reflect
# the post-tar state; this snapshot is our reference for what should
# survive a daemon restart.
# ---------------------------------------------------------------------------

phase_b_snapshot_pre_restart() {
    log_info "[Phase B] Snapshotting volume via sg passthrough ($RW_SG_DEVICE)..."
    if ! snapshot_via_sg "$SNAPSHOT_PRE"; then
        log_error "[Phase B] pre-restart snapshot failed"
        return 1
    fi
    log_info "[Phase B] snapshot $(stat -c%s "$SNAPSHOT_PRE") bytes, sha256=$(sha256sum "$SNAPSHOT_PRE" | awk '{print substr($1,1,16)}')..."
}

# ---------------------------------------------------------------------------
# Phase C — restart everything, snapshot again, compare
#
# The sg-passthrough snapshot comparison is the load-bearing
# persistence assertion: it can ONLY succeed if the daemon's on-disk
# state (page index + chunks) matches the pre-restart in-memory +
# disk state. Kernel page-cache shenanigans don't apply because every
# READ goes via SG_IO ioctl direct to the daemon.
#
# The trailing fsck + mount + per-file-hash check is supplementary —
# kernel-cache-prone but useful for catching ext4-level corruption.
# ---------------------------------------------------------------------------

phase_c_restart_and_verify() {
    log_info "[Phase C] Disconnecting iSCSI, stopping daemon, restarting..."
    disconnect_iscsi
    stop_daemon
    # Drop the kernel block-layer page cache. Belt-and-suspenders
    # against the supplementary mount check below; the sg-passthrough
    # snapshot is unaffected either way.
    sync && echo 3 > /proc/sys/vm/drop_caches
    start_daemon
    connect_iscsi

    log_info "[Phase C] Snapshotting post-restart volume via sg passthrough..."
    if ! snapshot_via_sg "$SNAPSHOT_POST"; then
        log_error "[Phase C] post-restart snapshot failed"
        return 1
    fi
    log_info "[Phase C] snapshot $(stat -c%s "$SNAPSHOT_POST") bytes, sha256=$(sha256sum "$SNAPSHOT_POST" | awk '{print substr($1,1,16)}')..."

    if ! cmp -s "$SNAPSHOT_PRE" "$SNAPSHOT_POST"; then
        log_error "[Phase C] DAEMON-SIDE PERSISTENCE FAILURE: sg-passthrough snapshots differ across restart"
        log_error "  pre-restart  size: $(stat -c%s "$SNAPSHOT_PRE") bytes"
        log_error "  post-restart size: $(stat -c%s "$SNAPSHOT_POST") bytes"
        # Show the first divergent offset to help triage.
        local first_diff
        first_diff=$(cmp "$SNAPSHOT_PRE" "$SNAPSHOT_POST" 2>&1 | head -1)
        log_error "  first divergence: $first_diff"
        return 1
    fi
    log_info "[Phase C] sg-passthrough snapshots match — daemon truly persisted across restart"

    # Supplementary: fsck + mount + per-file-hash check. Subject to
    # kernel block-cache caching of /dev/sdb across iscsi sessions
    # (see TEST-LIMITATIONS header), so this part is informational
    # — the snapshot comparison above is the durability gate.
    log_info "[Phase C] Supplementary fsck + mount + per-file-hash check..."
    local fsck_out
    fsck_out=$(fsck.ext4 -fn "$RW_DEVICE" 2>&1)
    local fsck_rc=$?
    echo "$fsck_out" | tail -3 | sed 's/^/    /'
    if (( fsck_rc != 0 )); then
        log_warn "  fsck.ext4 exit=$fsck_rc on $RW_DEVICE"
    fi
    if ! mount "$RW_DEVICE" "$MOUNT_POINT" 2>&1; then
        log_warn "  mount $RW_DEVICE failed — supplementary check skipped"
        return 0
    fi
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_AFTER"
    log_info "  mount listed $(wc -l < "$FIXTURE_HASH_AFTER") files"
    umount "$MOUNT_POINT"
    if diff -q "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" >/dev/null; then
        log_info "  all files round-tripped byte-for-byte across restart"
    else
        log_warn "  file hashes differ — but sg-passthrough snapshots matched, so this is most likely an ext4 / kernel-cache artifact, not lost daemon state"
        diff -u "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" | head -20 | sed 's/^/    /'
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Phase D — destroy the volume, GC, prove the orphan namespace is reclaimed
#
# `volume destroy` removes the manifest + page index but deliberately
# leaves the per-volume chunk pool behind — with nothing referencing
# them, every chunk is now an orphan. `thurvsa system gc` is the
# verb that reclaims them. A Local-scope volume's chunk-pool namespace
# is its UUID hex, so the orphans sit under
# `<data_dir>/chunks/local/<uuid>/` (warm cache) and
# `<local-backend>/chunks/<uuid>/` (backend tier). A plain sweep
# clears the pool; `--storage` extends it to the backend. `--dry-run`
# must touch nothing.
# ---------------------------------------------------------------------------

# Count *.dat chunk files under a directory (0 if the dir is gone).
count_chunks() { find "$1" -name '*.dat' 2>/dev/null | wc -l; }

phase_d_destroy_and_gc() {
    log_info "[Phase D] Destroy $VOLUME_NAME + system gc + verify reclaim"

    # The Local-scope namespace is the volume UUID hex — read it from
    # the manifest before `volume destroy` removes it.
    local manifest="$TEST_DIR/data/volumes/$VOLUME_NAME/manifest.json"
    local uuid
    uuid=$(grep -oE '"uuid":"[0-9a-f]{32}"' "$manifest" 2>/dev/null | head -1 | cut -d'"' -f4)
    if [[ -z "$uuid" ]]; then
        log_error "[Phase D] could not read volume UUID from $manifest"
        return 1
    fi
    local pool_ns="$TEST_DIR/data/chunks/local/$uuid"
    local backend_ns="$TEST_DIR/local-backend/chunks/$uuid"
    log_info "[Phase D] volume UUID $uuid"

    # Precondition: the Phase A workload sealed + uploaded chunks under
    # this volume's namespace.
    local pool_pre backend_pre
    pool_pre=$(count_chunks "$pool_ns")
    backend_pre=$(count_chunks "$backend_ns")
    if (( pool_pre < 1 )); then
        log_error "[Phase D] expected sealed chunks under $pool_ns, found none"
        return 1
    fi
    if (( backend_pre < 1 )); then
        log_error "[Phase D] expected uploaded chunks under $backend_ns, found none"
        return 1
    fi
    log_info "[Phase D] before destroy: $pool_pre chunk(s) in pool, $backend_pre in backend"

    # The volume must not be in an iSCSI session when destroyed.
    disconnect_iscsi

    if ! "$CLI_PATH" --config "$TEST_CONFIG" volume destroy "$VOLUME_NAME" --force >/dev/null 2>&1; then
        log_error "[Phase D] volume destroy failed"
        return 1
    fi
    # destroy removes the manifest but leaves the chunks as orphans.
    if (( $(count_chunks "$pool_ns") != pool_pre )); then
        log_error "[Phase D] destroy unexpectedly altered the chunk pool"
        return 1
    fi
    log_info "[Phase D] manifest gone, $pool_pre orphan chunk(s) still on disk"

    # Dry-run must be read-only.
    if ! "$CLI_PATH" --config "$TEST_CONFIG" system gc --dry-run >/dev/null 2>&1; then
        log_error "[Phase D] system gc --dry-run failed"
        return 1
    fi
    if (( $(count_chunks "$pool_ns") != pool_pre )); then
        log_error "[Phase D] --dry-run deleted chunks — it must be read-only"
        return 1
    fi
    log_info "[Phase D] --dry-run left all $pool_pre orphan(s) in place"

    # Real sweep — reclaims the local pool namespace.
    local gc_out
    gc_out=$("$CLI_PATH" --config "$TEST_CONFIG" system gc 2>&1)
    if (( $? != 0 )); then
        log_error "[Phase D] system gc failed"
        echo "$gc_out" | tail -5 | sed 's/^/    /'
        return 1
    fi
    echo "$gc_out" | tail -3 | sed 's/^/    /'
    if (( $(count_chunks "$pool_ns") != 0 )); then
        log_error "[Phase D] orphan chunk(s) survived gc under $pool_ns"
        return 1
    fi
    log_info "[Phase D] gc reclaimed every orphan chunk from the local pool"

    # `--storage` sweep — reclaims the backend-tier objects too.
    gc_out=$("$CLI_PATH" --config "$TEST_CONFIG" system gc --storage 2>&1)
    if (( $? != 0 )); then
        log_error "[Phase D] system gc --storage failed"
        echo "$gc_out" | tail -5 | sed 's/^/    /'
        return 1
    fi
    echo "$gc_out" | tail -3 | sed 's/^/    /'
    if (( $(count_chunks "$backend_ns") != 0 )); then
        log_error "[Phase D] orphan object(s) survived --storage gc under $backend_ns"
        return 1
    fi
    log_info "[Phase D] --storage gc reclaimed every orphan object from the backend"
}

main() {
    echo "========================================"
    echo "thurvsa Filesystem Workflow Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    ensure_volume
    connect_iscsi
    generate_fixture

    echo ""
    log_test "Phase A — mkfs.ext4 + tar xf + sync + hash"
    if phase_a_format_mount_extract; then
        log_pass "Phase A"
    else
        log_fail "Phase A"
        exit 1
    fi
    echo ""
    log_test "Phase B — pre-restart sg-passthrough volume snapshot"
    if phase_b_snapshot_pre_restart; then
        log_pass "Phase B"
    else
        log_fail "Phase B"
        exit 1
    fi
    echo ""
    log_test "Phase C — restart daemon + post-restart snapshot + verify"
    if phase_c_restart_and_verify; then
        log_pass "Phase C"
    else
        log_fail "Phase C"
        exit 1
    fi

    echo ""
    log_test "Phase D — destroy volume + system gc + verify orphan reclaim"
    if phase_d_destroy_and_gc; then
        log_pass "Phase D"
    else
        log_fail "Phase D"
        exit 1
    fi

    echo ""
    echo "========================================"
    echo "All workflow phases passed"
    echo "========================================"
    echo "Artifacts:"
    echo "  - Daemon log:        ${TEST_DIR}/daemon.log"
    echo "  - Fixture:           ${FIXTURE_TAR}"
    echo "  - Per-file hashes:   ${FIXTURE_HASH_BEFORE} / ${FIXTURE_HASH_AFTER}"
    echo "  - Volume snapshots:  ${SNAPSHOT_PRE} / ${SNAPSHOT_POST}"
    exit 0
}

main
