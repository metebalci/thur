#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa End-to-End Filesystem Workflow Test (NVMe/TCP)
#
# NVMe/TCP twin of test-iscsi-fs-workflow.sh — same three-phase persistence
# proof, same Linux-filesystem workload, but driven through Linux
# `nvme-cli` + `nvme_tcp` kernel module instead of `open-iscsi`. Proves
# thurvsa behaves like an ordinary NVMe namespace under a real
# filesystem on the host.
#
# Workflow:
#   Phase A — write workload via ext4
#     1. nvme connect to the thurvsa subsystem, identify /dev/nvmeXn1.
#     2. mkfs.ext4 /dev/nvmeXn1, mount at /mnt/thurvsa-test/.
#     3. Generate a fixture tarball (~4 MiB tree of mixed text +
#        random bytes) and `tar xf` it onto the mount.
#     4. Hash the extracted tree (per-file, sorted), sync, umount.
#   Phase B — pre-restart snapshot via O_DIRECT
#     5. `dd if=/dev/nvmeXn1 iflag=direct` reads the entire volume
#        bypassing the kernel page cache. NVMe's analog of sg_dd —
#        O_DIRECT skips the buffer cache and routes each READ command
#        straight to the daemon (unlike SCSI, NVMe doesn't have a
#        separate midlayer cache).
#   Phase C — restart everything, snapshot again, compare
#     6. `nvme disconnect`, kill -TERM the daemon, restart, reconnect.
#     7. Snapshot the post-restart volume the same way.
#     8. `cmp snap-pre.bin snap-post.bin` is the load-bearing
#        persistence gate: snapshots match iff the daemon truly
#        committed every page to disk before SIGTERM.
#     9. Supplementary: fsck.ext4 -fn + mount + per-file diff
#        (informational only — Linux page cache might mask data loss
#        on /dev/nvmeXn1 the way it does on /dev/sdX).
#
# Catches gaps that synthetic NVMe codec tests miss:
#   - Variable-LBA NVMe Read/Write/Flush as ext4 emits them under a
#     real filesystem workload (the elevator, fused CAW, DSM
#     deallocate, NVMe Flush all flow through the kernel block layer).
#   - Persistence across daemon restarts via the PageCache flush +
#     SYNC fence (Flush on NVMe is the analog of SCSI SYNCHRONIZE
#     CACHE; both route to `PageCache::synchronize_bytes`).
#   - Cloud-pool growth: each `tar xf` produces real content-addressed
#     uploads to the local backend.
#
# Prerequisites:
#   - nvme-cli         (sudo apt-get install nvme-cli)
#   - nvme_tcp kernel module (sudo modprobe nvme_tcp)
#   - e2fsprogs        (mkfs.ext4, fsck.ext4 — usually present)
#   - util-linux       (mount, umount — usually present)
#   - tar              (always present)
#   - thurvsad and thurvsa (built or on PATH)
#   - Root/sudo access (nvme connect + raw /dev/nvmeXn1 access require root)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-nvme-fs-workflow.sh [OPTIONS]
#
# The script self-elevates via sudo (NOPASSWD sudoers entry required).
#
# Options:
#   --release             Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --keep-nvme           Don't disconnect the NVMe session after tests
#   --nvmetcp-port PORT   Override NVMe/TCP port (default: free ephemeral port)
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
TEST_DIR="/tmp/thurvsa-test-nvme-fs-workflow-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
NVMETCP_PORT=""
HTTP_PORT=""
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-fs-workflow-test"
KEEP_DATA=0
KEEP_NVME=0
DAEMON_PID=""
NVME_CONNECTED=0
NVME_DEVICE=""
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-fs"
VOLUME_SIZE_MIB=64
FIXTURE_DIR="${TEST_DIR}/fixture"
FIXTURE_TAR="${TEST_DIR}/fixture.tar"
FIXTURE_HASH_BEFORE="${TEST_DIR}/fixture-hash-before.txt"
FIXTURE_HASH_AFTER="${TEST_DIR}/fixture-hash-after.txt"
SNAPSHOT_PRE="${TEST_DIR}/snap-pre.bin"
SNAPSHOT_POST="${TEST_DIR}/snap-post.bin"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-nvme) KEEP_NVME=1; shift ;;
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
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

    if [[ $NVME_CONNECTED -eq 1 && $KEEP_NVME -eq 0 ]]; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    fi

    if [[ -n "$DAEMON_PID" ]]; then
        log_info "Stopping daemon (PID: $DAEMON_PID)"
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
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
        [sha256sum]="sudo apt-get install coreutils"
    )
    for tool in nvme mkfs.ext4 fsck.ext4 mount umount tar curl sha256sum; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
            hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
        missing+=("nvme_tcp kernel module")
        hints+=("  - nvme_tcp: sudo modprobe nvme_tcp (kernel >= 5.0 required)")
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
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes" "$MOUNT_POINT"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

transport: nvmetcp

http:
  listen: "127.0.0.1:$HTTP_PORT"

nvmetcp:
  listen: "0.0.0.0:$NVMETCP_PORT"

audit:
  enabled: true
cloud:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

EOFCONFIG    mkdir -p "$TEST_DIR/local-backend"
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad (NVMe/TCP)..."
    RUST_LOG="info,nvme_tcp=debug" \
        "$DAEMON_PATH" --config "$TEST_CONFIG" >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    # NVMe/TCP transport doesn't bind the HTTP port until later in
    # startup, so poll the listener directly.
    for _ in $(seq 1 30); do
        if ss -tln 2>/dev/null | grep -q ":$NVMETCP_PORT\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $NVMETCP_PORT)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon failed to bind port $NVMETCP_PORT"
    tail -50 "${TEST_DIR}/daemon.log"
    exit 1
}

stop_daemon() {
    if [[ -n "$DAEMON_PID" ]]; then
        log_info "Stopping daemon (PID $DAEMON_PID)..."
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
}

ensure_volume() {
    if "$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null | grep -q "$VOLUME_NAME"; then
        log_info "Volume $VOLUME_NAME already present (reusing across restarts)"
        return 0
    fi
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size "${VOLUME_SIZE_MIB}M" --backend local --dedup local >/dev/null
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
    # nvme list-subsys json lacks the controller name on some
    # distros — `python3` walk handles both the old and new shapes.
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
        # Fallback: pick the newest /dev/nvme*n1 — the one we just
        # created should be the highest-numbered.
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
        # Give the kernel a moment to tear down /dev/nvmeXn1.
        sleep 1
    fi
}

# Snapshot the entire namespace via O_DIRECT. NVMe's analog of the
# iSCSI test's sg_dd snapshot: iflag=direct on /dev/nvmeXn1 bypasses
# the kernel buffer cache and routes each READ command straight to
# the daemon (the NVMe driver doesn't keep a separate midlayer cache,
# unlike the SCSI stack). The Phase B snapshot captures the daemon's
# view right after umount; the Phase C snapshot captures what
# survives a kill -TERM + restart. cmp on the two is the durability
# gate.
snapshot_via_direct() {
    local out_file="$1"
    local block="/dev/${NVME_DEVICE}n1"
    local total_bytes=$(( VOLUME_SIZE_MIB * 1024 * 1024 ))
    # 1 MiB I/Os — fits inside our advertised MAXH2CDATA + reads
    # multi-page-at-once so the daemon's read path exercises page
    # cache misses + chunk-pool fetches similar to real workloads.
    if ! dd "if=$block" "of=$out_file" bs=1M "count=$VOLUME_SIZE_MIB" \
            iflag=direct status=none 2>"$TEST_DIR/snapshot-dd.err"; then
        log_error "dd snapshot failed: $(cat "$TEST_DIR/snapshot-dd.err")"
        return 1
    fi
    local actual
    actual=$(stat -c%s "$out_file")
    if (( actual != total_bytes )); then
        log_error "snapshot size $actual != expected $total_bytes"
        return 1
    fi
    return 0
}

generate_fixture() {
    log_info "Generating fixture tree (~4 MiB of mixed text + random)..."
    mkdir -p "$FIXTURE_DIR/text" "$FIXTURE_DIR/random"
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
    local block="/dev/${NVME_DEVICE}n1"
    log_info "[Phase A] mkfs.ext4 + mount + tar xf + sync on $block"
    if ! mkfs.ext4 -F -q "$block"; then
        log_error "[Phase A] mkfs.ext4 failed on $block"
        return 1
    fi
    if ! mount "$block" "$MOUNT_POINT"; then
        log_error "[Phase A] mount failed on $block"
        return 1
    fi
    if ! tar -xf "$FIXTURE_TAR" -C "$MOUNT_POINT"; then
        log_error "[Phase A] tar xf failed"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    sync
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_BEFORE"
    local hashed
    hashed=$(wc -l < "$FIXTURE_HASH_BEFORE")
    log_info "[Phase A] hashed $hashed files"
    if (( hashed < 1 )); then
        log_error "[Phase A] no files visible after tar xf"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    if ! umount "$MOUNT_POINT"; then
        log_error "[Phase A] umount failed"
        return 1
    fi
    log_info "[Phase A] umounted cleanly"
}

phase_b_snapshot_pre_restart() {
    log_info "[Phase B] Snapshotting volume via O_DIRECT (/dev/${NVME_DEVICE}n1)..."
    if ! snapshot_via_direct "$SNAPSHOT_PRE"; then
        log_error "[Phase B] pre-restart snapshot failed"
        return 1
    fi
    log_info "[Phase B] snapshot $(stat -c%s "$SNAPSHOT_PRE") bytes, sha256=$(sha256sum "$SNAPSHOT_PRE" | awk '{print substr($1,1,16)}')..."
}

phase_c_restart_and_verify() {
    log_info "[Phase C] Disconnecting NVMe, stopping daemon, restarting..."
    disconnect_nvme
    stop_daemon
    # Drop the kernel block-layer page cache before the post-restart
    # snapshot — belt-and-suspenders since the snapshot uses O_DIRECT
    # anyway, but ensures the supplementary mount check below doesn't
    # see ghost data from the pre-restart session.
    sync && echo 3 > /proc/sys/vm/drop_caches
    start_daemon
    if ! connect_nvme; then
        log_error "[Phase C] reconnect failed"
        return 1
    fi

    log_info "[Phase C] Snapshotting post-restart volume via O_DIRECT..."
    if ! snapshot_via_direct "$SNAPSHOT_POST"; then
        log_error "[Phase C] post-restart snapshot failed"
        return 1
    fi
    log_info "[Phase C] snapshot $(stat -c%s "$SNAPSHOT_POST") bytes, sha256=$(sha256sum "$SNAPSHOT_POST" | awk '{print substr($1,1,16)}')..."

    if ! cmp -s "$SNAPSHOT_PRE" "$SNAPSHOT_POST"; then
        log_error "[Phase C] DAEMON-SIDE PERSISTENCE FAILURE: O_DIRECT snapshots differ across restart"
        log_error "  pre-restart  size: $(stat -c%s "$SNAPSHOT_PRE") bytes"
        log_error "  post-restart size: $(stat -c%s "$SNAPSHOT_POST") bytes"
        local first_diff
        first_diff=$(cmp "$SNAPSHOT_PRE" "$SNAPSHOT_POST" 2>&1 | head -1)
        log_error "  first divergence: $first_diff"
        return 1
    fi
    log_info "[Phase C] snapshots match — daemon truly persisted across restart"

    # Supplementary: fsck + mount + per-file-hash check. Mounting
    # the same namespace via the kernel block layer can hit cached
    # pages from the pre-restart session in some setups (we did
    # drop_caches but the NVMe driver state is not guaranteed to
    # match), so this is informational; the snapshot cmp above is
    # the load-bearing durability gate.
    local block="/dev/${NVME_DEVICE}n1"
    log_info "[Phase C] Supplementary fsck.ext4 -fn + mount + per-file-hash check..."
    local fsck_out fsck_rc
    fsck_out=$(fsck.ext4 -fn "$block" 2>&1)
    fsck_rc=$?
    echo "$fsck_out" | tail -3 | sed 's/^/    /'
    if (( fsck_rc != 0 )); then
        log_warn "  fsck.ext4 exit=$fsck_rc on $block"
    fi
    if ! mount "$block" "$MOUNT_POINT" 2>&1; then
        log_warn "  mount $block failed — supplementary check skipped"
        return 0
    fi
    (cd "$MOUNT_POINT" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$FIXTURE_HASH_AFTER"
    log_info "  mount listed $(wc -l < "$FIXTURE_HASH_AFTER") files"
    umount "$MOUNT_POINT"
    if diff -q "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" >/dev/null; then
        log_info "  all files round-tripped byte-for-byte across restart"
    else
        log_warn "  file hashes differ — but O_DIRECT snapshots matched, so this is most likely a kernel-cache artifact, not lost daemon state"
        diff -u "$FIXTURE_HASH_BEFORE" "$FIXTURE_HASH_AFTER" | head -20 | sed 's/^/    /'
    fi
    return 0
}

main() {
    echo "========================================"
    echo "thurvsa Filesystem Workflow Test (NVMe/TCP)"
    echo "========================================"
    echo ""

    check_prerequisites
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
    if phase_a_format_mount_extract; then
        log_pass "Phase A"
    else
        log_fail "Phase A"
        exit 1
    fi
    echo ""
    log_test "Phase B — pre-restart O_DIRECT volume snapshot"
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
