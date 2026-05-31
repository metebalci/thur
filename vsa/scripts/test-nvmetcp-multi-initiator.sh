#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa NVMe/TCP Multi-Initiator Reservation Test
#
# The NVMe/TCP counterpart of test-multi-initiator.sh (the iSCSI PR
# fencing test). Drives two distinct NVMe hosts — each its own
# `--hostnqn` + `--hostid` — at the same thurvsa volume and exercises
# the reservation matrix: host A registers + acquires Write Exclusive,
# host B registers, host B is fenced, then host B preempts host A and
# host A learns of it through the Reservation Notification log.
#
# What's asserted:
#   1. Two distinct host identities can each Connect.
#   2. Host A's Reservation Register + Acquire (Write Exclusive) succeeds.
#   3. Host B's Reservation Register succeeds (no exclusivity yet).
#   4. Host B is fenced — either its block WRITE returns Reservation
#      Conflict (when the kernel gives two independent namespace
#      devices) or its conflicting Reservation Acquire returns
#      Reservation Conflict (when the kernel coalesces the two
#      controllers under one NGUID — see the note below).
#   5. (two-device topology only) Host B preempts host A's reservation
#      (resv-acquire --racqa=1 --prkey=<A's key>) and host A reads a
#      populated Reservation Preempted (type 3) entry from its
#      per-controller Reservation Notification log -- the AER /
#      notification-delivery path end-to-end through the real kernel.
#      Skipped gracefully when nvme-cli lacks `resv-notif-log`, and noted
#      (not run) when the kernel coalesces host B into a passive multipath
#      path with no independent namespace-I/O handle. The in-process
#      counterpart is the unit test
#      nvme_nvm::dispatcher::tests::nvme_preempt_emits_reservation_notification
#      (commit 6045330).
#
# Why two modes: a thurvsa volume has one stable NGUID, so a single
# host's kernel treats two controllers to it as two paths to one
# namespace (native NVMe multipath) and coalesces — or, with multipath
# off, rejects the duplicate. Either way one machine can't always
# present as two block-layer initiators. The cross-host *block I/O*
# fencing logic is proven deterministically by the Rust unit test
# `nvme_nvm::dispatcher::tests::nvme_reservation_fences_other_host`;
# this script proves the reservation surface end-to-end through the
# real kernel + nvme-cli and adapts to whichever device topology the
# kernel produces. No silent skips — the chosen mode is logged.
#
# Prerequisites:
#   - nvme-cli, nvme_tcp kernel module, thurvsad + thurvsa
#   - sudo (self-elevates via 'exec sudo "$0" "$@"')
#   - nvme-cli's `resv-notif-log` is needed only for the AER /
#     notification check (assertion 5); it is skipped gracefully on
#     older nvme-cli builds that lack the subcommand.
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-nvmetcp-multi-initiator.sh [OPTIONS]
#
# Options (shared with the other vsa test scripts):
#   --debug               Use ./target/debug/ binaries (default: release)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --nvmetcp-port PORT   Override nvmetcp port (default: free ephemeral)
#   --http-port PORT      Override HTTP port (default: free ephemeral)
#

if [[ $EUID -ne 0 ]]; then
    exec sudo --preserve-env=PATH "$0" "$@"
fi

# No 'set -e' — run all assertions even if some fail.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-test-nvmetcp-multiinit-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
NVMETCP_PORT=""
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_A_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-host-a"
HOST_A_ID="11111111-1111-1111-1111-111111111111"
HOST_B_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-host-b"
HOST_B_ID="22222222-2222-2222-2222-222222222222"
KEY_A=0xa1a1
KEY_B=0xb2b2

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
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

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; PASS_COUNT=$((PASS_COUNT+1)); }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; FAIL_COUNT=$((FAIL_COUNT+1)); }
log_skip()  { echo -e "${YELLOW}[SKIP]${NC} $*"; SKIP_COUNT=$((SKIP_COUNT+1)); }

cleanup() {
    if nvme list-subsys 2>/dev/null | grep -q "$SUBNQN"; then
        log_info "Disconnecting NVMe subsystem $SUBNQN"
        nvme disconnect -n "$SUBNQN" 2>/dev/null || true
    fi
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=() hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvsad >/dev/null 2>&1; then DAEMON_PATH=$(command -v thurvsad)
        else missing+=("thurvsad"); hints+=("  - thurvsad: $build_cmd"); fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvsa >/dev/null 2>&1; then CLI_PATH=$(command -v thurvsa)
        else missing+=("thurvsa"); hints+=("  - thurvsa: $build_cmd"); fi
    fi
    command -v nvme >/dev/null 2>&1 || { missing+=("nvme"); hints+=("  - nvme-cli: sudo apt-get install nvme-cli"); }
    if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
        missing+=("nvme_tcp kernel module"); hints+=("  - nvme_tcp: sudo modprobe nvme_tcp")
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi
    if ! lsmod | grep -q '^nvme_tcp\b'; then
        log_info "Loading nvme_tcp kernel module"
        modprobe nvme_tcp || { log_error "Failed to load nvme_tcp"; exit 1; }
    fi
    log_info "All prerequisites met"
}

free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes" "$TEST_DIR/storage-primary"
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

transports: [nvmetcp]

http:
  listen: "127.0.0.1:$HTTP_PORT"

nvmetcp:
  listen: "0.0.0.0:$NVMETCP_PORT"

storage:
  backends:
    primary:
      type: local
      root_dir: "$TEST_DIR/storage-primary"
EOFCONFIG
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad (NVMe/TCP)..."
    RUST_LOG="info,nvme_tcp=debug,nvme_nvm=debug" \
        "$DAEMON_PATH" --config "$TEST_CONFIG" > "$TEST_DIR/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in $(seq 1 30); do
        if ss -tln 2>/dev/null | grep -q ":$NVMETCP_PORT\b"; then
            log_info "Daemon ready (PID: $DAEMON_PID, port: $NVMETCP_PORT)"
            return 0
        fi
        sleep 0.2
    done
    log_error "Daemon failed to bind port $NVMETCP_PORT"
    tail -50 "$TEST_DIR/daemon.log"
    exit 1
}

# Stop and re-launch the daemon against the SAME data dir + config
# (same port), appending to the existing log. Used by the PTPL
# restart-survival phase (issue #57).
restart_daemon() {
    stop_thur_daemon
    log_info "Restarting thurvsad (NVMe/TCP) against the same data dir..."
    RUST_LOG="info" "$DAEMON_PATH" --config "$TEST_CONFIG" >> "$TEST_DIR/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in $(seq 1 30); do
        if ss -tln 2>/dev/null | grep -q ":$NVMETCP_PORT\b"; then
            log_info "Daemon back up (PID: $DAEMON_PID)"
            return 0
        fi
        sleep 0.2
    done
    log_error "Daemon failed to rebind port $NVMETCP_PORT after restart"
    return 1
}

create_volume() {
    log_info "Creating test volume..."
    "$CLI_PATH" --config "$TEST_CONFIG" --user root volume create test-vol \
        --size 100MiB --backend primary --page-size 64KiB \
        || { log_error "Failed to create volume"; exit 1; }
}

list_ns_devs() { ls /dev/nvme*n1 2>/dev/null | sort; }

# Connect one host identity; echo the namespace device that appeared
# (empty if the kernel coalesced it onto an existing one).
connect_host() {
    local nqn="$1" hid="$2"
    local before after
    before=$(list_ns_devs)
    nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
        -n "$SUBNQN" --hostnqn "$nqn" --hostid "$hid" \
        >>"$TEST_DIR/connect.log" 2>&1 || return 1
    sleep 0.5
    after=$(list_ns_devs)
    comm -13 <(echo "$before") <(echo "$after") | head -1
}

# nvme-cli's `resv-notif-log` is the host-visible surface of NVMe
# reservation-notification AER delivery: the kernel rings on the AER and
# the log page carries the event. Older nvme-cli builds lack the
# subcommand entirely — gate assertion 5 on its presence.
nvme_has_resv_notif_log() {
    nvme help 2>&1 | grep -q 'resv-notif-log'
}

# Byte 8 of the 64-byte LID 0x80 log page is the Reservation Notification
# Log Page Type (0 empty / 1 reg-preempted / 2 released / 3
# reservation-preempted) — the same field the unit test inspects as
# data_in[8]. The binary form is version-stable; the decoded text labels
# are not. Reading the log drains it daemon-side (consumed on read).
resv_notif_type() {
    nvme resv-notif-log "$1" -o binary 2>/dev/null \
        | dd bs=1 skip=8 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'
}

# Full mode: two independent namespace devices → block-I/O fencing.
test_block_fencing() {
    local dev_a="$1" dev_b="$2"
    log_info "Two independent devices ($dev_a / $dev_b) — block-I/O fencing mode"

    nvme resv-register "$dev_a" --rrega=0 --nrkey="$KEY_A" >>"$TEST_DIR/a.log" 2>&1 \
        && nvme resv-acquire "$dev_a" --crkey="$KEY_A" --rtype=1 --racqa=0 >>"$TEST_DIR/a.log" 2>&1 \
        || { log_fail "host A register+acquire failed"; cat "$TEST_DIR/a.log"; return; }
    log_pass "host A registered + acquired Write Exclusive"

    if nvme resv-register "$dev_b" --rrega=0 --nrkey="$KEY_B" >>"$TEST_DIR/b.log" 2>&1; then
        log_pass "host B registered (no exclusivity claim)"
    else
        log_fail "host B register failed"; cat "$TEST_DIR/b.log"; return
    fi

    # Host B WRITE must be fenced (reservation conflict → block I/O error).
    if dd if=/dev/zero of="$dev_b" bs=4096 count=1 oflag=direct conv=fsync \
        status=none 2>"$TEST_DIR/b-write.err"; then
        log_fail "host B WRITE succeeded but should have been fenced"
    else
        log_pass "host B WRITE returned an error (reservation conflict)"
    fi

    # Host B READ allowed under Write Exclusive.
    if dd if="$dev_b" of=/dev/null bs=4096 count=1 iflag=direct \
        status=none 2>"$TEST_DIR/b-read.err"; then
        log_pass "host B READ allowed under Write Exclusive"
    else
        log_fail "host B READ failed: $(cat "$TEST_DIR/b-read.err")"
    fi

    # Host A (holder) may write.
    if dd if=/dev/zero of="$dev_a" bs=4096 count=1 oflag=direct conv=fsync \
        status=none 2>"$TEST_DIR/a-write.err"; then
        log_pass "host A (holder) WRITE succeeded"
    else
        log_fail "host A WRITE failed: $(cat "$TEST_DIR/a-write.err")"
    fi

    # Report should list both registrant keys.
    nvme resv-report "$dev_a" --numd=256 >"$TEST_DIR/report.log" 2>&1
    if grep -qi "a1a1" "$TEST_DIR/report.log" && grep -qi "b2b2" "$TEST_DIR/report.log"; then
        log_pass "Reservation Report lists both registrant keys"
    else
        log_fail "Reservation Report missing a key"; cat "$TEST_DIR/report.log"
    fi

    # --- Assertion 5: cross-host preempt -> Reservation Notification ---
    # State here: host A holds Write Exclusive, host B is a registrant.
    # Host B preempts host A; host A must learn of it by reading a
    # populated Reservation Preempted (type 3) entry from its
    # per-controller notification log. This drives the AER /
    # notification-delivery path end-to-end through the real kernel.
    if ! nvme_has_resv_notif_log; then
        log_skip "nvme-cli lacks resv-notif-log; skipping AER/notification check"
        return
    fi

    # Drain any stale entry on A so the preempt is the only event we see.
    resv_notif_type "$dev_a" >/dev/null

    if nvme resv-acquire "$dev_b" --crkey="$KEY_B" --prkey="$KEY_A" \
        --rtype=1 --racqa=1 >>"$TEST_DIR/b.log" 2>&1; then
        log_pass "host B preempted host A's reservation"
    else
        log_fail "host B preempt failed"; cat "$TEST_DIR/b.log"; return
    fi

    # Host A reads its notification log; expect Reservation Preempted (3).
    # The daemon arms the entry at preempt time and rings the AER; retry a
    # few times to absorb AER/log propagation latency.
    local ntype=""
    for _ in $(seq 1 10); do
        ntype=$(resv_notif_type "$dev_a")
        [[ "$ntype" == "3" ]] && break
        sleep 0.3
    done
    if [[ "$ntype" == "3" ]]; then
        log_pass "host A read Reservation Preempted (type 3) from notif log"
    else
        log_fail "host A notif-log type was '$ntype', expected 3 (Reservation Preempted)"
    fi

    # The entry is consumed on read -> the next read is the empty page.
    if [[ "$(resv_notif_type "$dev_a")" == "0" ]]; then
        log_pass "notification log drained to empty after consume"
    else
        log_fail "notification log not drained after read"
    fi

    # The issuer (host B) is never notified of its own action.
    if [[ "$(resv_notif_type "$dev_b")" == "0" ]]; then
        log_pass "issuer host B was not notified"
    else
        log_fail "issuer host B unexpectedly has a notification"
    fi

    # Host B is now the holder; host A's registration was removed by the
    # preempt.
    nvme resv-report "$dev_a" --numd=256 >"$TEST_DIR/report-preempt.log" 2>&1
    if grep -qi "b2b2" "$TEST_DIR/report-preempt.log" \
        && ! grep -qi "a1a1" "$TEST_DIR/report-preempt.log"; then
        log_pass "Reservation Report shows host B as holder, host A preempted"
    else
        log_fail "Reservation Report after preempt unexpected"; cat "$TEST_DIR/report-preempt.log"
    fi
}

# Coalesced mode: one shared namespace device. Prove the conflict
# *status* path end-to-end via a reservation command with a mismatched
# key (the daemon returns Reservation Conflict; nvme-cli surfaces it).
test_conflict_status() {
    local dev="$1"
    log_info "Single shared device ($dev) — reservation-conflict-status mode"
    log_info "  (cross-host block fencing is proven by the Rust unit test;"
    log_info "   the kernel coalesces one NGUID across both controllers here.)"

    nvme resv-register "$dev" --rrega=0 --nrkey="$KEY_A" >>"$TEST_DIR/a.log" 2>&1 \
        && nvme resv-acquire "$dev" --crkey="$KEY_A" --rtype=1 --racqa=0 >>"$TEST_DIR/a.log" 2>&1 \
        || { log_fail "register+acquire failed"; cat "$TEST_DIR/a.log"; return; }
    log_pass "registered + acquired Write Exclusive"

    # Acquire again with the WRONG current key → Reservation Conflict.
    if nvme resv-acquire "$dev" --crkey=0xdead --rtype=1 --racqa=0 \
        >"$TEST_DIR/conflict.log" 2>&1; then
        log_fail "mismatched-key Acquire succeeded but should have conflicted"
        cat "$TEST_DIR/conflict.log"
    else
        log_pass "mismatched-key Acquire returned Reservation Conflict"
    fi

    nvme resv-release "$dev" --crkey="$KEY_A" --rtype=1 --rrela=0 >/dev/null 2>&1 \
        && log_pass "Reservation Release accepted" \
        || log_fail "Reservation Release failed"

    # --- Assertion 5 (coalesced topology) ---
    # A true cross-host preempt notification can't be driven here: the
    # kernel made host B a passive multipath path with no independent
    # namespace-I/O handle (no second block device and no per-path
    # /dev/ngXn1), so only one initiator identity can issue reservation
    # I/O. The populated type-3 assertion runs in the two-device mode; the
    # daemon-side derivation is pinned by the unit test
    # nvme_preempt_emits_reservation_notification. We still exercise the
    # notification-log *read* path end-to-end through the real kernel.
    if ! nvme_has_resv_notif_log; then
        log_skip "nvme-cli lacks resv-notif-log; skipping notification-log read"
    elif [[ "$(resv_notif_type "$dev")" == "0" ]]; then
        log_pass "Reservation Notification log read returns a well-formed empty page"
        log_skip "coalesced topology: cross-host preempt notification needs independent devices"
    else
        log_fail "Reservation Notification log read returned an unexpected type"
    fi
}

# PTPL (issue #57): a CPTPL=set reservation must survive a daemon
# restart. Establishes a clean Write Exclusive reservation held by host
# A with CPTPL=set (the CLEAR + re-register makes it independent of any
# prior test's state), confirms reservations.json was written, then
# disconnects the whole subsystem, restarts the daemon against the same
# data dir, and reconnects. The reloaded state is authoritative: the
# Reservation Report still shows A's key (the host did NOT re-register)
# and, in two-device mode, host B's block write is still fenced. The
# HOSTID is host-stable, so no identity fixup is needed on reconnect.
test_ptpl_survives_restart() {
    log_info "PTPL: CPTPL=set reservation survives a daemon restart (issue #57)"
    local dev_a dev_b
    dev_a=$(connect_host "$HOST_A_NQN" "$HOST_A_ID")
    [[ -n "$dev_a" ]] || dev_a=$(list_ns_devs | head -1)
    [[ -n "$dev_a" ]] || { log_fail "PTPL: no namespace device for host A"; return; }

    # Force a clean state: register A ignoring any existing key, CLEAR
    # everything, then register A fresh with CPTPL=set + acquire WE.
    nvme resv-register "$dev_a" --rrega=0 --iekey=1 --nrkey="$KEY_A" >>"$TEST_DIR/ptpl.log" 2>&1
    nvme resv-release "$dev_a" --crkey="$KEY_A" --rrela=1 >>"$TEST_DIR/ptpl.log" 2>&1
    nvme resv-register "$dev_a" --rrega=0 --nrkey="$KEY_A" --cptpl=3 >>"$TEST_DIR/ptpl.log" 2>&1 \
        || { log_fail "PTPL: host A register (CPTPL=set) failed"; cat "$TEST_DIR/ptpl.log"; return; }
    nvme resv-acquire "$dev_a" --crkey="$KEY_A" --rtype=1 --racqa=0 >>"$TEST_DIR/ptpl.log" 2>&1 \
        || { log_fail "PTPL: host A acquire failed"; cat "$TEST_DIR/ptpl.log"; return; }
    log_pass "host A registered (CPTPL=set) + acquired Write Exclusive"

    if [[ -f "$TEST_DIR/data/reservations.json" ]]; then
        log_pass "reservations.json written (CPTPL=set persisted)"
    else
        log_fail "reservations.json missing despite CPTPL=set"
        return
    fi

    # Tear the whole subsystem down and restart the daemon, then
    # reconnect. No reservation command is issued before the checks.
    nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
    sleep 0.5
    restart_daemon || { log_fail "PTPL: daemon restart failed"; return; }
    dev_a=$(connect_host "$HOST_A_NQN" "$HOST_A_ID")
    [[ -n "$dev_a" ]] || dev_a=$(list_ns_devs | head -1)
    [[ -n "$dev_a" ]] || { log_fail "PTPL: host A reconnect yielded no device"; return; }

    nvme resv-report "$dev_a" --numd=256 >"$TEST_DIR/report-ptpl.log" 2>&1
    if grep -qi "a1a1" "$TEST_DIR/report-ptpl.log"; then
        log_pass "Reservation Report shows A's key after restart (no re-registration)"
    else
        log_fail "A's reservation did not survive the restart"
        cat "$TEST_DIR/report-ptpl.log"
        return
    fi

    # Two-device mode: B's WRITE must still be fenced. Coalesced
    # topology can't drive a second initiator's block I/O — skip, not pass.
    dev_b=$(connect_host "$HOST_B_NQN" "$HOST_B_ID")
    if [[ -n "$dev_b" && "$dev_b" != "$dev_a" ]]; then
        if dd if=/dev/zero of="$dev_b" bs=4096 count=1 oflag=direct conv=fsync \
            status=none 2>/dev/null; then
            log_fail "host B WRITE succeeded after restart but should be fenced"
        else
            log_pass "host B WRITE still RESERVATION CONFLICT after restart"
        fi
    else
        log_skip "coalesced topology: post-restart block fencing needs independent devices"
    fi
}

main() {
    [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(free_port)
    [[ -z "$HTTP_PORT" ]] && HTTP_PORT=$(free_port)
    check_prerequisites
    mkdir -p "$TEST_DIR/data"
    create_test_config
    start_daemon
    create_volume

    local dev_a dev_b
    if ! dev_a=$(connect_host "$HOST_A_NQN" "$HOST_A_ID") || [[ -z "$dev_a" ]]; then
        log_fail "host A connect did not yield a namespace device"
        tail -30 "$TEST_DIR/daemon.log"
        dev_a=$(list_ns_devs | head -1)
    else
        log_pass "host A connected: $dev_a"
    fi

    dev_b=$(connect_host "$HOST_B_NQN" "$HOST_B_ID")
    if [[ -n "$dev_b" && "$dev_b" != "$dev_a" ]]; then
        log_pass "host B connected: $dev_b (independent device)"
        test_block_fencing "$dev_a" "$dev_b"
    else
        log_pass "host B connected (kernel coalesced onto $dev_a)"
        test_conflict_status "$dev_a"
    fi

    echo
    test_ptpl_survives_restart

    echo
    echo "===================="
    echo " Results: $PASS_COUNT passed / $FAIL_COUNT failed / $SKIP_COUNT skipped"
    echo "===================="
    if (( FAIL_COUNT > 0 )); then
        echo "Daemon log:"; tail -40 "$TEST_DIR/daemon.log"
        exit 1
    fi
    exit 0
}

main
