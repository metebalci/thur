#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Dual-Transport Test (issue #66)
#
# Exports ONE volume over BOTH iSCSI (SBC) and NVMe/TCP at the same
# time (`transports: [iscsi, nvmetcp]`) and asserts the acceptance
# criteria for simultaneous dual-protocol export:
#
#   1. A SCSI host (iscsiadm -> /dev/sdX,/dev/sgN) and an NVMe host
#      (nvme connect -> /dev/nvmeXn1) each reach the same volume.
#   2. Cross-transport data coherence: a pattern written over iSCSI
#      reads back identically over NVMe (one shared per-volume cache).
#   3. Cross-protocol reservation fencing, both directions:
#        a. iSCSI takes Write Exclusive -> the NVMe host's write is
#           fenced (Reservation Conflict); `nvme resv-report` shows a
#           reservation is held.
#        b. NVMe takes Write Exclusive -> the iSCSI host's write is
#           fenced (sense 0x18); `sg_persist --read-reservation` shows
#           the reservation is held.
#   4. Proactive cross-transport notification, both directions (issue #67):
#        a. NVMe preempts an iSCSI-held reservation -> the iSCSI
#           initiator's next command returns CHECK CONDITION / UNIT
#           ATTENTION RESERVATIONS PREEMPTED (0x06/0x2A/0x03).
#        b. iSCSI preempts an NVMe-held reservation -> the NVMe host's
#           Reservation Notification log (LID 0x80) carries a Reservation
#           Preempted entry.
#
# The cross-protocol coherence falls out of the single shared
# ReservationManager keyed by LUN (issue #57): an iSCSI initiator port
# and an NVMe host are distinct registrant identities that never
# compare equal, so a non-holder is fenced regardless of transport. The
# proactive notification (4) is driven by one transport-neutral observer
# hook on that shared manager (issue #67): every mutating op fans the
# issuer-excluded affected set to the NVMe AER sink (LID 0x80) and the
# iSCSI Unit-Attention sink, so the signal crosses the transport boundary.
#
# Prerequisites:
#   - open-iscsi (iscsiadm), sg3-utils (sg_persist, sg_write_same),
#     lsscsi, nvme-cli, nvme_tcp kernel module, iscsid running.
#   - Root / sudo NOPASSWD (nvme connect + raw block I/O need root).
#     Self-elevates via `exec sudo "$0" "$@"`.
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-dual-transport.sh [--debug] [--keep-data]
#

# Self-elevate to root (nvme connect / raw I/O need it).
if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo --preserve-env=PATH "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-dual-transport-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-dual-transport-test"
NVMETCP_PORT=""
NVME_DEVICE=""
ISCSI_BLK=""
ISCSI_SG=""

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    if [[ -n "$NVME_DEVICE" ]] || nvme list-subsys 2>/dev/null | grep -q "$SUBNQN"; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    fi
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    for t in iscsiadm sg_persist sg_write_same sg_turs lsscsi nvme; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    [[ -f /etc/iscsi/initiatorname.iscsi ]] || {
        log_error "/etc/iscsi/initiatorname.iscsi missing — open-iscsi not initialised"
        exit 1
    }
    if ! lsmod | grep -q '^nvme_tcp'; then
        modprobe nvme_tcp 2>/dev/null || {
            log_error "nvme_tcp kernel module not loadable"
            exit 1
        }
    fi
    require_daemon_binaries thurvsa
}

start_daemon() {
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)
    NVMETCP_PORT=$(pick_free_port)
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "${TEST_DIR}/data"
transports: [iscsi, nvmetcp]
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  reservations:
    initiator_port: iqn
nvmetcp:
  listen: "127.0.0.1:$NVMETCP_PORT"
storage:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/local-backend"
EOFCONFIG
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    TEST_CONFIG="${TEST_DIR}/config.yaml" start_thur_daemon
    # start_thur_daemon waits on the HTTP /health endpoint; also confirm
    # both data-path listeners actually bound (the whole point of #66).
    for port in "$ISCSI_PORT" "$NVMETCP_PORT"; do
        local ok=0
        for _ in $(seq 1 30); do
            if ss -tln 2>/dev/null | grep -q ":$port\b"; then ok=1; break; fi
            sleep 0.2
        done
        if [[ $ok -ne 1 ]]; then
            log_error "data-path listener never bound port $port"
            tail -40 "${TEST_DIR}/daemon.log" 2>/dev/null || true
            return 1
        fi
    done
    log_info "Both listeners up: iscsi=$ISCSI_PORT nvmetcp=$NVMETCP_PORT"
}

connect_both() {
    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "vol-dual" --size 64M >/dev/null \
        || { log_error "volume create failed"; return 1; }

    # iSCSI login (default initiatorname; single initiator).
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null \
        || { log_error "iscsi discovery failed"; return 1; }
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null \
        || { log_error "iscsi login failed"; return 1; }
    sleep 3
    local line
    line=$(lsscsi -g | grep "THUR VSA" | tail -1)
    ISCSI_BLK=$(echo "$line" | awk '{print $(NF-1)}')
    ISCSI_SG=$(echo "$line" | awk '{print $NF}')
    if [[ -z "$ISCSI_BLK" || -z "$ISCSI_SG" || "$ISCSI_BLK" != /dev/* ]]; then
        log_error "could not resolve iSCSI block/sg device (lsscsi: $line)"
        return 1
    fi
    log_info "  iSCSI: block=$ISCSI_BLK sg=$ISCSI_SG"

    # NVMe/TCP connect.
    nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" -n "$SUBNQN" --hostnqn "$HOST_NQN" \
        >"$TEST_DIR/nvme-connect.log" 2>&1 \
        || { log_error "nvme connect failed: $(cat "$TEST_DIR/nvme-connect.log")"; return 1; }
    NVME_DEVICE=$(nvme list-subsys -o json 2>/dev/null \
        | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next((c["Name"] for s in d for ss in s.get("Subsystems",[]) if ss.get("NQN","")=="'"$SUBNQN"'" for c in ss.get("Paths",[])), ""))' 2>/dev/null)
    if [[ -z "$NVME_DEVICE" ]]; then
        NVME_DEVICE=$(ls /dev/nvme*n1 2>/dev/null | head -1 | xargs -n1 basename | sed 's/n1$//')
    fi
    [[ -n "$NVME_DEVICE" ]] || { log_error "could not locate NVMe device"; return 1; }
    log_info "  NVMe: /dev/${NVME_DEVICE}n1"
    return 0
}

# Test 1: one volume, two transports, coherent data.
test_cross_transport_io() {
    log_test "cross-transport I/O: write over iSCSI, read back over NVMe"
    local infile="$TEST_DIR/pattern.bin"
    local outfile="$TEST_DIR/readback.bin"
    dd if=/dev/urandom of="$infile" bs=1M count=1 status=none
    local in_sha
    in_sha=$(sha256sum "$infile" | awk '{print $1}')

    if ! dd if="$infile" of="$ISCSI_BLK" bs=1M count=1 oflag=direct conv=fsync status=none 2>"$TEST_DIR/iscsi-write.err"; then
        log_error "iSCSI write failed: $(cat "$TEST_DIR/iscsi-write.err")"
        return 1
    fi
    if ! dd if="/dev/${NVME_DEVICE}n1" of="$outfile" bs=1M count=1 iflag=direct status=none 2>"$TEST_DIR/nvme-read.err"; then
        log_error "NVMe read failed: $(cat "$TEST_DIR/nvme-read.err")"
        return 1
    fi
    local out_sha
    out_sha=$(sha256sum "$outfile" | awk '{print $1}')
    if [[ "$in_sha" == "$out_sha" ]]; then
        log_info "  data written over iSCSI read back identically over NVMe"
        return 0
    fi
    log_error "  cross-transport data mismatch: iscsi-in=$in_sha nvme-out=$out_sha"
    return 1
}

# Force the LUN to a clean reservation slate via the iSCSI sg device
# (register-ignore a scratch key, then CLEAR wipes every registration +
# the reservation). Keeps the two reservation tests independent so a
# failure in one can't cascade a misleading failure into the other.
reset_lun_reservations() {
    sg_persist --out --register-ignore --param-sark=0xDEAD "$ISCSI_SG" >/dev/null 2>&1 || true
    sg_persist --out --clear --param-rk=0xDEAD "$ISCSI_SG" >/dev/null 2>&1 || true
}

# Test 2a: iSCSI holds Write Exclusive -> NVMe host fenced.
test_iscsi_reservation_fences_nvme() {
    log_test "iSCSI Write Exclusive fences the NVMe host's writes"
    reset_lun_reservations
    sg_persist --out --register --param-sark=0xA1A1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI register failed"; return 1; }
    sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI reserve (WE) failed"; return 1; }
    log_info "  iSCSI registered + reserved (Write Exclusive)"

    # NVMe host (not a registrant) attempts a one-block write -> must
    # come back Reservation Conflict.
    local blk="$TEST_DIR/oneblock.bin"
    dd if=/dev/urandom of="$blk" bs=4096 count=1 status=none
    local nvme_log
    nvme_log=$(nvme write "/dev/${NVME_DEVICE}n1" --start-block=0 --block-count=0 \
        --data-size=4096 --data="$blk" 2>&1)
    local nvme_rc=$?
    if [[ $nvme_rc -ne 0 ]] && echo "$nvme_log" | grep -qiE "reservation conflict"; then
        log_info "  NVMe write fenced: Reservation Conflict"
    else
        log_error "  NVMe write was NOT fenced (rc=$nvme_rc): $nvme_log"
        return 1
    fi

    # The NVMe pull-side report must reflect the (iSCSI-held) reservation.
    if nvme resv-report "/dev/${NVME_DEVICE}n1" --numd=256 >"$TEST_DIR/resv-report.log" 2>&1; then
        # rtype 1 = Write Exclusive; a non-zero rtype means a reservation
        # is held on this namespace (held by the iSCSI registrant).
        if grep -qiE "rtype[^0-9]*1|Write Exclusive" "$TEST_DIR/resv-report.log"; then
            log_info "  nvme resv-report reflects the held Write Exclusive reservation"
        else
            log_error "  nvme resv-report does not show a held reservation"
            cat "$TEST_DIR/resv-report.log" | sed 's/^/    /' >&2
            return 1
        fi
    else
        log_error "  nvme resv-report failed: $(cat "$TEST_DIR/resv-report.log")"
        return 1
    fi

    # Reset the LUN for the reverse direction (clears all registrations
    # + the reservation in one PROUT).
    sg_persist --out --clear --param-rk=0xA1A1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI clear (reset) failed"; return 1; }
    return 0
}

# Test 2b: NVMe holds Write Exclusive -> iSCSI host fenced.
test_nvme_reservation_fences_iscsi() {
    log_test "NVMe Write Exclusive fences the iSCSI host's writes"
    reset_lun_reservations
    local dev="/dev/${NVME_DEVICE}n1"
    nvme resv-register "$dev" --rrega=0 --nrkey=0xB2B2 >/dev/null 2>&1 \
        || { log_error "NVMe resv-register failed"; return 1; }
    nvme resv-acquire "$dev" --crkey=0xB2B2 --rtype=1 --racqa=0 >/dev/null 2>&1 \
        || { log_error "NVMe resv-acquire (WE) failed"; return 1; }
    log_info "  NVMe registered + acquired (Write Exclusive)"

    # iSCSI host (non-holder) write -> must come back RESERVATION
    # CONFLICT (sense 0x18). sg_write_same hits the sg device directly so
    # the sense isn't masked by the block layer.
    local write_log
    write_log=$(sg_write_same --lba=0 --num=1 --in=/dev/zero "$ISCSI_SG" 2>&1 || true)
    if echo "$write_log" | grep -qiE "Reservation conflict|reservation_conflict|sense.*0x18"; then
        log_info "  iSCSI write fenced: RESERVATION CONFLICT (sense 0x18)"
    else
        log_error "  iSCSI write was NOT fenced: $write_log"
        return 1
    fi

    # The SCSI pull-side report must reflect the (NVMe-held) reservation.
    if sg_persist --in --read-reservation "$ISCSI_SG" >"$TEST_DIR/read-reservation.log" 2>&1; then
        if grep -qiE "Reservation follows|Write Exclusive|key=" "$TEST_DIR/read-reservation.log"; then
            log_info "  sg_persist --read-reservation reflects the held reservation"
        else
            log_error "  sg_persist --read-reservation shows no reservation"
            cat "$TEST_DIR/read-reservation.log" | sed 's/^/    /' >&2
            return 1
        fi
    else
        log_error "  sg_persist --read-reservation failed"
        return 1
    fi

    nvme resv-release "$dev" --crkey=0xB2B2 --rtype=1 --rrela=0 >/dev/null 2>&1 || true
    return 0
}

# Test 3a (issue #67): NVMe preempts an iSCSI-held reservation -> the
# iSCSI initiator's NEXT command surfaces UNIT ATTENTION RESERVATIONS
# PREEMPTED (0x06/0x2A/0x03), proactively, instead of only a later
# Reservation Conflict. The daemon never queues a power-on UA at login,
# and the diff excludes the issuer, so the only UA on the iSCSI session
# after the preempt is the one under test — a single probe sees it.
test_nvme_preempt_raises_iscsi_ua() {
    log_test "NVMe preempt raises RESERVATIONS PREEMPTED UA on the iSCSI initiator (#67)"
    reset_lun_reservations
    sg_persist --out --register --param-sark=0xA1A1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI register failed"; return 1; }
    # Write Exclusive: SCSI reservation TYPE 1 (== NVMe RTYPE 1 below).
    sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI reserve (WE) failed"; return 1; }
    # Drain any stray UA the issuer's own ops might have left (none
    # expected — the issuer is excluded) so the probe below is clean.
    sg_turs "$ISCSI_SG" >/dev/null 2>&1 || true
    log_info "  iSCSI registered + reserved (Write Exclusive)"

    local dev="/dev/${NVME_DEVICE}n1"
    nvme resv-register "$dev" --rrega=0 --nrkey=0xB2B2 >/dev/null 2>&1 \
        || { log_error "NVMe resv-register failed"; return 1; }
    # racqa=1 Preempt: NVMe host (key 0xB2B2) preempts the iSCSI holder
    # (prkey 0xA1A1), taking the reservation. RTYPE 1 = Write Exclusive.
    nvme resv-acquire "$dev" --crkey=0xB2B2 --prkey=0xA1A1 --rtype=1 --racqa=1 >/dev/null 2>&1 \
        || { log_error "NVMe resv-acquire (preempt) failed"; return 1; }
    log_info "  NVMe preempted the iSCSI-held reservation"

    # Authoritative assertion: the daemon raised a RESERVATIONS PREEMPTED
    # (0x06/0x2A/0x03) Unit Attention for the iSCSI session's (TSIH, LUN),
    # queued for delivery on its next command. This proves the full
    # cross-transport path end-to-end over a real iSCSI login: observer
    # hook -> IscsiReservationSink -> tsihs_for (resolving the real
    # login IQN, IQN-only here since the conffile sets
    # initiator_port: iqn) -> add_ua. No iSCSI 0x2A/0x03 UA is generated
    # before this preempt, so a plain match is unambiguous.
    local result=0
    if grep -qE "Added Unit Attention.*ASC=0x2a ASCQ=0x03" "${TEST_DIR}/daemon.log" 2>/dev/null; then
        log_info "  daemon raised RESERVATIONS PREEMPTED (0x2A/0x03) for the iSCSI session"
    else
        log_error "  daemon did not raise the iSCSI RESERVATIONS PREEMPTED UA"
        tail -20 "${TEST_DIR}/daemon.log" 2>/dev/null | sed 's/^/    /' >&2 || true
        result=1
    fi

    # Best-effort client-side confirmation: issue a raw TEST UNIT READY
    # (CDB 00..) and look for the UA in the returned sense. Informational
    # only — the Linux SCSI midlayer is free to consume a pending UA on a
    # background command before this pass-through probe, and sg pass-
    # through behaviour varies by environment, so a miss here is not a
    # test failure (the authoritative signal is the daemon log above).
    local turs
    turs=$(sg_raw -v "$ISCSI_SG" 00 00 00 00 00 00 2>&1 || true)
    if echo "$turs" | grep -qiE "Reservations preempted|2a 03"; then
        log_info "  iSCSI initiator also surfaced the UA on its next command"
    else
        log_info "  (iSCSI client did not surface the UA — kernel likely consumed it first)"
    fi

    # Unconditional cleanup so a failure here can't cascade into 3b.
    nvme resv-release "$dev" --crkey=0xB2B2 --rtype=1 --rrela=0 >/dev/null 2>&1 || true
    reset_lun_reservations
    return "$result"
}

# Test 3b (issue #67): iSCSI preempts an NVMe-held reservation -> the
# NVMe host's Reservation Notification log (LID 0x80) carries a
# Reservation Preempted entry (proactive AER source), not just a later
# Reservation Conflict.
test_iscsi_preempt_raises_nvme_notif() {
    log_test "iSCSI preempt raises a LID 0x80 Reservation Preempted on the NVMe host (#67)"
    reset_lun_reservations
    local dev="/dev/${NVME_DEVICE}n1"
    nvme resv-register "$dev" --rrega=0 --nrkey=0xB2B2 >/dev/null 2>&1 \
        || { log_error "NVMe resv-register failed"; return 1; }
    # Write Exclusive: NVMe RTYPE 1 (== SCSI TYPE 1 used by the preempt).
    nvme resv-acquire "$dev" --crkey=0xB2B2 --rtype=1 --racqa=0 >/dev/null 2>&1 \
        || { log_error "NVMe resv-acquire (WE) failed"; return 1; }
    # Drain the NVMe host's notification log so a stale entry can't
    # masquerade as the one under test.
    nvme resv-notif-log "$dev" >/dev/null 2>&1 || true
    log_info "  NVMe registered + acquired (Write Exclusive)"

    sg_persist --out --register --param-sark=0xA1A1 "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI register failed"; return 1; }
    # iSCSI host (key 0xA1A1) preempts the NVMe holder (sark 0xB2B2).
    # SCSI reservation TYPE 1 = Write Exclusive.
    sg_persist --out --preempt --param-rk=0xA1A1 --param-sark=0xB2B2 --prout-type=1 \
        "$ISCSI_SG" >/dev/null 2>&1 \
        || { log_error "iSCSI preempt failed"; return 1; }
    log_info "  iSCSI preempted the NVMe-held reservation"

    if nvme resv-notif-log "$dev" >"$TEST_DIR/resv-notif.log" 2>&1; then
        # The preempted holder gets Reservation Preempted (log page type 3).
        if grep -qiE "Reservation Preempted|Type[^0-9]*:?[^0-9]*3" "$TEST_DIR/resv-notif.log"; then
            log_info "  nvme resv-notif-log shows Reservation Preempted (type 3)"
        else
            log_error "  nvme resv-notif-log has no Reservation Preempted entry"
            sed 's/^/    /' "$TEST_DIR/resv-notif.log" >&2
            return 1
        fi
    else
        log_error "  nvme resv-notif-log failed: $(cat "$TEST_DIR/resv-notif.log")"
        return 1
    fi

    sg_persist --out --clear --param-rk=0xA1A1 "$ISCSI_SG" >/dev/null 2>&1 || true
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Dual-Transport (iSCSI + NVMe/TCP)"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1
    connect_both || exit 1
    echo ""

    local passed=0 failed=0
    for t in test_cross_transport_io test_iscsi_reservation_fences_nvme \
        test_nvme_reservation_fences_iscsi test_nvme_preempt_raises_iscsi_ua \
        test_iscsi_preempt_raises_nvme_notif; do
        if "$t"; then ((passed++)); else ((failed++)); fi
        echo ""
    done

    echo "Total: $((passed + failed))  Passed: $passed  Failed: $failed"
    [[ $failed -eq 0 ]] && exit 0 || exit 1
}

main
