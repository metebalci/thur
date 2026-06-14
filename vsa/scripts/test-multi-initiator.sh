#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Multi-Initiator iSCSI Test
#
# Drives two distinct iSCSI initiators against the same target via
# open-iscsi `iface` records (each iface carries its own
# `iface.initiatorname`), then exercises the persistent-reservation
# (PR) matrix: A registers + reserves, B registers, B's WRITE must
# come back RESERVATION_CONFLICT (sense 0x18) per SPC-4 § 5.9.
#
# What's asserted:
#   1. Two distinct initiator IQNs can each login.
#   2. Initiator A's REGISTER + RESERVE succeeds.
#   3. Initiator B's REGISTER succeeds (no exclusivity yet).
#   4. Initiator B's WRITE comes back with reservation conflict sense.
#
# Prerequisites:
#   - open-iscsi, lsscsi, sg3-utils
#   - iscsid running
#   - Root / sudo NOPASSWD
#
# Why iface and not /etc/iscsi/initiatorname.iscsi swap: open-iscsi
# tracks per-iface `iface.initiatorname` independently of the global
# `initiatorname.iscsi`. Using two ifaces lets us run two SESSIONS
# with distinct I_T nexus IDs without restarting iscsid, which was
# the failure mode of the initial swap-based approach (the kernel
# would re-attach the prior session under the cached InitiatorName,
# so the daemon saw both "initiators" as one I_T nexus and PR
# exclusivity was a no-op).
#
# Usage (invoke from repo root; self-elevates via sudo):
#   ./vsa/scripts/test-multi-initiator.sh [--debug] [--keep-data]
#

if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-multi-init-$$"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
IFACE_A="thurvsa-test-host-a-$$"
IFACE_B="thurvsa-test-host-b-$$"
INIT_A_NAME="iqn.2025-test.com.metebalci:host-a"
INIT_B_NAME="iqn.2025-test.com.metebalci:host-b"

init_common_daemon_args
parse_common_daemon_args "$@"

# Build a per-test iface record bound to a specific InitiatorName.
# open-iscsi stores these under /var/lib/iscsi/ifaces/<name> and
# evaluates `iface.initiatorname` at login, so two sessions opened
# via two ifaces present distinct I_T nexus IDs to the target.
create_iface() {
    local iface="$1" name="$2"
    iscsiadm -m iface -I "$iface" -o new >/dev/null 2>&1 || true
    iscsiadm -m iface -I "$iface" --op update -n iface.initiatorname -v "$name" >/dev/null 2>&1
}

delete_iface() {
    iscsiadm -m iface -I "$1" -o delete >/dev/null 2>&1 || true
}

cleanup() {
    # Per-iface logout (login records are keyed on iface, so the
    # generic --logout would miss them).
    for iface in "$IFACE_A" "$IFACE_B"; do
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --op delete 2>/dev/null || true
        delete_iface "$iface"
    done
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    for t in iscsiadm sg_persist sg_dd lsscsi curl systemctl; do
        if ! command -v "$t" >/dev/null 2>&1; then
            log_error "Missing prerequisite: $t"
            exit 1
        fi
    done
    [[ -f /etc/iscsi/initiatorname.iscsi ]] || {
        log_error "/etc/iscsi/initiatorname.iscsi missing — open-iscsi not initialised"
        exit 1
    }
    require_daemon_binaries thurvsa
}

start_daemon() {
    HTTP_PORT=$(pick_free_port)
    ISCSI_PORT=$(pick_free_port)
    mkdir -p "${TEST_DIR}/data" "${TEST_DIR}/local-backend"
    cat > "${TEST_DIR}/config.yaml" <<EOFCONFIG
data_dir: "${TEST_DIR}/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  # Key reservations by IQN alone (issue #57). open-iscsi mints a fresh
  # ISID on every login, so under the default "iqn-isid" a reconnecting
  # holder would be a new initiator port and couldn't reclaim its own
  # reservation. "iqn" is the right setting for open-iscsi clusters and
  # is what the restart-survival leg below relies on. (The default
  # "iqn-isid" still survives the restart and fences non-holders; it
  # just won't re-match the holder across an ISID change.)
  reservations:
    initiator_port: iqn
storage:
  backends:
    local:
      type: local
      root_dir: "${TEST_DIR}/local-backend"
EOFCONFIG
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    TEST_CONFIG="${TEST_DIR}/config.yaml" start_thur_daemon
}

# Login through a specific iface and return the resulting /dev/sgN
# device for the thurvsa LUN. We disambiguate by snapshotting the
# pre-login sg device set, doing the login, and reading off the new
# entry — `lsscsi` ordering depends on session age and isn't safe
# to filter on alone.
login_via_iface() {
    local iface="$1" before_file="$2" after_file="$3"
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" -I "$iface" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$iface" --login >/dev/null
    sleep 3
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$after_file"
    comm -13 "$before_file" "$after_file" | head -1
}

logout_via_iface() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$1" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" -I "$1" --op delete >/dev/null 2>&1 || true
    sleep 1
}

test_pr_reservation_conflict_between_initiators() {
    log_test "two initiators + PR matrix: B blocked while A holds the reservation"

    "$CLI_PATH" --config "${TEST_DIR}/config.yaml" volume create "vol-mi" --size 16M >/dev/null || return 1

    # Stage two ifaces with distinct iface.initiatorname; login each
    # independently so both sessions are live concurrently.
    create_iface "$IFACE_A" "$INIT_A_NAME"
    create_iface "$IFACE_B" "$INIT_B_NAME"

    local before="${TEST_DIR}/sg-before.txt"
    local after_a="${TEST_DIR}/sg-after-a.txt"
    local after_b="${TEST_DIR}/sg-after-b.txt"
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$before"

    local dev_a dev_b
    dev_a=$(login_via_iface "$IFACE_A" "$before" "$after_a")
    [[ -n "$dev_a" ]] || { log_error "A login: no new THUR VSA device"; lsscsi -g >&2; return 1; }
    log_info "  A=$INIT_A_NAME via $IFACE_A → $dev_a"

    dev_b=$(login_via_iface "$IFACE_B" "$after_a" "$after_b")
    [[ -n "$dev_b" ]] || { log_error "B login: no new THUR VSA device"; lsscsi -g >&2; return 1; }
    log_info "  B=$INIT_B_NAME via $IFACE_B → $dev_b"

    # A registers + takes Write Exclusive (type 1) reservation.
    if ! sg_persist --out --register --param-sark=0xA1A1 "$dev_a" >/dev/null 2>&1; then
        log_error "A register failed"
        return 1
    fi
    if ! sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$dev_a" >/dev/null 2>&1; then
        log_error "A reserve failed"
        return 1
    fi
    log_info "  ✓ A registered + reserved (Write Exclusive)"

    # B registers (allowed even when A holds the reservation).
    if ! sg_persist --out --register --param-sark=0xB2B2 "$dev_b" >/dev/null 2>&1; then
        log_error "B register failed"
        return 1
    fi
    log_info "  ✓ B registered (no exclusivity claim)"

    # Confirm both registrants show up in the daemon's PR table.
    # sg_persist's read-keys output lists each registered key on its
    # own indented line (`    0xa1a1`), not as `Key: <hex>`. We grep
    # the literal hex words rather than parse the layout.
    local pr_keys
    pr_keys=$(sg_persist --in --read-keys "$dev_a" 2>&1 | grep -oE '0x[0-9a-fA-F]+' | tr '\n' ' ')
    if [[ "$pr_keys" != *"0xa1a1"* || "$pr_keys" != *"0xb2b2"* ]]; then
        log_error "PR key table missing one of A/B (got: $pr_keys)"
        return 1
    fi
    log_info "  ✓ PR key table shows both A=0xa1a1 and B=0xb2b2"

    # B's WRITE must return RESERVATION CONFLICT (sense 0x18).
    # Probe via sg_write_same which takes an sg device directly — no
    # /dev/sdX detour through the kernel block layer (which has its
    # own caching and would mask a sense response).
    local write_log
    write_log=$(sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" 2>&1 || true)
    if echo "$write_log" | grep -qiE "Reservation conflict|reservation_conflict|sense.*0x18"; then
        log_info "  ✓ B's WRITE returned RESERVATION CONFLICT"
    else
        log_error "  ✗ B's WRITE did not conflict (daemon may not enforce PR)"
        echo "$write_log" | head -5 | sed 's/^/    /' >&2
        return 1
    fi

    # Cleanup the sessions so a re-run starts clean.
    logout_via_iface "$IFACE_A"
    logout_via_iface "$IFACE_B"
    return 0
}

# PTPL (issue #57): an APTPL=1 reservation must survive a daemon
# restart. A registers WITH APTPL + reserves Write Exclusive, both
# initiators log out, the daemon is stopped and restarted against the
# SAME data dir + config, then both re-login. The reservation must be
# reloaded from <data_dir>/reservations.json: B's write still conflicts
# and READ KEYS still lists A's key — with NO re-registration. A's own
# write succeeding after reconnect proves the holder is re-matched by
# its persisted port identity rather than the per-process TSIH the
# restart reset. The daemon runs in `initiator_port: iqn` mode (see the
# config), so the re-match is by IQN — necessary because open-iscsi
# mints a fresh ISID on every login, so the default `iqn-isid` mode
# would treat the reconnect as a new port (which is spec-correct, just
# not reclaim-friendly for open-iscsi).
test_pr_survives_daemon_restart() {
    log_test "PTPL: APTPL=1 reservation survives a daemon restart (issue #57)"

    # Reuse the single matrix-test volume (LUN 0) so device selection
    # stays unambiguous — a second volume would surface two new sg
    # devices per login and the `head -1` pick would be a coin flip.
    # The matrix test's registrations now persist across logout, so we
    # reset to a clean slate first: REGISTER AND IGNORE a scratch key,
    # then CLEAR, before establishing the APTPL reservation under test.
    create_iface "$IFACE_A" "$INIT_A_NAME"
    create_iface "$IFACE_B" "$INIT_B_NAME"

    local before="${TEST_DIR}/sg-ptpl-before.txt"
    local after_a="${TEST_DIR}/sg-ptpl-after-a.txt"
    local after_b="${TEST_DIR}/sg-ptpl-after-b.txt"
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$before"

    local dev_a dev_b
    dev_a=$(login_via_iface "$IFACE_A" "$before" "$after_a")
    [[ -n "$dev_a" ]] || { log_error "A login: no new device"; return 1; }
    dev_b=$(login_via_iface "$IFACE_B" "$after_a" "$after_b")
    [[ -n "$dev_b" ]] || { log_error "B login: no new device"; return 1; }

    # Clean slate: force A registered with a scratch key (ignoring any
    # surviving matrix-test state), then CLEAR the whole LUN.
    sg_persist --out --register-ignore --param-sark=0xCAFE "$dev_a" >/dev/null 2>&1 \
        || { log_error "A register-ignore (reset) failed"; return 1; }
    sg_persist --out --clear --param-rk=0xCAFE "$dev_a" >/dev/null 2>&1 \
        || { log_error "CLEAR (reset) failed"; return 1; }

    # The CLEAR above unregisters every *other* I_T nexus and, per
    # SPC-4 §6.14.2, leaves a RESERVATIONS PREEMPTED unit attention
    # pending on each. B still carried its key from the matrix test
    # (registrations survive logout under initiator_port: iqn), so B now
    # has that UA queued; A issued the CLEAR and has none. The
    # cross-transport notification that delivers it (issue #67) is
    # correct SCSI behavior — a real initiator just retries past a UA,
    # but sg_persist does not, so drain B's UA with a throwaway PR-IN
    # before B's APTPL register below would otherwise trip on it. (The
    # first read returns the UA and clears it; the rest are clean.)
    for _ in 1 2 3; do sg_persist --in --read-keys "$dev_b" >/dev/null 2>&1 && break; done

    # A registers WITH APTPL (--param-aptpl) then reserves Write
    # Exclusive; B registers (also APTPL) so its key persists too.
    sg_persist --out --register --param-sark=0xA1A1 --param-aptpl "$dev_a" >/dev/null 2>&1 \
        || { log_error "A register (APTPL) failed"; return 1; }
    sg_persist --out --reserve --param-rk=0xA1A1 --prout-type=1 "$dev_a" >/dev/null 2>&1 \
        || { log_error "A reserve failed"; return 1; }
    sg_persist --out --register --param-sark=0xB2B2 --param-aptpl "$dev_b" >/dev/null 2>&1 \
        || { log_error "B register (APTPL) failed"; return 1; }
    log_info "  ✓ A reserved (Write Exclusive, APTPL=1); B registered (APTPL=1)"

    # The on-disk store must exist now.
    if [[ ! -f "${TEST_DIR}/data/reservations.json" ]]; then
        log_error "  ✗ reservations.json not written despite APTPL=1"
        return 1
    fi
    log_info "  ✓ reservations.json present"

    # Log both out and restart the daemon against the same data dir +
    # config (same ports). DAEMON_LOG_MODE=append keeps one log.
    logout_via_iface "$IFACE_A"
    logout_via_iface "$IFACE_B"
    log_info "  restarting daemon..."
    stop_thur_daemon
    DAEMON_LOG_MODE=append TEST_CONFIG="${TEST_DIR}/config.yaml" start_thur_daemon

    # Re-login both initiators. open-iscsi mints a fresh ISID per login,
    # but the daemon runs in `initiator_port: iqn` mode (see the config),
    # so both reconnect as the same IQN-keyed registrants they were
    # before. No re-registration is issued between restart and the checks.
    lsscsi -g | awk '/THUR VSA/{print $NF}' > "$before"
    dev_a=$(login_via_iface "$IFACE_A" "$before" "$after_a")
    [[ -n "$dev_a" ]] || { log_error "A re-login: no device"; return 1; }
    dev_b=$(login_via_iface "$IFACE_B" "$after_a" "$after_b")
    [[ -n "$dev_b" ]] || { log_error "B re-login: no device"; return 1; }
    log_info "  re-logged in: A=$dev_a B=$dev_b"

    # 1. READ KEYS still lists A's key (the registration was reloaded).
    local pr_keys
    pr_keys=$(sg_persist --in --read-keys "$dev_a" 2>&1 | grep -oE '0x[0-9a-fA-F]+' | tr '\n' ' ')
    if [[ "$pr_keys" != *"0xa1a1"* ]]; then
        log_error "  ✗ A's key not present after restart (got: $pr_keys)"
        return 1
    fi
    log_info "  ✓ A's key survived the restart (no re-registration): $pr_keys"

    # 2. B's WRITE still returns RESERVATION CONFLICT.
    local write_log
    write_log=$(sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_b" 2>&1 || true)
    if echo "$write_log" | grep -qiE "Reservation conflict|reservation_conflict|sense.*0x18"; then
        log_info "  ✓ B's WRITE still RESERVATION CONFLICT after restart"
    else
        log_error "  ✗ B's WRITE did not conflict after restart"
        echo "$write_log" | head -5 | sed 's/^/    /' >&2
        return 1
    fi

    # 3. A (the holder, reconnected under the same IQN+ISID) may write —
    # proof the holder is matched by stable port identity, not the
    # restart-reset TSIH. No re-registration was done.
    if sg_write_same --lba=0 --num=1 --in=/dev/zero "$dev_a" >/dev/null 2>&1; then
        log_info "  ✓ A (holder) can still write after restart — re-matched by IQN-keyed port"
    else
        log_error "  ✗ A could not write after restart — holder not re-matched by port identity"
        return 1
    fi

    logout_via_iface "$IFACE_A"
    logout_via_iface "$IFACE_B"
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Multi-Initiator iSCSI"
    echo "========================================"
    echo ""

    check_prerequisites
    mkdir -p "$TEST_DIR"
    start_daemon || exit 1

    local passed=0 failed=0
    if test_pr_reservation_conflict_between_initiators; then
        ((passed++))
    else
        ((failed++))
    fi
    echo ""
    if test_pr_survives_daemon_restart; then
        ((passed++))
    else
        ((failed++))
    fi
    echo ""

    echo "Total: $((passed + failed))  Passed: $passed  Failed: $failed"
    [[ $failed -eq 0 ]] && exit 0 || exit 1
}

main
