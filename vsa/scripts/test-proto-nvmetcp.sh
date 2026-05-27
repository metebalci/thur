#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa NVMe/TCP Conformance Test
#
# Drives thurvsad's NVMe/TCP transport with the Linux kernel
# nvme_tcp host driver via nvme-cli. Verifies the full host-visible
# round trip end-to-end:
#
#   1. ICReq / ICResp handshake
#   2. Connect with SUBNQN admission
#   3. Property Get CAP / VS, Property Set CC.EN
#   4. Identify Controller / Identify Namespace / Active NS list
#   5. Get Features Number of Queues
#   6. Get Log Page SMART / Health
#   7. dd write + read 10 MiB round-trip with sha256 compare
#   8. Disconnect — clean teardown
#
# This is the live-stack counterpart to the in-process loopback tests
# in nvme-tcp/src/server.rs. The in-process tests exercise the codec
# + dispatch without a kernel; this script exercises real Linux
# nvme_tcp behavior (which is much pickier about spec compliance
# than synthetic tests).
#
# Prerequisites:
#   - nvme-cli         (Debian/Ubuntu: sudo apt-get install nvme-cli)
#   - nvme_tcp kernel module (load with: sudo modprobe nvme_tcp)
#   - thurvsad and thurvsa (built or on PATH)
#   - sudo: nvme connect / disconnect + raw block I/O require root.
#     The script self-elevates via 'exec sudo "$0" "$@"' on first
#     entry. Either run from a NOPASSWD sudoers entry or be prepared
#     to enter your password.
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-proto-nvmetcp.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default: debug)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --nvmetcp-port PORT   Override nvmetcp port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#   --tls                 Enable TLS 1.3 PSK (NVMe-TCP §3.6.1.5). Adds
#                         prereqs: `tlshd` userspace TLS daemon running
#                         (Linux nvme_tcp uses kTLS — the kernel hands
#                         the handshake off to tlshd). Generates a
#                         single host PSK, writes nvmetcp-psks.json,
#                         imports the key into the kernel keyring, then
#                         runs nvme connect --tls. Everything else
#                         identical to the cleartext path.
#

# Self-elevate to root (needed for nvme connect / disconnect and the
# raw /dev/nvmeXn1 access from dd). Skip when already root.
if [[ $EUID -ne 0 ]]; then
    exec sudo --preserve-env=PATH "$0" "$@"
fi

# Note: We don't use 'set -e' because we want to run all tests even if some fail.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

TEST_DIR="/tmp/thurvsa-test-proto-nvmetcp-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
NVMETCP_PORT=""
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-conformance-test"
NVME_DEVICE=""
USE_TLS=0
TLS_KEY_STR=""

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
        --tls) USE_TLS=1; shift ;;
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

# Isolate the kernel session keyring for TLS-PSK runs. Without this,
# leftover `.nvme` PSK entries from a prior run (or a revoked session
# keyring inherited from a crashed run) make `nvme gen-tls-key` fail
# with `EKEYREVOKED` (errno 128) — the symptom shows up as
# `Failed to insert key, error 128`. `keyctl session -` re-execs the
# script attached to a fresh anonymous session keyring that's tied to
# this process; the kernel reaps it on exit, so no manual cleanup is
# needed. The `THURVSA_KEYRING_ISOLATED` guard prevents an infinite
# re-exec loop. Only TLS runs need this — non-TLS conformance never
# touches the keyring.
if [[ $USE_TLS -eq 1 && "${THURVSA_KEYRING_ISOLATED:-}" != "1" ]]; then
    if command -v keyctl >/dev/null 2>&1; then
        export THURVSA_KEYRING_ISOLATED=1
        exec keyctl session - "$0" "$@"
    fi
fi

PASS_COUNT=0
FAIL_COUNT=0
log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; PASS_COUNT=$((PASS_COUNT+1)); }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; FAIL_COUNT=$((FAIL_COUNT+1)); }

cleanup() {
    # Disconnect any leftover NVMe connection to the test subsystem
    # FIRST — leaving an active connection past daemon shutdown
    # leaves /dev/nvmeXn1 pointing at a dead controller and confuses
    # the next run.
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
    if ! command -v nvme >/dev/null 2>&1; then
        missing+=("nvme")
        hints+=("  - nvme-cli: sudo apt-get install nvme-cli")
    fi
    if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
        missing+=("nvme_tcp kernel module")
        hints+=("  - nvme_tcp: sudo modprobe nvme_tcp (kernel ≥ 5.0 required)")
    fi
    if ! command -v sha256sum >/dev/null 2>&1; then
        missing+=("sha256sum")
        hints+=("  - sha256sum: sudo apt-get install coreutils")
    fi
    if [[ $USE_TLS -eq 1 ]]; then
        # tlshd is the userspace TLS daemon Linux nvme_tcp hands the
        # TLS 1.3 handshake off to (kTLS data path). Without it
        # `nvme connect --tls` hangs waiting for the handshake.
        if ! systemctl is-active --quiet tlshd 2>/dev/null \
            && ! pgrep -x tlshd >/dev/null 2>&1; then
            missing+=("tlshd (running)")
            hints+=("  - tlshd: sudo apt-get install ktls-utils && sudo systemctl start tlshd")
        fi
        # keyctl is what we used above to re-exec under a fresh
        # session keyring; if the binary's missing the isolation
        # wrapper silently fell through and `nvme gen-tls-key` will
        # fail later with a confusing error. Surface it now.
        if ! command -v keyctl >/dev/null 2>&1; then
            missing+=("keyctl")
            hints+=("  - keyctl: sudo apt-get install keyutils")
        fi
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

    log_info "All prerequisites met"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes"
    local tls_block=""
    if [[ $USE_TLS -eq 1 ]]; then
        tls_block=$'\n  tls:\n    mode: psk\n    identity_file: "'"$TEST_DIR/data/nvmetcp-psks.json"$'"'
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

transport: nvmetcp

http:
  listen: "127.0.0.1:$HTTP_PORT"

nvmetcp:
  listen: "0.0.0.0:$NVMETCP_PORT"$tls_block

storage:
  backends:
    primary:
      type: local
      root_dir: "$TEST_DIR/storage-primary"

EOFCONFIG

    mkdir -p "$TEST_DIR/storage-primary"
}

free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad (NVMe/TCP)..."
    # Crank nvme-tcp tracing high so we can see accept_loop iterations
    # and any TLS-handshake failures land in the test log.
    RUST_LOG="info,nvme_tcp=trace,nvme_nvm=debug" \
        "$DAEMON_PATH" --config "$TEST_CONFIG" > "$TEST_DIR/daemon.log" 2>&1 &
    DAEMON_PID=$!
    # Wait for the nvmetcp listener to come up
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

setup_tls_psk() {
    log_info "Generating TLS PSK and inserting into .nvme keyring..."
    # nvme gen-tls-key --insert --keyring=.nvme emits two lines:
    #   1. the NVMeTLSkey-1:NN:base64: interchange string
    #   2. "Inserted TLS key <hex-serial>"
    # We need line 1 for the daemon's identity file and line 2 for
    # `nvme connect --tls-key=`. Kernel nvme-fabrics only finds keys
    # in the .nvme keyring — passing the interchange string directly
    # to --tls-key= lands it in the wrong place and the kernel
    # rejects the connect with "failed to write to nvme-fabrics
    # device".
    local out
    out=$(nvme gen-tls-key --hostnqn="$HOST_NQN" --subsysnqn="$SUBNQN" \
        --hmac=1 --identity=1 --insert --keyring=.nvme \
        2>"$TEST_DIR/gen-tls-key.err") \
        || { log_error "nvme gen-tls-key failed: $(cat "$TEST_DIR/gen-tls-key.err")"; exit 1; }
    local key_str serial
    key_str=$(echo "$out" | grep -E '^NVMeTLSkey-1:0[12]:' | head -1)
    serial=$(echo "$out" | grep -E '^Inserted' | awk '{print $NF}')
    if [[ -z "$key_str" || -z "$serial" ]]; then
        log_error "Unexpected gen-tls-key output: $out"
        exit 1
    fi
    # nvme-cli's --tls-key= parses with strtoul(s, NULL, 0). A leading
    # "0" makes it octal — `049765c7` becomes `04` = 4. Convert to
    # plain decimal so the kernel resolves the right keyring entry.
    local serial_dec
    serial_dec=$(printf '%d' "0x$serial")
    log_info "Generated PSK: ${key_str:0:24}...: (keyring serial: 0x$serial / $serial_dec)"
    TLS_KEY_STR="$serial_dec"

    # Daemon-side identity file
    cat > "$TEST_DIR/data/nvmetcp-psks.json" <<EOFPSK
{
  "version": 1,
  "psks": [
    {
      "host_nqn": "$HOST_NQN",
      "interchange_key": "$key_str"
    }
  ]
}
EOFPSK
    chmod 0640 "$TEST_DIR/data/nvmetcp-psks.json"
}

connect_nvmetcp() {
    log_info "Connecting via nvme-cli..."
    local tls_args=()
    if [[ $USE_TLS -eq 1 ]]; then
        tls_args+=(--tls --tls-key="$TLS_KEY_STR")
    fi
    if ! nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
        -n "$SUBNQN" --hostnqn "$HOST_NQN" "${tls_args[@]}" \
        2>&1 | tee "$TEST_DIR/nvme-connect.log"; then
        log_fail "nvme connect failed"
        return 1
    fi
    # nvme connect prints "connecting to device: nvmeN" — parse it
    NVME_DEVICE=$(nvme list-subsys -o json 2>/dev/null \
        | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next((c["Name"] for s in d for ss in s.get("Subsystems",[]) if ss.get("NQN","")=="'"$SUBNQN"'" for c in ss.get("Paths",[])), ""))' \
        2>/dev/null)
    if [[ -z "$NVME_DEVICE" ]]; then
        # Fallback: scan /dev/nvme*
        NVME_DEVICE=$(ls /dev/nvme*n1 2>/dev/null | head -1 | xargs -n1 basename | sed 's/n1$//')
    fi
    if [[ -z "$NVME_DEVICE" ]]; then
        log_fail "Could not locate the connected NVMe device"
        return 1
    fi
    log_pass "Connected: /dev/${NVME_DEVICE}n1"
}

run_identify_tests() {
    log_info "Running Identify probes..."
    if nvme id-ctrl "/dev/$NVME_DEVICE" > "$TEST_DIR/id-ctrl.txt" 2>&1; then
        if grep -q 'subnqn  *: '"$SUBNQN" "$TEST_DIR/id-ctrl.txt"; then
            log_pass "Identify Controller: SUBNQN matches"
        else
            log_fail "Identify Controller: SUBNQN mismatch in id-ctrl output"
            grep '^subnqn' "$TEST_DIR/id-ctrl.txt" || true
        fi
    else
        log_fail "nvme id-ctrl failed"
        cat "$TEST_DIR/id-ctrl.txt"
    fi
    if nvme id-ns "/dev/${NVME_DEVICE}n1" -n 1 > "$TEST_DIR/id-ns.txt" 2>&1; then
        log_pass "Identify Namespace returned"
    else
        log_fail "nvme id-ns failed"
        cat "$TEST_DIR/id-ns.txt"
    fi
}

run_smart_log() {
    log_info "Running Get Log Page (SMART)..."
    if nvme smart-log "/dev/$NVME_DEVICE" > "$TEST_DIR/smart.txt" 2>&1; then
        if grep -q 'temperature' "$TEST_DIR/smart.txt"; then
            log_pass "SMART log returned (temperature field present)"
        else
            log_fail "SMART log missing expected fields"
            cat "$TEST_DIR/smart.txt"
        fi
    else
        log_fail "nvme smart-log failed"
        cat "$TEST_DIR/smart.txt"
    fi
}

run_io_round_trip() {
    log_info "Running 10 MiB dd round trip with sha256 check..."
    local infile="$TEST_DIR/in.bin"
    local outfile="$TEST_DIR/out.bin"
    dd if=/dev/urandom of="$infile" bs=1M count=10 status=none
    local in_sha
    in_sha=$(sha256sum "$infile" | awk '{print $1}')

    if ! dd if="$infile" of="/dev/${NVME_DEVICE}n1" bs=1M count=10 oflag=direct conv=fsync status=none 2>"$TEST_DIR/dd-write.err"; then
        log_fail "dd write failed: $(cat "$TEST_DIR/dd-write.err")"
        return
    fi
    if ! dd if="/dev/${NVME_DEVICE}n1" of="$outfile" bs=1M count=10 iflag=direct status=none 2>"$TEST_DIR/dd-read.err"; then
        log_fail "dd read failed: $(cat "$TEST_DIR/dd-read.err")"
        return
    fi
    local out_sha
    out_sha=$(sha256sum "$outfile" | awk '{print $1}')
    if [[ "$in_sha" == "$out_sha" ]]; then
        log_pass "10 MiB write+read round trip preserves SHA-256"
    else
        log_fail "Data integrity mismatch: in=$in_sha out=$out_sha"
    fi
}

run_disconnect() {
    log_info "Disconnecting..."
    if nvme disconnect -n "$SUBNQN" >/dev/null 2>&1; then
        log_pass "Disconnect succeeded"
    else
        log_fail "Disconnect failed"
    fi
}

main() {
    [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(free_port)
    [[ -z "$HTTP_PORT" ]] && HTTP_PORT=$(free_port)
    check_prerequisites
    mkdir -p "$TEST_DIR/data"
    if [[ $USE_TLS -eq 1 ]]; then
        setup_tls_psk
    fi
    create_test_config
    start_daemon
    create_volume
    if connect_nvmetcp; then
        run_identify_tests
        run_smart_log
        run_io_round_trip
        run_disconnect
    fi

    echo
    echo "===================="
    echo " Results: $PASS_COUNT passed / $FAIL_COUNT failed"
    echo "===================="
    if (( FAIL_COUNT > 0 )); then
        echo "Daemon log:"
        tail -30 "$TEST_DIR/daemon.log"
        exit 1
    fi
    exit 0
}

main
