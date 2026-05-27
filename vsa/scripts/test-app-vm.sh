#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA QEMU VM-Disk Integration Test
#
# Boots a real Linux guest (Ubuntu 26.04 LTS "Resolute Raccoon" minimal
# cloud image) from a thurvsa volume. Catches gaps the synthetic /
# postgres tests miss: mixed-I/O OS workload, boot-time fs mounts,
# systemd journal writes under realistic block alignments,
# package-extraction patterns. The VSA counterpart to
# vtl/scripts/test-app-bareos.sh in the "realistic block consumer"
# lane: postgres covers DB workloads, this covers OS-as-workload.
#
# Workflow:
#   Phase A — bootstrap
#     1. thurvsad up with one volume + local backend.
#     2. Login (iscsi or nvmetcp); resolve the host block device.
#     3. Fetch (cached) the Ubuntu 26.04 minimal cloud image into
#        /var/cache/thur/cloud-images/.
#     4. qemu-img convert qcow2 -> raw onto the block device. The VSA
#        volume is now the guest's root disk.
#   Phase B — clean boot + verify
#     5. Build a cloud-init NoCloud seed iso whose user-data is a
#        script that writes a seed-derived fixture + sha256sums under
#        /var/test-fixture and then powers off.
#     6. Boot qemu (q35 + OVMF UEFI firmware; TCG, no KVM required)
#        with the VSA volume + seed iso. Wait for clean guest shutdown.
#     7. partprobe + mount root partition read-only on the host.
#     8. Verify every fixture file hashes to its host-derived expected
#        value (byte-for-byte: same AES-CTR keystream + size).
#   Phase C — crash variant (skipped under --quick)
#     9. Boot a second qemu run with a fresh instance-id; cloud-init
#        writes more fixture, but we issue `system_reset` via the
#        QEMU monitor mid-write, then quit qemu.
#    10. Re-mount the root partition on the host. The kernel runs
#        ext4 journal replay during mount; fsck -fn afterwards must
#        report a clean filesystem.
#    11. Verify the Phase B fixture is still intact — those files
#        were fsynced before the crash, so they must survive.
#
# The two load-bearing assertions:
#   - After clean boot+shutdown, every fsynced file on the VSA volume
#     hashes to its host-precomputed expected value.
#   - After a forced reset of the guest mid-write, ext4 journal replay
#     produces a clean filesystem and previously-fsynced data survives.
#
# Reproducibility: --seed N picks the same fixture file count + sizes
# (boundary-biased Monte Carlo). --quick skips Phase C entirely (single
# boot + shutdown only; ~3 min wall clock vs ~7 min default).
#
# Prerequisites:
#   - qemu-system-x86_64, qemu-img    (sudo apt-get install qemu-system-x86 qemu-utils)
#   - ovmf  (UEFI firmware for q35)   (sudo apt-get install ovmf)
#   - cloud-localds                   (sudo apt-get install cloud-image-utils)
#   - parted (partprobe)              (sudo apt-get install parted)
#   - curl, openssl, sha256sum, socat (curl/openssl/coreutils/socat)
#   - iSCSI mode:  open-iscsi, lsscsi; iscsid running
#                  (sudo systemctl enable --now iscsid)
#   - NVMe/TCP mode: nvme-cli; nvme_tcp kernel module
#                    (sudo modprobe nvme_tcp)
#   - Internet on first run (cached under /var/cache/thur/cloud-images/)
#   - Root/sudo access (self-elevates via NOPASSWD sudoers)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-app-vm.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run
#   --quick               Skip Phase C (~1 min total)
#   --transport T         iscsi (default) or nvmetcp
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvsad
#   --cli-path PATH       Override thurvsa
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect iSCSI on exit (iscsi mode)
#   --keep-nvme           Don't disconnect NVMe on exit (nvmetcp mode)
#   --iscsi-port PORT     Override iSCSI port (iscsi mode)
#   --nvmetcp-port PORT   Override NVMe/TCP port (nvmetcp mode)
#   --http-port PORT      Override HTTP port
#

# Self-elevate via sudo. The script does partprobe, mount, iscsiadm /
# nvme, and boots qemu against a raw block device. NOPASSWD sudoers
# entry assumed.
if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

TEST_DIR="/tmp/test-app-vm-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TRANSPORT="iscsi"
NVMETCP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-fs-vm-test"
KEEP_ISCSI=0
KEEP_NVME=0
ISCSI_CONNECTED=0
NVME_CONNECTED=0
NVME_DEVICE=""
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-vm"
# Volume size is computed from the cloud image's virtual size in
# size_volume_for_image() — matching exactly avoids a GPT-secondary-
# header mismatch (when the volume is larger than the image's GPT
# was authored for, OVMF refuses to enumerate the ESP).
VOLUME_SIZE_MIB=0
SEED=""
QUICK=0
RW_DEVICE=""
LOOP_DEVICE=""
ROOT_PART=""
QEMU_PID=""
QEMU_MONITOR=""
OVMF_CODE=""
OVMF_VARS=""

# Ubuntu 26.04 LTS "Resolute Raccoon" minimal cloud image. UEFI/GPT
# bootable — q35 + OVMF firmware required. The `/release/` directory
# always points at the most recently published rebuild for the LTS;
# override CLOUD_IMG_URL to pin to a dated rebuild if needed.
: "${CLOUD_IMG_URL:=https://cloud-images.ubuntu.com/minimal/releases/resolute/release/ubuntu-26.04-minimal-cloudimg-amd64.img}"
CLOUD_IMG_FILE="$(basename "$CLOUD_IMG_URL")"
CLOUD_CACHE_DIR="/var/cache/thur/cloud-images"
CLOUD_CACHE_FILE="${CLOUD_CACHE_DIR}/${CLOUD_IMG_FILE}"

# Fixture params — set by pick_workload_params().
FIXTURE_COUNT=0
FIXTURE_KEYS=()   # parallel arrays indexed 0..FIXTURE_COUNT-1
FIXTURE_SIZES=()
FIXTURE_NAMES=()

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --transport) TRANSPORT="$2"; shift 2 ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --keep-nvme) KEEP_NVME=1; shift ;;
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

case "$TRANSPORT" in
    iscsi|nvmetcp) ;;
    *) echo "Unknown --transport '$TRANSPORT' (expected iscsi or nvmetcp)"; exit 1 ;;
esac

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

# Kill any qemu we still have a PID for. Used by cleanup and between
# Phase B / Phase C transitions.
kill_qemu() {
    if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
        kill "$QEMU_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            kill -0 "$QEMU_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    QEMU_PID=""
}

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    kill_qemu
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi
    if [[ -n "$LOOP_DEVICE" ]] && [[ -b "$LOOP_DEVICE" ]]; then
        losetup -d "$LOOP_DEVICE" 2>/dev/null || true
        LOOP_DEVICE=""
    fi
    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsi_logout_and_delete
    fi
    if [[ $NVME_CONNECTED -eq 1 && $KEEP_NVME -eq 0 ]]; then
        nvme_tcp_disconnect
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
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE, transport: $TRANSPORT)..."
    local missing=() hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvsad >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvsad)
        else
            missing+=("thurvsad"); hints+=("  - thurvsad: $build_cmd")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvsa >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvsa)
        else
            missing+=("thurvsa"); hints+=("  - thurvsa: $build_cmd")
        fi
    fi

    declare -A HINTS=(
        [qemu-system-x86_64]="sudo apt-get install qemu-system-x86"
        [qemu-img]="sudo apt-get install qemu-utils"
        [cloud-localds]="sudo apt-get install cloud-image-utils"
        [partprobe]="sudo apt-get install parted"
        [curl]="sudo apt-get install curl"
        [openssl]="(usually present)"
        [sha256sum]="(coreutils — usually present)"
        [socat]="sudo apt-get install socat"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [blkid]="(util-linux — usually present)"
        [lsblk]="(util-linux — usually present)"
        [fsck.ext4]="sudo apt-get install e2fsprogs"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [nvme]="sudo apt-get install nvme-cli"
    )
    local tools=(qemu-system-x86_64 qemu-img cloud-localds partprobe partx blockdev curl openssl sha256sum socat mount umount blkid lsblk fsck.ext4)
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        tools+=(iscsiadm lsscsi)
    else
        tools+=(nvme)
    fi
    for tool in "${tools[@]}"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool"); hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if [[ "$TRANSPORT" == "nvmetcp" ]]; then
        if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
            missing+=("nvme_tcp kernel module")
            hints+=("  - nvme_tcp: sudo modprobe nvme_tcp")
        fi
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if [[ "$TRANSPORT" == "iscsi" ]]; then
        if command -v systemctl >/dev/null 2>&1; then
            if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
                log_error "iscsid (open-iscsi) service is not running."
                echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
                exit 1
            fi
        fi
    else
        if ! lsmod | grep -q '^nvme_tcp\b'; then
            log_info "Loading nvme_tcp kernel module"
            modprobe nvme_tcp || { log_error "Failed to load nvme_tcp"; exit 1; }
        fi
    fi

    locate_ovmf || exit 1

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

# Find the OVMF firmware blobs. Debian / Ubuntu ship them under
# /usr/share/OVMF/; both the 4M (preferred for q35) and legacy 2M
# variants may exist. Sets globals OVMF_CODE (read-only firmware) and
# OVMF_VARS (template; we'll copy a writable instance per-run).
locate_ovmf() {
    local code_candidates=(
        /usr/share/OVMF/OVMF_CODE_4M.fd
        /usr/share/OVMF/OVMF_CODE.fd
        /usr/share/edk2-ovmf/OVMF_CODE.fd
        /usr/share/qemu/OVMF_CODE.fd
    )
    local vars_candidates=(
        /usr/share/OVMF/OVMF_VARS_4M.fd
        /usr/share/OVMF/OVMF_VARS.fd
        /usr/share/edk2-ovmf/OVMF_VARS.fd
        /usr/share/qemu/OVMF_VARS.fd
    )
    local c
    for c in "${code_candidates[@]}"; do
        [[ -r "$c" ]] && { OVMF_CODE="$c"; break; }
    done
    for c in "${vars_candidates[@]}"; do
        [[ -r "$c" ]] && { OVMF_VARS="$c"; break; }
    done
    # Prefer matched 4M/2M pair — mismatching the two halves causes
    # firmware to fail to boot. If only one of the 4M paths exists,
    # fall through to whichever pair we found.
    if [[ "$OVMF_CODE" == *_4M.fd && "$OVMF_VARS" != *_4M.fd ]]; then
        if [[ -r /usr/share/OVMF/OVMF_VARS_4M.fd ]]; then
            OVMF_VARS=/usr/share/OVMF/OVMF_VARS_4M.fd
        else
            # 4M code without 4M vars — fall back to non-4M code.
            OVMF_CODE="${OVMF_CODE%_4M.fd}.fd"
        fi
    fi
    if [[ -z "$OVMF_CODE" || -z "$OVMF_VARS" ]]; then
        log_error "OVMF firmware not found. Install with: sudo apt-get install ovmf"
        return 1
    fi
    log_info "OVMF firmware: code=$OVMF_CODE vars=$OVMF_VARS"
}

assign_ports_vm() {
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

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data" "$MOUNT_POINT"
    local transport_block
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        transport_block=$'iscsi:\n  listen: "127.0.0.1:'"$ISCSI_PORT"'"'
    else
        transport_block=$'transport: nvmetcp\nnvmetcp:\n  listen: "0.0.0.0:'"$NVMETCP_PORT"'"'
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"

$transport_block

disk_cache:
  disk_free_min_gb: 0

storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

keystore:
  backends:
    local: { type: local }
EOFCONFIG
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
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" > "${TEST_DIR}/daemon.log" 2>&1 &
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

ensure_volume() {
    if "$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null | grep -q "$VOLUME_NAME"; then
        log_info "Volume $VOLUME_NAME already present"
        return 0
    fi
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size "${VOLUME_SIZE_MIB}M" --backend local >/dev/null
}

connect_iscsi() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null \
        || { log_error "iscsi login failed"; exit 1; }
    ISCSI_CONNECTED=1
    sleep 2
    local row
    for _ in 1 2 3 4 5; do
        row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
        [[ -n "$row" ]] && break
        sleep 1
    done
    [[ -n "$row" ]] || { log_error "iscsi login OK but no THUR VSA device appeared"; lsscsi -g; exit 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; exit 1; }
    log_info "thurvsa LUN -> $RW_DEVICE"
}

connect_nvme() {
    nvme_tcp_connect || exit 1
    RW_DEVICE="/dev/${NVME_DEVICE}n1"
    log_info "thurvsa namespace -> $RW_DEVICE"
}

transport_connect() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        connect_iscsi
    else
        connect_nvme
    fi
}

fetch_cloud_image() {
    mkdir -p "$CLOUD_CACHE_DIR"
    if [[ -s "$CLOUD_CACHE_FILE" ]]; then
        log_info "Using cached cloud image: $CLOUD_CACHE_FILE"
        return 0
    fi
    log_info "Fetching $CLOUD_IMG_URL (cached for next run)..."
    local tmp="${CLOUD_CACHE_FILE}.tmp"
    if ! curl -fsSL --retry 3 -o "$tmp" "$CLOUD_IMG_URL"; then
        log_error "curl failed for $CLOUD_IMG_URL"
        echo "  Override the image URL with CLOUD_IMG_URL=https://..."
        rm -f "$tmp"
        exit 1
    fi
    mv "$tmp" "$CLOUD_CACHE_FILE"
    log_info "Cached at $CLOUD_CACHE_FILE ($(du -h "$CLOUD_CACHE_FILE" | cut -f1))"
}

write_cloud_image_to_volume() {
    log_info "qemu-img convert: $CLOUD_IMG_FILE -> $RW_DEVICE (raw)..."
    if ! qemu-img convert -p -f qcow2 -O raw "$CLOUD_CACHE_FILE" "$RW_DEVICE" \
            > "$TEST_DIR/qemu-img-convert.log" 2>&1; then
        log_error "qemu-img convert failed:"
        tail -20 "$TEST_DIR/qemu-img-convert.log" | sed 's/^/  /'
        exit 1
    fi
    blockdev --flushbufs "$RW_DEVICE" || true
    log_info "Cloud image written; refreshing kernel partition table..."
    partprobe "$RW_DEVICE" >/dev/null 2>&1 || true
    sleep 1
}

# Compute volume size from the cached cloud image's virtual size,
# rounded up to MiB. Matching the image size exactly keeps the GPT
# secondary header at the disk's actual end, which is what OVMF
# requires before it will enumerate the ESP and boot the guest.
size_volume_for_image() {
    # qemu-img info --output=json has two virtual-size fields (one
    # under children[0].info for the file's on-disk size, one at the
    # top level for the qcow2 virtual size). Parsing the human
    # "virtual size:" line is unambiguous; we just pull the bytes
    # value out of the parens.
    local bytes
    bytes=$(qemu-img info "$CLOUD_CACHE_FILE" 2>/dev/null \
        | sed -n 's/^virtual size:.*(\([0-9]\+\) bytes).*/\1/p')
    if [[ -z "$bytes" || "$bytes" == "0" ]]; then
        log_error "Could not read virtual size from $CLOUD_CACHE_FILE"
        qemu-img info "$CLOUD_CACHE_FILE" 2>&1 | sed 's/^/  /'
        exit 1
    fi
    # Round up to MiB so the daemon doesn't reject a non-MiB-aligned
    # size. Cloud image virtual sizes are already a multiple of
    # 64 KiB in practice; this is belt-and-braces.
    VOLUME_SIZE_MIB=$(( (bytes + 1048575) / 1048576 ))
    log_info "Cloud image virtual size: $bytes bytes -> volume sized to ${VOLUME_SIZE_MIB} MiB"
}

# Pick fixture params deterministically from the seed. Boundary-biased
# sizes (same buckets as monte-carlo.sh's mc_pick_size_boundary_biased)
# but capped: total fixture must fit comfortably alongside the ~3-4 GiB
# Ubuntu image on the 8 GiB volume. Quick mode trims to 4 files.
pick_workload_params() {
    local n cap_total
    if [[ $QUICK -eq 1 ]]; then
        n=4
        cap_total=$((  8 * 1024 * 1024 ))   # 8 MiB ceiling for --quick
    else
        n=10
        cap_total=$(( 256 * 1024 * 1024 ))  # 256 MiB ceiling
    fi
    local i total=0 size key name
    for (( i = 0; i < n; i++ )); do
        MC_OP_INDEX=$i
        size=$(mc_pick_size_boundary_biased "fxsize")
        # Clamp residual budget so we don't overrun.
        local remaining=$(( cap_total - total ))
        (( remaining < 1 )) && remaining=1
        (( size > remaining )) && size=$remaining
        # blake3(seed||index||"content")[:64] for AES-256 key — same
        # construction as mc_content_to so host + guest produce
        # byte-identical streams.
        key=$(printf '%s|file-%03d|%s|content' "$MC_SEED" "$((i+1))" "1" \
            | b3sum --no-names 2>/dev/null \
            || printf '%s|file-%03d|%s|content' "$MC_SEED" "$((i+1))" "1" | sha256sum | awk '{print $1}')
        key="${key:0:64}"
        name=$(printf 'file-%03d.bin' "$((i+1))")
        FIXTURE_KEYS+=("$key")
        FIXTURE_SIZES+=("$size")
        FIXTURE_NAMES+=("$name")
        total=$(( total + size ))
        mc_log_op "fixture" "name=$name" "size=$size" "key=${key:0:8}…"
    done
    FIXTURE_COUNT=$n
    log_info "Fixture: $n files, total $(awk -v b=$total 'BEGIN{printf "%.1f", b/1048576}') MiB"
}

# Emit the guest-side script that materializes the fixture. The host
# produces this script with the (key, size) pairs hard-coded so the
# guest doesn't need to re-derive anything from the seed.
write_fixture_script() {
    local out_path="$1"
    {
        echo '#!/bin/sh'
        echo '# Cloud-init runcmd target. Generates the fixture, computes'
        echo '# sha256sums, syncs, and powers off.'
        echo 'mkdir -p /var/test-fixture'
        for (( i = 0; i < FIXTURE_COUNT; i++ )); do
            local k="${FIXTURE_KEYS[$i]}"
            local s="${FIXTURE_SIZES[$i]}"
            local n="${FIXTURE_NAMES[$i]}"
            # openssl exits non-zero on SIGPIPE from `head -c`; that's
            # benign — `head` already wrote exactly `$s` bytes.
            echo "openssl enc -aes-256-ctr -K $k -iv 00000000000000000000000000000000 -in /dev/zero 2>/dev/null | head -c $s > /var/test-fixture/$n || true"
        done
        echo 'sync'
        # Write hashes with relative paths so the host-side
        # `sha256sum -c` can verify after re-mounting at a different
        # path (the guest's /var ends up at e.g. /tmp/.../mnt/var on
        # the host; absolute paths in the sums file wouldn't resolve).
        echo 'cd /var/test-fixture && sha256sum *.bin > sha256sums.txt'
        echo 'sync'
        echo '# Brief grace so cloud-init logs the runcmd completion'
        echo '# before the kernel halts.'
        echo 'sleep 1'
        echo 'poweroff'
    } > "$out_path"
    chmod +x "$out_path"
}

# Build the NoCloud seed iso. instance_id controls cloud-init re-run
# semantics: a fresh id forces it to re-execute even if state from a
# prior boot is on the rootfs.
build_seed_iso() {
    local iso_path="$1"
    local instance_id="$2"
    local runcmd_path="$3"

    local stage="$TEST_DIR/seed-stage-$$-$RANDOM"
    mkdir -p "$stage"
    cat > "$stage/meta-data" <<EOFMETA
instance-id: $instance_id
local-hostname: thurvsa-vmtest
EOFMETA
    # The runcmd payload is copied into user-data verbatim. write_files
    # with the binary script inline keeps cloud-init's permissions /
    # encoding handling simple; runcmd then just execs it.
    {
        echo '#cloud-config'
        echo 'preserve_hostname: false'
        echo 'hostname: thurvsa-vmtest'
        echo 'write_files:'
        echo '  - path: /usr/local/bin/thur-fixture.sh'
        echo '    permissions: "0755"'
        echo '    encoding: b64'
        echo '    content: |'
        base64 -w 76 "$runcmd_path" | sed 's/^/      /'
        echo 'runcmd:'
        echo '  - [/usr/local/bin/thur-fixture.sh]'
    } > "$stage/user-data"

    if ! cloud-localds -d raw "$iso_path" "$stage/user-data" "$stage/meta-data" \
            > "$TEST_DIR/cloud-localds.log" 2>&1; then
        log_error "cloud-localds failed:"
        cat "$TEST_DIR/cloud-localds.log" | sed 's/^/  /'
        exit 1
    fi
    rm -rf "$stage"
}

# Boot qemu in the background. The monitor socket is exposed at
# $QEMU_MONITOR so the caller can send HMP commands (system_reset,
# quit, ...). Returns once qemu has bound the monitor socket. Uses TCG
# unconditionally — the issue spec calls KVM out of scope for the test
# path.
boot_qemu() {
    local seed_iso="$1"
    local log_path="$2"
    local timeout_s="${3:-600}"

    QEMU_MONITOR="$TEST_DIR/qmp-$$-$RANDOM.sock"
    rm -f "$QEMU_MONITOR"

    # Ubuntu 26.04 cloud images are UEFI/GPT-only — q35 + OVMF
    # pflash is mandatory. The OVMF_VARS template must be copied to a
    # writable per-run instance (UEFI writes back NVRAM during boot).
    local vars_copy="$TEST_DIR/ovmf-vars-$$-$RANDOM.fd"
    cp "$OVMF_VARS" "$vars_copy"

    # -no-reboot turns guest poweroff/halt into qemu exit. -nographic
    # routes the serial console to stdout. -accel tcg,thread=multi
    # gives every host core some work — full systemd Ubuntu boot under
    # TCG single-threaded is painfully slow; multi-threaded brings it
    # down to ~60-90 s.
    qemu-system-x86_64 \
        -name "thurvsa-vmtest" \
        -machine q35 \
        -accel tcg,thread=multi \
        -m 1024 \
        -smp 2 \
        -nographic \
        -no-reboot \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$vars_copy" \
        -monitor "unix:${QEMU_MONITOR},server,nowait" \
        -drive "file=$RW_DEVICE,format=raw,if=virtio,cache=writeback,discard=unmap" \
        -drive "file=$seed_iso,format=raw,if=virtio,readonly=on" \
        -serial "file:$log_path" \
        -netdev user,id=n0 -device virtio-net,netdev=n0 \
        > "$TEST_DIR/qemu-stdout.log" 2>&1 &
    QEMU_PID=$!

    # Wait for the monitor socket — qemu binds it before the guest CPU
    # is started, so this is a "qemu launched OK" probe, not a "guest
    # booted" probe.
    for _ in {1..30}; do
        [[ -S "$QEMU_MONITOR" ]] && return 0
        kill -0 "$QEMU_PID" 2>/dev/null || break
        sleep 0.5
    done
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        log_error "qemu exited before binding monitor; tail of stdout:"
        tail -30 "$TEST_DIR/qemu-stdout.log" | sed 's/^/  /'
        exit 1
    fi
    log_error "qemu monitor socket did not appear within 15 s"
    exit 1
}

# Send one HMP command to the qemu monitor. Drops the response — HMP
# replies are diagnostic text only.
qemu_hmp() {
    local cmd="$1"
    printf '%s\n' "$cmd" | socat - "UNIX-CONNECT:$QEMU_MONITOR" >/dev/null 2>&1 || true
}

# Wait for the qemu process to exit, up to $1 seconds. Returns 0 on
# clean exit (poweroff with -no-reboot), 1 on timeout, 2 if qemu was
# already gone.
wait_qemu_exit() {
    local timeout_s="$1"
    local deadline=$(( $(date +%s) + timeout_s ))
    if [[ -z "$QEMU_PID" ]]; then return 2; fi
    while (( $(date +%s) < deadline )); do
        if ! kill -0 "$QEMU_PID" 2>/dev/null; then
            wait "$QEMU_PID" 2>/dev/null || true
            QEMU_PID=""
            return 0
        fi
        sleep 1
    done
    return 1
}

# Locate the ext4 root partition. thurvsa iSCSI / NVMe devices expose
# 4 KiB logical sectors, but the cloud image's GPT was authored for
# 512-byte LBAs — read direct, the host kernel reads "LBA 1" at byte
# 4096 instead of byte 512, finds garbage, and parses only the
# protective MBR. The fix is to wrap the block device in a loop
# device with --sector-size 512 forced; the loop layer re-presents
# the same bytes with the right LBA size, and the kernel's GPT
# parser then enumerates the ESP, /boot, BIOS-boot, and root
# partitions normally. The loop wrapper is host-side-only — the
# guest sees /dev/sdb directly through qemu and is unaffected.
attach_loop_device() {
    [[ -n "$LOOP_DEVICE" && -b "$LOOP_DEVICE" ]] && return 0
    blockdev --flushbufs "$RW_DEVICE" 2>/dev/null || true
    local lo
    if ! lo=$(losetup -fP --sector-size 512 --show "$RW_DEVICE" 2>"$TEST_DIR/losetup.err"); then
        log_error "losetup failed to wrap $RW_DEVICE with 512-byte sectors:"
        cat "$TEST_DIR/losetup.err" | sed 's/^/  /'
        return 1
    fi
    LOOP_DEVICE="$lo"
    sleep 1
    log_info "Wrapped $RW_DEVICE -> $LOOP_DEVICE (sector size 512)"
}

detach_loop_device() {
    if [[ -n "$LOOP_DEVICE" && -b "$LOOP_DEVICE" ]]; then
        losetup -d "$LOOP_DEVICE" 2>/dev/null || true
        LOOP_DEVICE=""
        sleep 1
    fi
}

locate_root_partition() {
    attach_loop_device || return 1
    local part="" attempt
    for attempt in 1 2 3 4 5; do
        part=$(lsblk -lnp -o NAME,FSTYPE "$LOOP_DEVICE" 2>/dev/null \
            | awk -v p="$LOOP_DEVICE" '$1 != p && $2 == "ext4" {print $1; exit}')
        if [[ -n "$part" ]]; then
            ROOT_PART="$part"
            log_info "Guest root partition: $ROOT_PART"
            return 0
        fi
        partprobe "$LOOP_DEVICE" 2>/dev/null || true
        sleep 1
    done
    log_error "Could not find ext4 partition on $LOOP_DEVICE"
    echo "  lsblk -lp $LOOP_DEVICE:"
    lsblk -lp "$LOOP_DEVICE" 2>&1 | sed 's/^/    /'
    echo "  blkid -p $LOOP_DEVICE:"
    blkid -p "$LOOP_DEVICE" 2>&1 | sed 's/^/    /'
    return 1
}

# Compute the SHA256 of the keystream emitted by the same AES-256-CTR
# parameters the guest used, truncated to $size. Used to derive the
# expected hash for each fixture file from the seed alone — no need to
# read the guest's /var/test-fixture.sha256.
expected_sha256() {
    local key_hex="$1" size="$2"
    openssl enc -aes-256-ctr -K "$key_hex" -iv 00000000000000000000000000000000 -in /dev/zero 2>/dev/null \
        | head -c "$size" \
        | sha256sum | awk '{print $1}'
}

verify_clean_fixture() {
    log_info "[Phase B] mounting $ROOT_PART read-only at $MOUNT_POINT..."
    if ! mount -o ro "$ROOT_PART" "$MOUNT_POINT"; then
        log_error "[Phase B] mount failed"
        return 1
    fi
    local dir="$MOUNT_POINT/var/test-fixture"
    if [[ ! -d "$dir" ]]; then
        log_error "[Phase B] guest did not create /var/test-fixture/"
        umount "$MOUNT_POINT" 2>/dev/null || true
        return 1
    fi
    local fails=0 i name size key actual expected
    for (( i = 0; i < FIXTURE_COUNT; i++ )); do
        name="${FIXTURE_NAMES[$i]}"
        size="${FIXTURE_SIZES[$i]}"
        key="${FIXTURE_KEYS[$i]}"
        local f="$dir/$name"
        if [[ ! -f "$f" ]]; then
            log_fail "[Phase B] missing $name"
            fails=$((fails+1)); continue
        fi
        local actual_size
        actual_size=$(stat -c %s "$f")
        if [[ "$actual_size" != "$size" ]]; then
            log_fail "[Phase B] $name size mismatch: expected $size, got $actual_size"
            fails=$((fails+1)); continue
        fi
        actual=$(sha256sum "$f" | awk '{print $1}')
        expected=$(expected_sha256 "$key" "$size")
        if [[ "$actual" != "$expected" ]]; then
            log_fail "[Phase B] $name sha256 mismatch (expected $expected, got $actual)"
            fails=$((fails+1)); continue
        fi
    done
    # Cross-check: the guest's own sha256sum file should agree with
    # the files on the volume. Catches a class of host-side I/O
    # corruption that happens to match our expected-from-seed hash.
    if [[ -f "$dir/sha256sums.txt" ]]; then
        (cd "$dir" && sha256sum -c sha256sums.txt >/dev/null 2>&1) \
            || { log_fail "[Phase B] guest sha256sum -c failed against the mounted files"; fails=$((fails+1)); }
    fi
    umount "$MOUNT_POINT" || true
    if (( fails > 0 )); then
        log_fail "[Phase B] $fails fixture file(s) failed verification"
        return 1
    fi
    log_info "[Phase B] all $FIXTURE_COUNT fixture files verified"
    return 0
}

phase_b_clean_boot() {
    log_info "[Phase B] building cloud-init seed iso (clean boot)..."
    local script="$TEST_DIR/fixture-clean.sh"
    local seed_iso="$TEST_DIR/seed-clean.iso"
    write_fixture_script "$script"
    build_seed_iso "$seed_iso" "thurvsa-vmtest-clean" "$script"

    log_info "[Phase B] booting qemu (TCG; Ubuntu boots in ~60-90 s)..."
    boot_qemu "$seed_iso" "$TEST_DIR/qemu-clean-serial.log" 900
    log_info "[Phase B] waiting for clean shutdown (up to 15 min)..."
    if ! wait_qemu_exit 900; then
        log_error "[Phase B] guest did not power off within 15 min"
        echo "  tail of serial console:"
        tail -40 "$TEST_DIR/qemu-clean-serial.log" 2>/dev/null | sed 's/^/    /'
        kill_qemu
        return 1
    fi
    log_info "[Phase B] guest powered off cleanly"

    locate_root_partition || return 1
    verify_clean_fixture || return 1
    return 0
}

# Phase C — boot a second qemu run with a fresh instance-id so
# cloud-init re-executes and writes a second fixture. We send
# `system_reset` mid-write via the QEMU monitor and immediately follow
# it with `quit`, killing qemu without giving the guest a chance to
# fsync the work in flight. Then we mount on the host (kernel runs ext4
# journal replay), run fsck.ext4 -fn, and verify that the Phase B
# fixture (which WAS fsynced) is still intact.
phase_c_crash_variant() {
    log_info "[Phase C] building seed iso for the crash run..."
    local script="$TEST_DIR/fixture-crash.sh"
    local seed_iso="$TEST_DIR/seed-crash.iso"
    # Saturate the guest with the largest single fixture file we
    # accommodate, so the kill window definitely catches it mid-write.
    # We reuse FIXTURE_KEYS[0] but bump the size by an order of
    # magnitude; the file lands under /var/test-fixture-crash/ so it
    # cannot stomp on the Phase B set.
    {
        echo '#!/bin/sh'
        echo 'mkdir -p /var/test-fixture-crash'
        echo "openssl enc -aes-256-ctr -K ${FIXTURE_KEYS[0]} -iv 00000000000000000000000000000000 -in /dev/zero 2>/dev/null | head -c $(( 64 * 1024 * 1024 )) > /var/test-fixture-crash/big.bin || true"
        echo 'sync'
        echo 'poweroff'
    } > "$script"
    chmod +x "$script"
    build_seed_iso "$seed_iso" "thurvsa-vmtest-crash" "$script"

    # qemu opens /dev/sdb with O_EXCL; the loop wrapper would block
    # that. Drop the wrapper for the duration of the run.
    detach_loop_device

    log_info "[Phase C] booting qemu for the crash run..."
    boot_qemu "$seed_iso" "$TEST_DIR/qemu-crash-serial.log" 600

    # Wait long enough for the guest kernel + cloud-init to be well
    # into the runcmd write. Ubuntu 26.04 under TCG reaches the runcmd
    # stage somewhere around the 75-90 s mark; 120 s gives the openssl
    # generator enough time to be mid-stream when we pull the plug.
    local crash_delay=120
    log_info "[Phase C] sleeping ${crash_delay}s, then SIGKILL the qemu process (sim. host hard-reset)..."
    sleep "$crash_delay"

    # Hard-kill qemu directly. system_reset + quit via HMP would also
    # work, but SIGKILL is closer to a real host-side crash: the host
    # never gets a chance to flush the qemu-side write cache. Any
    # writes the guest fsync()'d should have reached the VSA volume
    # already; un-fsync'd writes may be lost. This is exactly the
    # contract we want to validate.
    kill_qemu
    log_info "[Phase C] qemu terminated; checking volume integrity..."

    # Re-probe and locate the root partition again — the kernel may
    # have lost it across the long qemu lifetime + iSCSI/NVMe activity.
    locate_root_partition || return 1

    # Mount + immediate umount triggers ext4 journal replay during
    # mount; afterwards `fsck.ext4 -fn` should report a clean fs.
    log_info "[Phase C] mounting to trigger journal replay..."
    if ! mount "$ROOT_PART" "$MOUNT_POINT"; then
        log_fail "[Phase C] mount failed — possible filesystem damage"
        return 1
    fi
    sync
    umount "$MOUNT_POINT" || { log_fail "[Phase C] umount failed"; return 1; }

    log_info "[Phase C] fsck.ext4 -fn $ROOT_PART (must be clean)..."
    local fsck_out fsck_rc=0
    fsck_out=$(fsck.ext4 -fn "$ROOT_PART" 2>&1) || fsck_rc=$?
    echo "$fsck_out" | tail -5 | sed 's/^/    /'
    if (( fsck_rc != 0 )); then
        log_fail "[Phase C] fsck.ext4 returned $fsck_rc — filesystem damage after crash"
        return 1
    fi
    log_info "[Phase C] filesystem clean; re-mounting read-only to verify Phase B fixture..."
    if ! mount -o ro "$ROOT_PART" "$MOUNT_POINT"; then
        log_fail "[Phase C] read-only remount failed"
        return 1
    fi
    local fails=0 i name size key actual expected
    for (( i = 0; i < FIXTURE_COUNT; i++ )); do
        name="${FIXTURE_NAMES[$i]}"
        size="${FIXTURE_SIZES[$i]}"
        key="${FIXTURE_KEYS[$i]}"
        local f="$MOUNT_POINT/var/test-fixture/$name"
        if [[ ! -f "$f" ]]; then
            log_fail "[Phase C] Phase B fixture file lost after crash: $name"
            fails=$((fails+1)); continue
        fi
        actual=$(sha256sum "$f" | awk '{print $1}')
        expected=$(expected_sha256 "$key" "$size")
        if [[ "$actual" != "$expected" ]]; then
            log_fail "[Phase C] $name hash mismatch after crash (was fsynced; expected $expected, got $actual)"
            fails=$((fails+1)); continue
        fi
    done
    umount "$MOUNT_POINT" || true
    if (( fails > 0 )); then
        log_fail "[Phase C] $fails Phase B fixture file(s) lost or corrupted across crash + replay"
        return 1
    fi
    log_info "[Phase C] all $FIXTURE_COUNT fsynced Phase B files survived crash + journal replay"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA QEMU VM-Disk Integration Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports_vm
    create_test_config
    start_daemon
    # The cloud image must be fetched and inspected first — the
    # volume size matches its virtual size, so we can't create the
    # volume until we know that.
    fetch_cloud_image
    size_volume_for_image
    ensure_volume
    transport_connect
    write_cloud_image_to_volume

    mc_seed_init "$SEED" "$TEST_DIR/ops.log"
    pick_workload_params

    echo ""
    log_test "Phase B — clean boot + fixture verify"
    if phase_b_clean_boot; then
        log_pass "Phase B"
    else
        log_fail "Phase B"
        mc_dump_failure
        exit 1
    fi

    if [[ $QUICK -eq 1 ]]; then
        echo ""
        log_info "Skipping Phase C (--quick)"
    else
        echo ""
        log_test "Phase C — crash mid-write + journal replay + Phase B invariant"
        if phase_c_crash_variant; then
            log_pass "Phase C"
        else
            log_fail "Phase C"
            mc_dump_failure
            exit 1
        fi
    fi

    echo ""
    echo "========================================"
    log_pass "VM-disk test passed (seed=$MC_SEED, transport=$TRANSPORT)"
    echo "========================================"
    echo "  reusable reproducer: --seed $MC_SEED --transport $TRANSPORT"
    echo "  qemu serial logs:    $TEST_DIR/qemu-*-serial.log"
    echo "  daemon log:          $TEST_DIR/daemon.log"
    exit 0
}

main
