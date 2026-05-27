#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Smoke Test
#
# Lightweight checks against the daemon's management surface — HTTP endpoints,
# CLI commands, iSCSI discovery (sendtargets only, no SCSI data path), and
# daemon log hygiene. Does NOT exercise the SCSI data path or write to a tape.
#
# No sudo / no kernel iSCSI initiator required: discovery uses libiscsi's
# userspace iscsi-ls.
#
# Companions:
#   - test-proto-iscsi.sh — iSCSI protocol layer (login + INQUIRY) via libiscsi
#   - test-scsi-conformance.sh  — full SCSI/SSC/SMC conformance via sg3_utils (sudo)
#   - test-backup-workflow.sh   — end-to-end tar+mtx backup/restore (sudo)
#
# Usage (invoke from repo root):
#   ./vtl/scripts/test-smoke.sh [OPTIONS]
#
# Options:
#   --release             Use ./target/release/ binaries (default is ./target/debug/)
#   --daemon-path PATH    Path to thurvtld binary (overrides default)
#   --cli-path PATH       Path to thurvtl binary (overrides default)
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

# Note: We don't use 'set -e' because we want to run all tests even if some fail

# Shared helpers (colors + log_info/warn/error/test + pick_free_port +
# assign_ports). Behaviour-identical to the copies these scripts used
# to carry inline.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

# Configuration
BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/test-smoke-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
DAEMON_PID=""

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --release)
            BUILD_PROFILE="release"
            shift
            ;;
        --daemon-path)
            DAEMON_PATH="$2"
            shift 2
            ;;
        --cli-path)
            CLI_PATH="$2"
            shift 2
            ;;
        --keep-data)
            KEEP_DATA=1
            shift
            ;;
        --iscsi-port)
            ISCSI_PORT="$2"
            shift 2
            ;;
        --http-port)
            HTTP_PORT="$2"
            shift 2
            ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

cleanup() {
    standard_cleanup
}

# Set up cleanup trap
trap cleanup EXIT INT TERM

# Check prerequisites
check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."

    require_daemon_binaries thurvtl

    if ! command -v iscsi-inq &> /dev/null; then
        log_error "iscsi-inq not found. Install with: sudo apt-get install libiscsi-bin"
        exit 1
    fi

    if ! command -v curl &> /dev/null; then
        log_error "curl not found. Install with: sudo apt-get install curl"
        exit 1
    fi

    if ! command -v openssl &> /dev/null; then
        log_error "openssl not found. Install with: sudo apt-get install openssl"
        exit 1
    fi

    log_info "All prerequisites met"
}

# Create test configuration
create_test_config() {
    log_info "Creating test configuration..."

    mkdir -p "$TEST_DIR"

    cat > "$TEST_CONFIG" << EOFCONFIG
data_dir: "$TEST_DIR/data"

library:
  num_slots: 40
  num_drives: 2
  lto_generation: 8

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"

storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

keystore:
  backends:
    local: { type: local }
EOFCONFIG

    mkdir -p "$TEST_DIR/data"
    log_info "Test config created at: $TEST_CONFIG"
}

# Create test cartridges via the daemon's admin socket. Runs AFTER
# start_daemon() since cartridge create is now daemon-routed.
create_test_cartridges() {
    log_info "Creating test cartridges via admin socket..."

    local barcode="TEST001L8"
    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$barcode" --lto-generation 8 2>&1)

    if echo "$output" | grep -q "Created cartridge"; then
        log_info "✓ Created test cartridge: $barcode"
    else
        log_warn "Could not create test cartridge: $output"
    fi

    # Second cartridge: at-rest encrypted with the `local` keystore
    # entry from `keystore.backends:` in the YAML conffile. Exercises
    # the appliance-side at-rest seam: keystore generate-and-wrap,
    # manifest encryption block, DEK cache pre-population.
    local enc_barcode="ENC001L8"
    output=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$enc_barcode" \
        --lto-generation 8 --encrypt --keystore local 2>&1)
    if echo "$output" | grep -q "Created cartridge" \
        && echo "$output" | grep -q "At-rest encryption: keystore 'local'"; then
        log_info "✓ Created at-rest encrypted cartridge: $enc_barcode"
    else
        log_warn "Could not create encrypted test cartridge: $output"
    fi
}

# Start daemon
start_daemon() {
    # Run the daemon with the admin socket inside the test workspace
    # so we don't need write access to /run/thurvtl/. Both the
    # daemon and CLI honor THURVTL_ADMIN_SOCKET; exporting once
    # covers every CLI invocation downstream of this script.
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    start_thur_daemon
}

# Test: iSCSI surface alive — login + INQUIRY against the changer LUN.
#
# We use iscsi-inq instead of iscsi-ls (also userspace, no sudo) because
# iscsi-ls's post-SendTargets target-rescan trips on libiscsi's hostname
# parser; iscsi-inq does a direct portal connection and exercises a strict
# superset (TCP connect + Login PDU + CmdSN/StatSN + SCSI INQUIRY + Logout).
test_discovery() {
    log_test "Testing iSCSI surface (login + INQUIRY against changer LUN 0)..."

    local output
    output=$(timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/0" 2>&1)

    if echo "$output" | grep -q "Peripheral Device Type:MEDIA_CHANGER"; then
        log_info "✓ iSCSI surface alive (INQUIRY returned MEDIA_CHANGER)"
        return 0
    else
        log_error "✗ iSCSI INQUIRY failed. Output: $output"
        return 1
    fi
}

# Test: HTTP Health Check
test_http_health() {
    log_test "Testing HTTP health endpoint..."

    local response
    response=$(curl -s "http://127.0.0.1:$HTTP_PORT/health")

    if echo "$response" | grep -q '"status":"ok"'; then
        log_info "✓ Health check passed: $response"
        return 0
    else
        log_error "✗ Health check failed. Response: $response"
        return 1
    fi
}

# Test: Library Info
test_library_info() {
    log_test "Testing library info..."

    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" library info 2>&1)

    if echo "$output" | grep -q "Storage slots:"; then
        log_info "✓ Library info retrieved successfully"
        return 0
    else
        log_error "✗ Library info failed. Output: $output"
        return 1
    fi
}

# Test: At-rest encryption manifest + `cartridge key show` reports
# the encrypted cartridge created above. We stop the daemon for the
# read-only `key show` call (the dispatcher routes the verb daemon-
# down), then restart so subsequent tests use a live daemon.
test_at_rest_encryption() {
    log_test "Testing at-rest encryption manifest + cartridge key show..."

    local enc_barcode="ENC001L8"
    local manifest="${TEST_DIR}/data/tapes/${enc_barcode}/manifest.json"
    if [[ ! -f "$manifest" ]]; then
        log_error "✗ encrypted cartridge manifest missing at $manifest"
        return 1
    fi
    # serde's snake_case rename of `Aes256Gcm` collapses to
    # `aes256_gcm` (no underscore between digits and letters); the
    # human-readable `as_str()` form is `aes_256_gcm` and is what
    # the CLI prints. Mirrors the VSA pattern.
    if ! grep -q '"encryption"' "$manifest" \
        || ! grep -q '"keystore_backend":"local"' "$manifest" \
        || ! grep -q '"algorithm":"aes256_gcm"' "$manifest"; then
        log_error "✗ encrypted manifest is missing the expected encryption block:"
        cat "$manifest"
        return 1
    fi

    # `cartridge key show` is daemon-down — temporarily stop the
    # daemon so the CLI takes the daemon-down path.
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true

    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge key show "$enc_barcode" 2>&1)
    if ! echo "$output" | grep -q "At-rest encryption: enabled" \
        || ! echo "$output" | grep -q "Algorithm:        aes_256_gcm" \
        || ! echo "$output" | grep -q "Keystore backend: local"; then
        log_error "✗ cartridge key show did not report the expected metadata:"
        echo "$output"
        # Restart daemon before bailing so the rest of the suite has one.
        start_daemon
        return 1
    fi

    # Restart the daemon: exercises the boot-time DEK unwrap path
    # (the daemon walks tapes/*/manifest.json, finds the encryption
    # block, asks the `local` keystore to unwrap, populates the
    # DriveManager DEK cache).
    start_daemon
    if ! grep -q "At-rest DEK cached for cartridge '${enc_barcode}'" "${TEST_DIR}/daemon.log"; then
        log_error "✗ daemon did not report at-rest DEK cache pre-population in its log"
        tail -40 "${TEST_DIR}/daemon.log"
        return 1
    fi

    log_info "✓ at-rest encryption manifest + key show + boot-time unwrap all OK"
    return 0
}

# Test: Check Cartridge Exists (created before daemon started)
test_create_cartridge() {
    log_test "Testing cartridge exists (created before daemon started)..."

    local barcode="TEST001L8"
    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge list 2>&1)

    if echo "$output" | grep -q "$barcode"; then
        log_info "✓ Cartridge exists: $barcode"
        return 0
    else
        log_error "✗ Cartridge not found. Output: $output"
        return 1
    fi
}

# Test: List Cartridges
test_list_cartridges() {
    log_test "Testing cartridge list..."

    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" cartridge list 2>&1)

    if echo "$output" | grep -q "TEST001L8"; then
        log_info "✓ Cartridge list shows created cartridge"
        return 0
    else
        log_error "✗ Cartridge not found in list. Output: $output"
        return 1
    fi
}

# Test: Drive Status (tape-specific)
test_drive_status() {
    log_test "Testing drive status..."

    local output
    output=$("$CLI_PATH" --config "$TEST_CONFIG" drive status 0 2>&1)

    if echo "$output" | grep -q -E "(Drive|Empty|Loaded)"; then
        log_info "✓ Drive status command successful"
        log_info "  $output"
        return 0
    else
        log_error "✗ Drive status failed. Output: $output"
        return 1
    fi
}

# Test: HTTP Sessions Endpoint
test_http_sessions() {
    log_test "Testing HTTP sessions endpoint..."

    local response
    response=$(curl -s "http://127.0.0.1:$HTTP_PORT/sessions")

    if echo "$response" | grep -q -E "(sessions|target_iqn)"; then
        log_info "✓ Sessions endpoint responded successfully"
        return 0
    else
        log_error "✗ Sessions endpoint failed. Response: $response"
        return 1
    fi
}

# Test: HTTP Info Endpoint
test_http_info() {
    log_test "Testing HTTP info endpoint..."

    local response
    response=$(curl -s "http://127.0.0.1:$HTTP_PORT/info")

    if echo "$response" | grep -q "drives"; then
        log_info "✓ Info endpoint responded successfully"
        return 0
    else
        log_error "✗ Info endpoint failed. Response: $response"
        return 1
    fi
}

# Test: HTTPS auto-gen + load-existing.
#
# Stops the running daemon, restarts it with an http.tls block whose
# cert/key paths are both missing, asserts the daemon (a) auto-generates
# a self-signed pair logged at WARN, (b) serves /health over HTTPS,
# (c) sets the right file modes, (d) on a second restart loads the
# existing pair (no regeneration) with the same fingerprint.
test_http_tls_auto_gen() {
    log_test "Testing HTTPS auto-gen + load-existing..."
    local tls_config="${TEST_DIR}/config-tls.yaml"
    local tls_dir="${TEST_DIR}/tls"
    local tls_log="${TEST_DIR}/daemon-tls.log"
    rm -rf "$tls_dir"
    mkdir -p "$tls_dir"

    # Stop the daemon started by start_daemon() so we can bring up a
    # fresh process under TLS without overlapping binds.
    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi

    # Reuse the existing config wholesale; append the http.tls block.
    # Drop the original http: line first so the new one wins.
    sed '/^http:/,/^[^ ]/{/^http:/d;/^  /d;}' "$TEST_CONFIG" > "$tls_config"
    cat >> "$tls_config" <<EOFTLS
http:
  listen: "127.0.0.1:$HTTP_PORT"
  tls:
    cert_file: "$tls_dir/cert.pem"
    key_file: "$tls_dir/key.pem"
    client_ca_file: ""
EOFTLS

    # First run: auto-generate.
    NO_COLOR=1 RUST_LOG=info "$DAEMON_PATH" --config "$tls_config" > "$tls_log" 2>&1 &
    DAEMON_PID=$!
    local ready=0
    for _ in {1..30}; do
        if curl -ksf "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 1
    done
    if [[ $ready -ne 1 ]]; then
        log_error "✗ TLS daemon did not become ready"
        tail -20 "$tls_log"
        return 1
    fi

    if ! grep -q "self-signed cert generated" "$tls_log"; then
        log_error "✗ expected WARN self-signed log line missing"
        tail -20 "$tls_log"
        return 1
    fi

    local cert_mode key_mode
    cert_mode=$(stat -c '%a' "$tls_dir/cert.pem" 2>/dev/null)
    key_mode=$(stat -c '%a' "$tls_dir/key.pem" 2>/dev/null)
    if [[ "$cert_mode" != "644" || "$key_mode" != "600" ]]; then
        log_error "✗ wrong file modes: cert=$cert_mode key=$key_mode (want 644 / 600)"
        return 1
    fi

    local fp1
    fp1=$(grep -o 'fingerprint_sha256=[a-f0-9]\{64\}' "$tls_log" | head -1 | cut -d= -f2)
    if [[ -z "$fp1" ]]; then
        log_error "✗ no fingerprint logged on first start"
        return 1
    fi

    # Second run: load existing.
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    : > "$tls_log"
    NO_COLOR=1 RUST_LOG=info "$DAEMON_PATH" --config "$tls_config" > "$tls_log" 2>&1 &
    DAEMON_PID=$!
    ready=0
    for _ in {1..30}; do
        if curl -ksf "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 1
    done
    if [[ $ready -ne 1 ]]; then
        log_error "✗ TLS daemon did not restart"
        tail -20 "$tls_log"
        return 1
    fi
    if grep -q "self-signed cert generated" "$tls_log"; then
        log_error "✗ second start regenerated cert (should have loaded existing)"
        return 1
    fi
    if ! grep -q "loaded existing cert/key" "$tls_log"; then
        log_error "✗ expected 'loaded existing cert/key' log missing"
        tail -20 "$tls_log"
        return 1
    fi
    local fp2
    fp2=$(grep -o 'fingerprint_sha256=[a-f0-9]\{64\}' "$tls_log" | head -1 | cut -d= -f2)
    if [[ "$fp1" != "$fp2" ]]; then
        log_error "✗ fingerprint changed across restart: $fp1 vs $fp2"
        return 1
    fi

    log_info "✓ HTTPS auto-gen + load-existing OK (fp=${fp1:0:16}…)"
    return 0
}

# Test: HTTPS mTLS (client-cert enforcement).
#
# Mints a self-signed CA + leaf client cert (plus an independent
# wrong-CA leaf), restarts the daemon under http.tls with
# client_ca_file set, and asserts: (a) curl with the trusted client
# cert succeeds, (b) curl with no client cert is refused at the TLS
# handshake, (c) curl with a cert signed by an untrusted CA is also
# refused (CA pinning, not just "any cert").
test_http_mtls() {
    log_test "Testing HTTPS mTLS (client-cert enforcement)..."
    local mtls_config="${TEST_DIR}/config-mtls.yaml"
    local mtls_dir="${TEST_DIR}/mtls"
    local mtls_log="${TEST_DIR}/daemon-mtls.log"
    rm -rf "$mtls_dir"
    mkdir -p "$mtls_dir"

    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi

    # Trusted CA + client leaf. extendedKeyUsage=clientAuth is required;
    # rustls rejects client certs lacking it.
    openssl ecparam -name prime256v1 -genkey -noout -out "$mtls_dir/ca.key" 2>/dev/null
    openssl req -new -x509 -key "$mtls_dir/ca.key" -out "$mtls_dir/ca.pem" \
        -days 1 -subj "/CN=test-mtls-ca" 2>/dev/null
    openssl ecparam -name prime256v1 -genkey -noout -out "$mtls_dir/client.key" 2>/dev/null
    openssl req -new -key "$mtls_dir/client.key" -out "$mtls_dir/client.csr" \
        -subj "/CN=test-client" 2>/dev/null
    openssl x509 -req -in "$mtls_dir/client.csr" -CA "$mtls_dir/ca.pem" \
        -CAkey "$mtls_dir/ca.key" -CAcreateserial -out "$mtls_dir/client.pem" \
        -days 1 -extfile <(printf "extendedKeyUsage=clientAuth") 2>/dev/null

    # Independent CA + leaf for the wrong-issuer negative case.
    openssl ecparam -name prime256v1 -genkey -noout -out "$mtls_dir/wrong-ca.key" 2>/dev/null
    openssl req -new -x509 -key "$mtls_dir/wrong-ca.key" -out "$mtls_dir/wrong-ca.pem" \
        -days 1 -subj "/CN=wrong-mtls-ca" 2>/dev/null
    openssl ecparam -name prime256v1 -genkey -noout -out "$mtls_dir/wrong-client.key" 2>/dev/null
    openssl req -new -key "$mtls_dir/wrong-client.key" -out "$mtls_dir/wrong-client.csr" \
        -subj "/CN=wrong-client" 2>/dev/null
    openssl x509 -req -in "$mtls_dir/wrong-client.csr" -CA "$mtls_dir/wrong-ca.pem" \
        -CAkey "$mtls_dir/wrong-ca.key" -CAcreateserial -out "$mtls_dir/wrong-client.pem" \
        -days 1 -extfile <(printf "extendedKeyUsage=clientAuth") 2>/dev/null

    # Server cert/key auto-gen by the daemon; pin our CA for client verification.
    sed '/^http:/,/^[^ ]/{/^http:/d;/^  /d;}' "$TEST_CONFIG" > "$mtls_config"
    cat >> "$mtls_config" <<EOFMTLS
http:
  listen: "127.0.0.1:$HTTP_PORT"
  tls:
    cert_file: "$mtls_dir/server-cert.pem"
    key_file: "$mtls_dir/server-key.pem"
    client_ca_file: "$mtls_dir/ca.pem"
EOFMTLS

    NO_COLOR=1 RUST_LOG=info "$DAEMON_PATH" --config "$mtls_config" > "$mtls_log" 2>&1 &
    DAEMON_PID=$!
    local ready=0
    for _ in {1..30}; do
        if curl -ksf --cert "$mtls_dir/client.pem" --key "$mtls_dir/client.key" \
            "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 1
    done
    if [[ $ready -ne 1 ]]; then
        log_error "✗ mTLS daemon did not become ready with trusted client cert"
        tail -20 "$mtls_log"
        return 1
    fi

    # Negative: no client cert at all → handshake should fail.
    if curl -ksf "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
        log_error "✗ /health responded without any client cert (mTLS not enforced)"
        return 1
    fi

    # Negative: cert signed by an untrusted CA → handshake should fail.
    if curl -ksf --cert "$mtls_dir/wrong-client.pem" --key "$mtls_dir/wrong-client.key" \
        "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
        log_error "✗ /health responded with a wrong-CA client cert (CA pinning broken)"
        return 1
    fi

    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""

    log_info "✓ HTTPS mTLS enforcement OK (trusted/no-cert/wrong-CA = pass/refuse/refuse)"
    return 0
}

# Test: Multiple login cycles (connection stability)
test_connection_stability() {
    log_test "Testing connection stability (10 login + INQUIRY cycles)..."

    local failures=0
    for i in {1..10}; do
        if ! timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/0" > /dev/null 2>&1; then
            ((failures++))
            log_warn "  Cycle $i failed"
        fi
    done

    if [[ $failures -eq 0 ]]; then
        log_info "✓ All 10 login + INQUIRY cycles successful"
        return 0
    else
        log_error "✗ $failures/10 cycles failed"
        return 1
    fi
}

# Test: Check daemon logs for errors
test_audit_log_writes() {
    log_test "Testing audit log captures cartridge creation..."

    local audit_dir="${TEST_DIR}/data/audit"
    if [[ ! -d "$audit_dir" ]]; then
        log_error "✗ Audit dir not found at $audit_dir"
        return 1
    fi

    # The pre-daemon create_test_cartridges step ran before the daemon
    # started, so the CLI should have written one cartridge.create
    # entry per cartridge. Daemon start adds a daemon.start entry.
    local total_entries
    total_entries=$(find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | wc -l)
    if [[ $total_entries -lt 2 ]]; then
        log_error "✗ Expected >=2 audit entries, found $total_entries"
        log_error "  Audit dir contents:"
        ls -la "$audit_dir" 2>&1 | sed 's/^/    /'
        return 1
    fi

    if find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | grep -q '"op":"cartridge.create"'; then
        log_info "✓ cartridge.create entry present"
    else
        log_error "✗ No cartridge.create entry in audit log"
        return 1
    fi

    if find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | grep -q '"op":"daemon.start"'; then
        log_info "✓ daemon.start entry present"
    else
        log_error "✗ No daemon.start entry in audit log"
        return 1
    fi

    log_info "✓ Audit log captured $total_entries entries"
    return 0
}

test_audit_export() {
    log_test "Testing audit export JSONL format..."

    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" system audit export --format jsonl 2>/dev/null)
    if [[ -z "$out" ]]; then
        log_error "✗ audit export produced no output"
        return 1
    fi
    # Each line should be a JSON object with "seq" and "op" keys.
    if echo "$out" | head -1 | grep -q '"seq"'; then
        log_info "✓ audit export JSONL output looks well-formed"
        return 0
    else
        log_error "✗ audit export JSONL first line missing 'seq' key: $(echo "$out" | head -1)"
        return 1
    fi
}

test_audit_verify_chain_ok() {
    log_test "Testing audit verify reports a valid chain..."

    # Tamper-evident is the only audit mode, so audit verify always
    # walks the chain and exits 0 on success.
    "$CLI_PATH" --config "$TEST_CONFIG" system audit verify >/dev/null 2>&1
    local rc=$?
    if [[ $rc -eq 0 ]]; then
        log_info "✓ audit verify exited 0 (chain valid)"
        return 0
    else
        log_error "✗ Expected exit 0, got $rc"
        return 1
    fi
}

test_daemon_logs() {
    log_test "Checking daemon logs for unexpected errors..."

    local error_count
    if grep -q "ERROR" "${TEST_DIR}/daemon.log" 2>/dev/null; then
        error_count=$(grep -c "ERROR" "${TEST_DIR}/daemon.log")
    else
        error_count=0
    fi

    if [[ $error_count -eq 0 ]]; then
        log_info "✓ No unexpected errors in daemon logs"
        return 0
    else
        log_warn "⚠ Found $error_count ERROR entries in daemon logs"
        log_warn "Last 10 ERROR lines:"
        grep "ERROR" "${TEST_DIR}/daemon.log" | tail -10
        # Don't fail on this - just warn
        return 0
    fi
}

# Main test execution
main() {
    echo "========================================"
    echo "Thur VTL Smoke Test"
    echo "========================================"
    echo ""
    
    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    create_test_cartridges
    
    # Track test results
    local passed=0
    local failed=0
    
    # Run tests
    local tests=(
        "test_http_health"
        "test_discovery"
        "test_library_info"
        "test_create_cartridge"
        "test_list_cartridges"
        "test_at_rest_encryption"
        "test_drive_status"
        "test_http_sessions"
        "test_http_info"
        "test_connection_stability"
        "test_audit_log_writes"
        "test_audit_export"
        "test_audit_verify_chain_ok"
        "test_daemon_logs"
        "test_http_tls_auto_gen"
        "test_http_mtls"
    )
    
    echo ""
    echo "Running tests..."
    echo "----------------"
    
    for test in "${tests[@]}"; do
        if $test; then
            ((passed++))
        else
            ((failed++))
        fi
        echo ""
    done
    
    # Summary
    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total tests: $((passed + failed))"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""
    
    if [[ $failed -eq 0 ]]; then
        log_info "✓ All tests passed!"
        echo ""
        echo "Test artifacts:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data: ${TEST_DIR}/data"
        exit 0
    else
        log_error "✗ $failed test(s) failed"
        echo ""
        echo "Debug information:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data: ${TEST_DIR}/data"
        exit 1
    fi
}

# Run main
main
