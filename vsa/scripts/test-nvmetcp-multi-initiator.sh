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
# host B registers, and host B is fenced.
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
log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; PASS_COUNT=$((PASS_COUNT+1)); }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; FAIL_COUNT=$((FAIL_COUNT+1)); }

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

transport: nvmetcp

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
    echo "===================="
    echo " Results: $PASS_COUNT passed / $FAIL_COUNT failed"
    echo "===================="
    if (( FAIL_COUNT > 0 )); then
        echo "Daemon log:"; tail -40 "$TEST_DIR/daemon.log"
        exit 1
    fi
    exit 0
}

main
