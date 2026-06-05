#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Snapshot + Clone End-to-End Test (issue #13)
#
# Proves copy-on-write snapshots and clones through the real iSCSI
# data path against a live daemon + on-disk chunk pool:
#
#   Phase A — write pattern A to the source volume, SYNC.
#   Phase B — `volume snapshot create src snap1` freezes the page table.
#   Phase C — overwrite the same region with pattern B, SYNC.
#   Phase D — `volume clone src cloneA --from-snapshot snap1` (frozen)
#             and `volume clone src cloneB` (live), then rescan.
#   Phase E — the load-bearing assertions:
#               * read cloneA  == pattern A   (snapshot kept the old data)
#               * read src      == pattern B   (parent diverged)
#               * read cloneB   == pattern B   (live clone took current)
#             This is copy-on-write: one shared chunk pool, three
#             distinct page tables.
#   Phase F — `system gc` must NOT reclaim pattern A's chunks while a
#             snapshot/clone still references them: re-read cloneA after
#             GC and confirm it is still pattern A.
#   Phase G — clone an ENCRYPTED volume (#86): the clone inherits the
#             source crypto identity and decrypts the shared chunks.
#   Phase H — a divergent write to the encrypted clone seals new chunks;
#             the encrypted source stays unchanged.
#   Phase I — destroy the source while the clone lives: the shared DEK
#             sidecar survives and the clone still decrypts (refcount).
#   Phase J — destroy the last family member: the DEK sidecar is gone.
#   Phase K — in-place restore (issue #85): on a fresh volume, write A,
#             snapshot, overwrite B, then `volume snapshot restore`. The
#             volume reads pattern A again (rollback through the real data
#             path), and a GC afterwards keeps it readable.
#
# The op model is transport-agnostic; only the login / device-discovery
# primitives are iSCSI-specific (mirrors test-fs.sh's Phase D note).
#
# Requirements (self-elevates via sudo; NOPASSWD sudoers entry needed):
#   - Root/sudo access
#   - open-iscsi      (sudo apt-get install open-iscsi)
#   - iscsid running  (sudo systemctl enable --now iscsid)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-snapshot.sh [--debug] [--keep-data]
#                                  [--dedup local|global]
#

set -u

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-snapshot-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
ISCSI_CONNECTED=0
DEDUP="local"
VOLUME_SIZE_MIB=16
REGION_BYTES=$((1024 * 1024))   # 1 MiB written/verified region (16 pages)

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --dedup) DEDUP="$2"; shift 2 ;;
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

cleanup() {
    [[ $ISCSI_CONNECTED -eq 1 ]] && iscsi_logout_and_delete
    standard_cleanup
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    require_daemon_binaries thurvsa
    for t in iscsiadm curl dd cmp; do
        command -v "$t" >/dev/null || { log_error "$t required"; exit 1; }
    done
}

start_daemon() {
    assign_ports
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "$TEST_CONFIG" <<EOF
$(yaml_header)

$(yaml_iscsi "$TARGET_IQN")

$(yaml_local_backend)

keystore:
  backends:
    local: { type: local }
EOF
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    RUST_LOG=warn start_thur_daemon
}

cli() { "$CLI_PATH" --config "$TEST_CONFIG" "$@"; }

# LUN assigned to a volume, from `volume info --json`.
get_lun() {
    cli volume info "$1" --json 2>/dev/null \
        | grep -oE '"lun":[[:space:]]*[0-9]+' | grep -oE '[0-9]+' | head -1
}

# A volume's 16-byte uuid (hex), read straight from its on-disk
# manifest. For an origin encrypted volume the uuid is also its crypto
# identity, so the DEK sidecar is <data_dir>/keys/<uuid>.key.
get_uuid() {
    grep -oE '"uuid":[[:space:]]*"[0-9a-f]{32}"' \
        "${TEST_DIR}/data/volumes/$1/manifest.json" 2>/dev/null \
        | grep -oE '[0-9a-f]{32}' | head -1
}

# By-path device node for a LUN; waits up to 10s for the kernel to
# publish it AND for READ CAPACITY to report a non-zero size (a
# freshly-rescanned clone device can exist before it is fully probed).
lun_device() {
    local lun="$1"
    local path="/dev/disk/by-path/ip-127.0.0.1:${ISCSI_PORT}-iscsi-${TARGET_IQN}-lun-${lun}"
    local i sz
    for i in {1..20}; do
        if [[ -e "$path" ]]; then
            sz=$(blockdev --getsize64 "$path" 2>/dev/null || echo 0)
            [[ "${sz:-0}" -gt 0 ]] && { echo "$path"; return 0; }
        fi
        sleep 0.5
    done
    return 1
}

# Write a 1 MiB pattern file to a LUN's first region, O_DIRECT + fsync
# so the bytes reach the daemon and a SYNCHRONIZE CACHE fences them.
write_region() {
    local lun="$1" src="$2" dev
    dev=$(lun_device "$lun") || { log_error "no device for LUN $lun"; return 1; }
    dd if="$src" of="$dev" bs="$REGION_BYTES" count=1 oflag=direct conv=fsync status=none
}

# Read a LUN's first region into a file. Flush the kernel block cache
# first so the read routes to the daemon and reflects its current view.
read_region() {
    local lun="$1" out="$2" dev
    dev=$(lun_device "$lun") || { log_error "no device for LUN $lun"; return 1; }
    blockdev --flushbufs "$dev" 2>/dev/null || true
    sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
    dd if="$dev" of="$out" bs="$REGION_BYTES" count=1 status=none 2>"${TEST_DIR}/dd.err" \
        || { log_error "read LUN $lun: $(cat "${TEST_DIR}/dd.err")"; return 1; }
}

# ---------------------------------------------------------------------

PATTERN_A="${TEST_DIR}/patternA.bin"
PATTERN_B="${TEST_DIR}/patternB.bin"
READBACK="${TEST_DIR}/readback.bin"

test_snapshot_clone_cow() {
    log_test "snapshot + clone copy-on-write through the iSCSI data path (dedup=$DEDUP)"

    head -c "$REGION_BYTES" /dev/urandom > "$PATTERN_A"
    head -c "$REGION_BYTES" /dev/urandom > "$PATTERN_B"

    cli volume create src --size "${VOLUME_SIZE_MIB}M" --dedup "$DEDUP" >/dev/null || {
        log_error "  ✗ create src"; return 1; }
    cli volume create enc --size "${VOLUME_SIZE_MIB}M" --dedup "$DEDUP" \
        --encrypt --keystore local >/dev/null || {
        log_error "  ✗ create encrypted volume"; return 1; }

    iscsi_discover_and_login
    local src_lun; src_lun=$(get_lun src)
    [[ -n "$src_lun" ]] || { log_error "  ✗ no LUN for src"; return 1; }

    # Phase A — pattern A.
    write_region "$src_lun" "$PATTERN_A" || return 1
    log_info "  ✓ Phase A: wrote pattern A to src (LUN $src_lun)"

    # Phase B — snapshot.
    cli volume snapshot create src snap1 >/dev/null || {
        log_error "  ✗ snapshot create"; return 1; }
    cli volume snapshot list src 2>/dev/null | grep -q snap1 || {
        log_error "  ✗ snapshot list missing snap1"; return 1; }
    log_info "  ✓ Phase B: snapshot snap1 created + listed"

    # Phase C — overwrite with pattern B.
    write_region "$src_lun" "$PATTERN_B" || return 1
    log_info "  ✓ Phase C: overwrote src with pattern B"

    # Phase D — clones (from snapshot + from live), then rescan so the
    # new LUNs publish as devices.
    cli volume clone src cloneA --from-snapshot snap1 >/dev/null || {
        log_error "  ✗ clone from snapshot"; return 1; }
    cli volume clone src cloneB >/dev/null || {
        log_error "  ✗ clone from live"; return 1; }
    iscsiadm -m session --rescan >/dev/null 2>&1
    sleep 2
    local clonea_lun cloneb_lun
    clonea_lun=$(get_lun cloneA); cloneb_lun=$(get_lun cloneB)
    [[ -n "$clonea_lun" && -n "$cloneb_lun" ]] || {
        log_error "  ✗ clone LUNs not assigned"; return 1; }
    log_info "  ✓ Phase D: cloneA (LUN $clonea_lun), cloneB (LUN $cloneb_lun)"

    # Phase E — the copy-on-write assertions.
    read_region "$clonea_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ cloneA != pattern A (snapshot did not preserve old data)"; return 1; }
    read_region "$src_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_B" || {
        log_error "  ✗ src != pattern B (parent did not diverge)"; return 1; }
    read_region "$cloneb_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_B" || {
        log_error "  ✗ cloneB != pattern B (live clone wrong)"; return 1; }
    log_info "  ✓ Phase E: cloneA=A, src=B, cloneB=B — copy-on-write divergence confirmed"

    # Phase F — GC must retain chunks a snapshot/clone still references.
    cli system gc >/dev/null 2>&1 || { log_error "  ✗ system gc failed"; return 1; }
    read_region "$clonea_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ cloneA != pattern A after GC (snapshot chunks wrongly reclaimed)"; return 1; }
    curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null || {
        log_error "  ✗ daemon unhealthy after GC"; return 1; }
    log_info "  ✓ Phase F: GC retained snapshot/clone chunks; cloneA still pattern A"

    # Phase G — clone an ENCRYPTED volume (issue #86). The clone inherits
    # the source's crypto identity, so reads of un-diverged pages decrypt
    # against the shared ciphertext chunks.
    local enc_lun; enc_lun=$(get_lun enc)
    [[ -n "$enc_lun" ]] || { log_error "  ✗ no LUN for enc"; return 1; }
    write_region "$enc_lun" "$PATTERN_A" || return 1
    cli volume clone enc encclone >/dev/null || {
        log_error "  ✗ clone encrypted volume (issue #86 regression)"; return 1; }
    iscsiadm -m session --rescan >/dev/null 2>&1
    sleep 2
    local encclone_lun; encclone_lun=$(get_lun encclone)
    [[ -n "$encclone_lun" ]] || { log_error "  ✗ encrypted clone LUN not assigned"; return 1; }
    read_region "$encclone_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ encrypted clone != pattern A (shared chunks did not decrypt)"; return 1; }
    log_info "  ✓ Phase G: encrypted clone reads pattern A (shared encrypted chunks decrypt)"

    # Phase H — a divergent write to the encrypted clone seals NEW
    # encrypted chunks under the shared crypto identity; the source stays
    # pattern A.
    write_region "$encclone_lun" "$PATTERN_B" || return 1
    read_region "$encclone_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_B" || {
        log_error "  ✗ encrypted clone != pattern B after divergent write"; return 1; }
    read_region "$enc_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ encrypted source diverged (should still be pattern A)"; return 1; }
    log_info "  ✓ Phase H: clone diverged to B, encrypted source still A"

    # Phase I — DEK custody is refcounted (issue #86). Destroy the SOURCE
    # while the clone lives: the shared sidecar key must survive and the
    # clone must still decrypt.
    local cryptoid sidecar
    cryptoid=$(get_uuid enc)
    [[ -n "$cryptoid" ]] || { log_error "  ✗ could not read enc crypto identity"; return 1; }
    sidecar="${TEST_DIR}/data/keys/${cryptoid}.key"
    [[ -f "$sidecar" ]] || { log_error "  ✗ expected DEK sidecar $sidecar missing"; return 1; }
    cli volume destroy enc --force >/dev/null || {
        log_error "  ✗ destroy encrypted source"; return 1; }
    [[ -f "$sidecar" ]] || {
        log_error "  ✗ DEK sidecar removed while clone still references it"; return 1; }
    read_region "$encclone_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_B" || {
        log_error "  ✗ clone unreadable after source destroy (DEK stranded)"; return 1; }
    log_info "  ✓ Phase I: source destroyed, shared DEK retained, clone still decrypts"

    # Phase J — destroying the LAST family member forgets the DEK.
    cli volume destroy encclone --force >/dev/null || {
        log_error "  ✗ destroy encrypted clone"; return 1; }
    [[ ! -f "$sidecar" ]] || {
        log_error "  ✗ DEK sidecar survived the last family member ($sidecar)"; return 1; }
    log_info "  ✓ Phase J: last family member destroyed, DEK forgotten"

    # Phase K — in-place restore (issue #85) on a fresh, isolated volume.
    cli volume create rsrc --size "${VOLUME_SIZE_MIB}M" --dedup "$DEDUP" >/dev/null || {
        log_error "  ✗ create rsrc"; return 1; }
    iscsiadm -m session --rescan >/dev/null 2>&1
    sleep 2
    local rsrc_lun; rsrc_lun=$(get_lun rsrc)
    [[ -n "$rsrc_lun" ]] || { log_error "  ✗ no LUN for rsrc"; return 1; }

    # Write A, snapshot it, then overwrite with B (sanity: B reads back).
    write_region "$rsrc_lun" "$PATTERN_A" || return 1
    cli volume snapshot create rsrc rsnap >/dev/null || {
        log_error "  ✗ snapshot create rsnap"; return 1; }
    write_region "$rsrc_lun" "$PATTERN_B" || return 1
    read_region "$rsrc_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_B" || {
        log_error "  ✗ rsrc != pattern B before restore"; return 1; }

    # A restore without --force must refuse (CLI typo-guard).
    if cli volume snapshot restore rsrc rsnap >/dev/null 2>&1; then
        log_error "  ✗ restore without --force should have refused"; return 1
    fi

    # Restore is destructive and sends no "data changed" signal to hosts,
    # so the contract is to quiesce the host first. Model that here: log
    # the session out (== unmount), restore the now-offline volume
    # daemon-side, then reconnect for a cache-fresh read. A buffered read
    # over a still-logged-in device would serve stale page-cache content.
    iscsi_logout_and_delete
    cli volume snapshot restore rsrc rsnap --force >/dev/null || {
        log_error "  ✗ snapshot restore"; return 1; }
    iscsi_discover_and_login
    iscsiadm -m session --rescan >/dev/null 2>&1
    sleep 2

    read_region "$rsrc_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ rsrc != pattern A after restore (rollback failed)"; return 1; }
    log_info "  ✓ Phase K: rsrc rolled back in place to rsnap, reads pattern A"

    # GC after restore: the snapshot still references the restored chunks,
    # so the volume stays readable; the daemon stays healthy.
    cli system gc >/dev/null 2>&1 || { log_error "  ✗ system gc after restore"; return 1; }
    read_region "$rsrc_lun" "$READBACK" || return 1
    cmp -s "$READBACK" "$PATTERN_A" || {
        log_error "  ✗ rsrc != pattern A after post-restore GC"; return 1; }
    curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null || {
        log_error "  ✗ daemon unhealthy after post-restore GC"; return 1; }
    log_info "  ✓ Phase K: post-restore GC kept rsrc readable; daemon healthy"

    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Snapshot + Clone E2E (dedup=$DEDUP)"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1

    if test_snapshot_clone_cow; then
        echo ""
        echo "Total: 1  Passed: 1  Failed: 0"
        exit 0
    else
        echo ""
        echo "Total: 1  Passed: 0  Failed: 1"
        exit 1
    fi
}

main
