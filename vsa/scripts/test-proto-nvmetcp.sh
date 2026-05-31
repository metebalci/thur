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
#   --debug               Use ./target/debug/ binaries (default: release)
#   --daemon-path PATH    Override path to thurvsad binary
#   --cli-path PATH       Override path to thurvsa binary
#   --keep-data           Don't clean up test data directory
#   --nvmetcp-port PORT   Override nvmetcp port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#   --tls                 Enable TLS 1.3 PSK (NVMe-TCP §3.6.1.5). Adds
#                         prereqs: `tlshd` userspace TLS daemon running
#                         (Linux nvme_tcp uses kTLS — the kernel hands
#                         the handshake off to tlshd). Generates a single
#                         host PSK, registers it with the live daemon via
#                         `thurvsa nvmetcp psks add` (admitting the host
#                         to test-vol), imports the key into the kernel
#                         keyring, then runs nvme connect --tls.
#   --dhchap              Enable DH-HMAC-CHAP in-band auth (NVMe Base
#                         §8.13). Generates a host secret + a controller
#                         secret with `nvme gen-dhchap-key`, registers
#                         them with the live daemon via `thurvsa nvmetcp
#                         dhchap add` (admitting the host to test-vol
#                         only), and runs `nvme connect --dhchap-secret
#                         --dhchap-ctrl-secret` (mutual auth). Also
#                         asserts a wrong secret is refused and that a
#                         non-admitted volume stays invisible. Requires
#                         nvme-cli + kernel with dhchap support. Composes
#                         with --tls ("dhchap+tls").
#
#   Both auth modes set `nvmetcp.{tls,auth}.identity_file` to a path
#   OUTSIDE <data_dir> and assert the CLI `add` wrote there (not under
#   <data_dir>): the issue #69 regression guard — the admin handlers
#   must honor the override the transport reads, or every host is
#   silently refused.
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
PSK_KEY_STR=""
USE_DHCHAP=0
DHCHAP_KEY_STR=""
DHCHAP_CTRL_STR=""

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --nvmetcp-port) NVMETCP_PORT="$2"; shift 2 ;;
        --tls) USE_TLS=1; shift ;;
        --dhchap) USE_DHCHAP=1; shift ;;
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

    if [[ $USE_DHCHAP -eq 1 ]]; then
        # DH-HMAC-CHAP needs an nvme-cli/kernel that accepts
        # --dhchap-secret on connect (and can generate a secret).
        if ! nvme connect --help 2>&1 | grep -q -- '--dhchap-secret'; then
            missing+=("nvme connect --dhchap-secret (nvme-cli too old)")
            hints+=("  - dhchap: needs nvme-cli >= 2.0 and kernel >= 5.20")
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
    # Identity files live OUTSIDE <data_dir> on purpose: the override dir
    # is distinct from the daemon default so a handler that ignored
    # `identity_file` (issue #69) would write under <data_dir> and
    # diverge from where the transport reads. register_identities asserts
    # the override path got the entry and the default path did not.
    mkdir -p "$TEST_DIR/etc"
    local tls_block=""
    if [[ $USE_TLS -eq 1 ]]; then
        tls_block=$'\n  tls:\n    mode: psk\n    identity_file: "'"$TEST_DIR/etc/nvmetcp-psks.json"$'"'
    fi
    local auth_block=""
    if [[ $USE_DHCHAP -eq 1 ]]; then
        auth_block=$'\n  auth:\n    mode: dhchap\n    identity_file: "'"$TEST_DIR/etc/nvmetcp-dhchap.json"$'"'
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

transports: [nvmetcp]

http:
  listen: "127.0.0.1:$HTTP_PORT"

nvmetcp:
  listen: "0.0.0.0:$NVMETCP_PORT"$tls_block$auth_block

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
    # Stash the interchange string; it's registered with the LIVE daemon
    # later via `thurvsa nvmetcp psks add` (register_identities), which
    # exercises the admin write path against the identity_file override
    # — the issue #69 guard. The daemon mints an empty stub at boot.
    PSK_KEY_STR="$key_str"
}

setup_dhchap() {
    log_info "Generating DH-HMAC-CHAP secrets..."
    # `nvme gen-dhchap-key --hmac=1` emits one `DHHC-1:01:base64:` line:
    # a random, NQN-transformable secret (the transform binds to the
    # hostnqn at auth time on both sides). One for the host, one for the
    # controller (bidirectional / mutual auth).
    DHCHAP_KEY_STR=$(nvme gen-dhchap-key --hmac=1 \
        2>"$TEST_DIR/gen-dhchap-key.err" | grep -E '^DHHC-1:' | head -1)
    DHCHAP_CTRL_STR=$(nvme gen-dhchap-key --hmac=1 \
        2>>"$TEST_DIR/gen-dhchap-key.err" | grep -E '^DHHC-1:' | head -1)
    if [[ -z "$DHCHAP_KEY_STR" || -z "$DHCHAP_CTRL_STR" ]]; then
        log_error "nvme gen-dhchap-key produced no key: $(cat "$TEST_DIR/gen-dhchap-key.err")"
        exit 1
    fi
    log_info "Host secret ${DHCHAP_KEY_STR:0:16}... ctrl secret ${DHCHAP_CTRL_STR:0:16}..."
    # The secrets are registered with the LIVE daemon later via `thurvsa
    # nvmetcp dhchap add` (register_identities), admitting the host to
    # test-vol ONLY (test-vol-hidden stays invisible — the admission
    # fence) and exercising the admin write path against the
    # identity_file override (the issue #69 guard). Daemon mints an
    # empty stub at boot.
}

create_hidden_volume() {
    log_info "Creating a second, non-admitted volume (admission fence)..."
    "$CLI_PATH" --config "$TEST_CONFIG" --user root volume create test-vol-hidden \
        --size 100MiB --backend primary --page-size 64KiB \
        || { log_error "Failed to create hidden volume"; exit 1; }
}

# Issue #69 guard. The CLI `add` must land the entry in the configured
# identity_file override (<TEST_DIR>/etc/nvmetcp-<kind>.json) — where the
# transport reads — and must NOT write the <data_dir> default path. Under
# the pre-fix bug the admin handlers hardcoded <data_dir>/..., so the
# entry would diverge from the transport's read path and every host would
# be refused. <kind> is "psks" or "dhchap".
assert_identity_override() {
    local kind="$1"
    local override="$TEST_DIR/etc/nvmetcp-$kind.json"
    local default="$TEST_DIR/data/nvmetcp-$kind.json"
    if grep -qF "$HOST_NQN" "$override" 2>/dev/null; then
        log_pass "nvmetcp $kind add honored identity_file override (issue #69)"
    else
        log_fail "nvmetcp $kind entry missing from override $override (issue #69)"
    fi
    if [[ -e "$default" ]]; then
        log_fail "nvmetcp $kind written to <data_dir> default $default — override ignored (issue #69)"
    else
        log_pass "nvmetcp $kind not written under <data_dir> (issue #69)"
    fi
}

# Register host identities with the LIVE daemon via the admin CLI. This
# is the path the pre-fix bug broke (the handlers wrote the wrong file);
# the existing flow wrote the JSON by hand and never exercised it. Runs
# after the volumes exist (admission validates volume names) and before
# connect (the daemon re-reads the identity file per handshake).
register_identities() {
    if [[ $USE_TLS -eq 1 ]]; then
        log_info "Registering host PSK via 'nvmetcp psks add'..."
        if "$CLI_PATH" --config "$TEST_CONFIG" --user root \
            nvmetcp psks add --host-nqn "$HOST_NQN" --key "$PSK_KEY_STR" \
            --volume test-vol >"$TEST_DIR/psks-add.log" 2>&1; then
            assert_identity_override psks
        else
            log_fail "nvmetcp psks add failed: $(cat "$TEST_DIR/psks-add.log")"
        fi
    fi
    if [[ $USE_DHCHAP -eq 1 ]]; then
        log_info "Registering host DH-HMAC-CHAP secret via 'nvmetcp dhchap add'..."
        if "$CLI_PATH" --config "$TEST_CONFIG" --user root \
            nvmetcp dhchap add --host-nqn "$HOST_NQN" --key "$DHCHAP_KEY_STR" \
            --ctrl-key "$DHCHAP_CTRL_STR" --volume test-vol \
            >"$TEST_DIR/dhchap-add.log" 2>&1; then
            assert_identity_override dhchap
        else
            log_fail "nvmetcp dhchap add failed: $(cat "$TEST_DIR/dhchap-add.log")"
        fi
    fi
}

run_dhchap_admission_check() {
    log_info "Checking DH-HMAC-CHAP volume admission fence..."
    # test-vol is admitted (visible); test-vol-hidden is not. Linux only
    # creates block devices for namespaces in the (filtered) Active NS
    # list, so the non-admitted namespace must have no /dev node.
    if [[ -e "/dev/${NVME_DEVICE}n1" ]]; then
        log_pass "Admitted namespace /dev/${NVME_DEVICE}n1 visible"
    else
        log_fail "Admitted namespace /dev/${NVME_DEVICE}n1 missing"
    fi
    if [[ -e "/dev/${NVME_DEVICE}n2" ]]; then
        log_fail "Non-admitted namespace /dev/${NVME_DEVICE}n2 visible — fence broken"
    else
        log_pass "Non-admitted namespace fenced (no /dev/${NVME_DEVICE}n2)"
    fi
}

run_dhchap_wrong_secret() {
    log_info "Verifying a wrong DH-HMAC-CHAP secret is refused..."
    local bogus
    bogus=$(nvme gen-dhchap-key --hmac=1 2>/dev/null | grep -E '^DHHC-1:' | head -1)
    if [[ -z "$bogus" ]]; then
        log_info "could not generate a bogus key; skipping negative test"
        return
    fi
    if nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
        -n "$SUBNQN" --hostnqn "$HOST_NQN" --dhchap-secret "$bogus" \
        >"$TEST_DIR/nvme-connect-bogus.log" 2>&1; then
        log_fail "connect with a wrong secret SUCCEEDED (should be refused)"
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1
    else
        log_pass "connect with a wrong secret refused"
    fi

    # Issue #68: the refusal must also leave a forensic trail. Verify the
    # live daemon's NvmetcpLoginAudit sink wrote a reply_invalid row to the
    # audit chain (the writer task is async, so poll briefly).
    local found=0
    for _ in $(seq 1 30); do
        if grep -rqs 'nvmetcp.dhchap.failure' "$TEST_DIR/data/audit/" 2>/dev/null \
            && grep -rqs 'reply_invalid' "$TEST_DIR/data/audit/" 2>/dev/null; then
            found=1
            break
        fi
        sleep 0.2
    done
    if [[ $found -eq 1 ]]; then
        log_pass "wrong-secret refusal recorded an nvmetcp.dhchap.failure audit row"
    else
        log_fail "no nvmetcp.dhchap.failure (reply_invalid) audit row after refused connect"
    fi
}

connect_nvmetcp() {
    log_info "Connecting via nvme-cli..."
    local tls_args=()
    if [[ $USE_TLS -eq 1 ]]; then
        tls_args+=(--tls --tls-key="$TLS_KEY_STR")
    fi
    if [[ $USE_DHCHAP -eq 1 ]]; then
        tls_args+=(--dhchap-secret "$DHCHAP_KEY_STR")
        [[ -n "$DHCHAP_CTRL_STR" ]] && tls_args+=(--dhchap-ctrl-secret "$DHCHAP_CTRL_STR")
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

run_reservation_tests() {
    log_info "Running NVMe reservation flow (register / acquire / report / notif-log / release)..."
    local dev="/dev/${NVME_DEVICE}n1"
    local key=0x123456

    if ! nvme resv-register "$dev" --rrega=0 --nrkey="$key" \
        >"$TEST_DIR/resv-register.log" 2>&1; then
        log_fail "resv-register failed: $(cat "$TEST_DIR/resv-register.log")"
        return
    fi
    log_pass "Reservation Register accepted"

    if ! nvme resv-acquire "$dev" --crkey="$key" --rtype=1 --racqa=0 \
        >"$TEST_DIR/resv-acquire.log" 2>&1; then
        log_fail "resv-acquire failed: $(cat "$TEST_DIR/resv-acquire.log")"
        return
    fi
    log_pass "Reservation Acquire (Write Exclusive) accepted"

    if ! nvme resv-report "$dev" --numd=256 >"$TEST_DIR/resv-report.log" 2>&1; then
        log_fail "resv-report failed: $(cat "$TEST_DIR/resv-report.log")"
        return
    fi
    # The registrant key (0x123456) and rtype=1 must appear in the
    # Reservation Status Data Structure.
    if grep -qiE "123456" "$TEST_DIR/resv-report.log"; then
        log_pass "Reservation Report lists the registrant key"
    else
        log_fail "Reservation Report missing registrant key 0x123456"
        cat "$TEST_DIR/resv-report.log"
    fi

    # Reservation Notification log page (LID 0x80). Before AER landed
    # this returned "Invalid Field in Command"; it must now return a
    # well-formed 64-byte page. A single-host flow never queues an entry
    # (the issuing host is never notified of its own ops), so the page is
    # the all-zero empty form — a successful read is the assertion.
    # Cross-host notification semantics (which type, which host) are
    # covered by the Rust unit + transport tests.
    if nvme resv-notif-log --help >/dev/null 2>&1; then
        if nvme resv-notif-log "$dev" >"$TEST_DIR/resv-notif-log.log" 2>&1; then
            log_pass "Reservation Notification log (LID 0x80) returns a valid page"
        else
            log_fail "resv-notif-log failed: $(cat "$TEST_DIR/resv-notif-log.log")"
        fi
    else
        log_info "nvme-cli lacks resv-notif-log; skipping LID 0x80 assertion"
    fi

    if ! nvme resv-release "$dev" --crkey="$key" --rtype=1 --rrela=0 \
        >"$TEST_DIR/resv-release.log" 2>&1; then
        log_fail "resv-release failed: $(cat "$TEST_DIR/resv-release.log")"
        return
    fi
    log_pass "Reservation Release accepted"
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
    if [[ $USE_DHCHAP -eq 1 ]]; then
        setup_dhchap
    fi
    create_test_config
    start_daemon
    create_volume
    if [[ $USE_DHCHAP -eq 1 ]]; then
        create_hidden_volume
    fi
    register_identities
    if connect_nvmetcp; then
        run_identify_tests
        [[ $USE_DHCHAP -eq 1 ]] && run_dhchap_admission_check
        run_smart_log
        run_io_round_trip
        run_reservation_tests
        run_disconnect
        [[ $USE_DHCHAP -eq 1 ]] && run_dhchap_wrong_secret
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
