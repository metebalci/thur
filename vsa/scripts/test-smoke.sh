#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa Smoke Test
#
# Lightweight checks against thurvsad's management surface — HTTP
# /health + /metrics, the thurvsa admin-socket round-trip (volume
# create / list / info / destroy), and the iSCSI login surface
# (libiscsi INQUIRY against the thurvsa IQN). Does NOT exercise the
# SCSI data path (no WRITE / READ / CAW); for that see
# test-scsi-conformance.sh.
#
# No sudo / no kernel iSCSI initiator required: the iSCSI assertion
# uses libiscsi's userspace iscsi-inq, the admin-socket talks over a
# per-test Unix socket under /tmp.
#
# Companions:
#   - test-proto-iscsi.sh — iSCSI protocol layer (login + INQUIRY) via libiscsi
#   - test-scsi-conformance.sh  — full SBC conformance via sg3_utils (sudo)
#   - test-fs.sh — end-to-end mkfs+mount+tar (sudo; --transport iscsi|nvmetcp)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-smoke.sh [OPTIONS]
#
# Options:
#   --debug               Use ./target/debug/ binaries (default: ./target/release/)
#   --daemon-path PATH    Path to thurvsad binary (overrides default)
#   --cli-path PATH       Path to thurvsa binary (overrides default)
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port (default: free ephemeral port)
#   --http-port PORT      Override HTTP port (default: free ephemeral port)
#

# Note: We don't use 'set -e' because we want to run all tests even if some fail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

# Configuration
TEST_DIR="/tmp/thurvsa-test-smoke-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
# Web-admin password (#4) the gate test sets; /sessions + /info are
# gated behind it once configured.
ADMIN_PW="smoke-admin-pass-1"

init_common_daemon_args
parse_common_daemon_args "$@"

cleanup() {
    standard_cleanup
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."

    require_daemon_binaries thurvsa

    if ! command -v iscsi-inq &>/dev/null; then
        log_error "iscsi-inq not found. Install with: sudo apt-get install libiscsi-bin"
        exit 1
    fi
    if ! command -v curl &>/dev/null; then
        log_error "curl not found. Install with: sudo apt-get install curl"
        exit 1
    fi
    if ! command -v openssl &>/dev/null; then
        log_error "openssl not found. Install with: sudo apt-get install openssl"
        exit 1
    fi

    log_info "All prerequisites met"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data/volumes"

    cat > "$TEST_CONFIG" <<EOFCONFIG
$(yaml_header Password)

$(yaml_iscsi)

$(yaml_local_backend)
EOFCONFIG

    mkdir -p "$TEST_DIR/data"
    log_info "Test config created at: $TEST_CONFIG"
}

start_daemon() {
    # Per-test admin socket so we don't need write access to /run/thurvsa/.
    # Both daemon and CLI honor THURVSA_ADMIN_SOCKET; export once for
    # downstream CLI invocations.
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    start_thur_daemon
}

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

test_http_health() {
    log_test "Testing HTTP /health endpoint..."
    local response
    response=$(curl -s "http://127.0.0.1:$HTTP_PORT/health")
    if echo "$response" | grep -q '"status":"ok"' && \
       echo "$response" | grep -q '"daemon":"thurvsa"'; then
        log_info "Health check passed: $response"
        return 0
    else
        log_error "Health check failed. Response: $response"
        return 1
    fi
}

# Web-admin password gate (#4): before a password is set /sessions
# fails closed (503); after `system set-admin-password` it needs HTTP
# Basic (401 without, 200 with), while /metrics + /health stay open.
test_http_admin_password_gate() {
    log_test "Testing web-admin password gate..."

    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT/sessions")
    if [ "$code" != "503" ]; then
        log_error "/sessions returned $code before a password was set (expected 503)"
        return 1
    fi

    local out
    out=$(THURVSA_ADMIN_PASSWORD="$ADMIN_PW" "$CLI_PATH" --config "$TEST_CONFIG" \
        system set-admin-password 2>&1)
    if ! echo "$out" | grep -q "web-admin password set"; then
        log_error "set-admin-password failed: $out"
        return 1
    fi

    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT/sessions")
    if [ "$code" != "401" ]; then
        log_error "/sessions without creds returned $code (expected 401)"
        return 1
    fi

    code=$(curl -s -o /dev/null -w "%{http_code}" -u "webadmin:$ADMIN_PW" \
        "http://127.0.0.1:$HTTP_PORT/sessions")
    if [ "$code" != "200" ]; then
        log_error "/sessions with valid creds returned $code (expected 200)"
        return 1
    fi

    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT/metrics")
    if [ "$code" != "200" ]; then
        log_error "/metrics returned $code without creds (expected 200 — must stay open)"
        return 1
    fi

    log_info "Admin-password gate: 503 unset, 401 no creds, 200 with creds, /metrics open"
    return 0
}

test_http_metrics() {
    log_test "Testing HTTP /metrics endpoint..."
    local response
    response=$(curl -s "http://127.0.0.1:$HTTP_PORT/metrics")
    # thurvsa shares the `thur_*` instrument namespace with thurvtld
    # (one shared-telemetry surface for the product line); the
    # `service.name=thurvsa` resource attribute distinguishes them.
    # Either pattern proves the Prometheus exporter rendered.
    if echo "$response" | grep -qE '(^thur_|service_name="thurvsa")'; then
        log_info "Metrics endpoint returned Prometheus text (thur_* / service.name)"
        return 0
    else
        log_error "Metrics endpoint missing expected series. First 5 lines:"
        echo "$response" | head -5 | sed 's/^/    /'
        return 1
    fi
}

# Test: read-only Web UI (#5).
#
# Runs after the admin-password gate (ADMIN_PW set) and a volume create.
# Asserts the static /ui bundle (embedded — no asset_dir in the smoke
# config) and the read-only /api/v1/volumes probe are both behind the
# gate: 401 without creds, 200 with. Also checks the served
# Content-Type.
test_webui() {
    log_test "Testing read-only Web UI..."

    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT/ui/")
    if [ "$code" != "401" ]; then
        log_error "/ui/ should be 401 without creds, got $code"
        return 1
    fi
    local index
    index=$(curl -s -u "webadmin:$ADMIN_PW" "http://127.0.0.1:$HTTP_PORT/ui/")
    if ! echo "$index" | grep -q "data-product"; then
        log_error "/ui/ did not serve index.html. Response head: $(echo "$index" | head -c 120)"
        return 1
    fi

    local ctype
    ctype=$(curl -s -D - -o /dev/null -u "webadmin:$ADMIN_PW" \
        "http://127.0.0.1:$HTTP_PORT/ui/app.css" | grep -i "^content-type:")
    if ! echo "$ctype" | grep -qi "text/css"; then
        log_error "/ui/app.css wrong content-type: $ctype"
        return 1
    fi

    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT/api/v1/volumes")
    if [ "$code" != "401" ]; then
        log_error "/api/v1/volumes should be 401 without creds, got $code"
        return 1
    fi
    code=$(curl -s -o /dev/null -w "%{http_code}" -u "webadmin:$ADMIN_PW" \
        "http://127.0.0.1:$HTTP_PORT/api/v1/volumes")
    if [ "$code" != "200" ]; then
        log_error "/api/v1/volumes should be 200 with creds, got $code"
        return 1
    fi

    log_info "Web UI served /ui + read-only /api/v1 behind the gate"
    return 0
}

# Pre-start: no volumes => REPORT LUNS empty, so issue INQUIRY against
# LUN 0 only AFTER we've created a volume below.
test_volume_create() {
    log_test "Testing thurvsa volume create..."
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" volume create test-vol --size 16M 2>&1)
    if echo "$out" | grep -qE "^OK: volume 'test-vol' created"; then
        log_info "Volume created: $out"
        return 0
    else
        log_error "volume create failed. Output: $out"
        return 1
    fi
}

test_volume_list() {
    log_test "Testing thurvsa volume list..."
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" volume list 2>&1)
    if echo "$out" | grep -q "test-vol"; then
        log_info "volume list contains test-vol"
        return 0
    else
        log_error "test-vol not in volume list. Output: $out"
        return 1
    fi
}

test_volume_info() {
    log_test "Testing thurvsa volume info..."
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" volume info test-vol --json 2>&1)
    if echo "$out" | grep -q '"name"' && echo "$out" | grep -q 'test-vol'; then
        log_info "volume info returned JSON manifest"
        return 0
    else
        log_error "volume info failed. Output: $out"
        return 1
    fi
}

# iscsi-inq exercises login (CmdSN/StatSN) + INQUIRY in one shot against
# LUN 0 — proves the iSCSI surface is alive and the volume registered.
# thurvsa identifies as DIRECT_ACCESS (PDT 0x00).
test_iscsi_inquiry() {
    log_test "Testing iSCSI login + INQUIRY against LUN 0..."
    local out
    out=$(timeout 10 iscsi-inq "iscsi://127.0.0.1:$ISCSI_PORT/$TARGET_IQN/0" 2>&1)
    if echo "$out" | grep -qE "Peripheral Device Type:DIRECT_ACCESS(_BLOCK_DEVICE)?\b|Peripheral Device Type:DISK\b"; then
        log_info "iSCSI INQUIRY returned DIRECT_ACCESS / disk PDT"
        return 0
    elif echo "$out" | grep -qE "Vendor:MB\b|Product:THUR VSA\b"; then
        # libiscsi-bin print format may differ across versions. Vendor /
        # product strings are the next-best evidence the LUN responded.
        # Identity: vendor `MB`, product `THUR VSA` (2026-05-11
        # clean-slate rename).
        log_info "iSCSI INQUIRY returned MB / THUR VSA identity"
        return 0
    else
        log_error "iSCSI INQUIRY did not match expected output:"
        echo "$out" | head -10 | sed 's/^/    /'
        return 1
    fi
}

test_volume_destroy() {
    log_test "Testing thurvsa volume destroy..."
    local out
    out=$("$CLI_PATH" --config "$TEST_CONFIG" volume destroy test-vol --force 2>&1)
    if echo "$out" | grep -qE "^OK: volume 'test-vol' destroyed"; then
        log_info "Volume destroyed: $out"
    else
        log_error "volume destroy failed. Output: $out"
        return 1
    fi
    # And confirm it's gone from the list.
    local list
    list=$("$CLI_PATH" --config "$TEST_CONFIG" volume list 2>&1)
    if echo "$list" | grep -q "test-vol"; then
        log_error "test-vol still in volume list after destroy: $list"
        return 1
    fi
    return 0
}

test_audit_log_writes() {
    log_test "Testing audit log captures volume lifecycle..."
    local audit_dir="${TEST_DIR}/data/audit"
    if [[ ! -d "$audit_dir" ]]; then
        log_error "Audit dir not found at $audit_dir"
        return 1
    fi

    # We expect: daemon.start (boot) + volume.create + volume.destroy.
    if find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | grep -q '"op":"daemon.start"'; then
        log_info "daemon.start entry present"
    else
        log_error "No daemon.start entry in audit log"
        return 1
    fi

    if find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | grep -q '"op":"volume.create"'; then
        log_info "volume.create entry present"
    else
        log_warn "No volume.create entry in audit log (admin-socket emitter not yet wired?)"
    fi

    if find "$audit_dir" -name 'audit-*.jsonl' -exec cat {} + 2>/dev/null | grep -q '"op":"volume.destroy"'; then
        log_info "volume.destroy entry present"
    else
        log_warn "No volume.destroy entry in audit log"
    fi

    return 0
}

test_http_tls_auto_gen() {
    log_test "Testing HTTPS auto-gen + load-existing..."
    local tls_config="${TEST_DIR}/config-tls.yaml"
    local tls_dir="${TEST_DIR}/tls"
    local tls_log="${TEST_DIR}/daemon-tls.log"
    rm -rf "$tls_dir"
    mkdir -p "$tls_dir"

    if [[ -n "$DAEMON_PID" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi

    sed '/^http:/,/^[^ ]/{/^http:/d;/^  /d;}' "$TEST_CONFIG" > "$tls_config"
    cat >> "$tls_config" <<EOFTLS
http:
  listen: "127.0.0.1:$HTTP_PORT"
  tls:
    cert_file: "$tls_dir/cert.pem"
    key_file: "$tls_dir/key.pem"
    client_ca_file: ""
EOFTLS

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
        log_error "TLS daemon did not become ready"
        tail -20 "$tls_log"
        return 1
    fi

    if ! grep -q "self-signed cert generated" "$tls_log"; then
        log_error "expected WARN self-signed log line missing"
        tail -20 "$tls_log"
        return 1
    fi

    local cert_mode key_mode
    cert_mode=$(stat -c '%a' "$tls_dir/cert.pem" 2>/dev/null)
    key_mode=$(stat -c '%a' "$tls_dir/key.pem" 2>/dev/null)
    if [[ "$cert_mode" != "644" || "$key_mode" != "600" ]]; then
        log_error "wrong file modes: cert=$cert_mode key=$key_mode (want 644 / 600)"
        return 1
    fi

    local fp1
    fp1=$(grep -o 'fingerprint_sha256=[a-f0-9]\{64\}' "$tls_log" | head -1 | cut -d= -f2)
    if [[ -z "$fp1" ]]; then
        log_error "no fingerprint logged on first start"
        return 1
    fi

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
        log_error "TLS daemon did not restart"
        tail -20 "$tls_log"
        return 1
    fi
    if grep -q "self-signed cert generated" "$tls_log"; then
        log_error "second start regenerated cert (should have loaded existing)"
        return 1
    fi
    if ! grep -q "loaded existing cert/key" "$tls_log"; then
        log_error "expected 'loaded existing cert/key' log missing"
        tail -20 "$tls_log"
        return 1
    fi
    local fp2
    fp2=$(grep -o 'fingerprint_sha256=[a-f0-9]\{64\}' "$tls_log" | head -1 | cut -d= -f2)
    if [[ "$fp1" != "$fp2" ]]; then
        log_error "fingerprint changed across restart: $fp1 vs $fp2"
        return 1
    fi

    log_info "HTTPS auto-gen + load-existing OK (fp=${fp1:0:16}…)"
    return 0
}

# HTTPS mTLS: trusted client cert passes, no cert and wrong-CA cert
# are refused at the TLS handshake.
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
        log_error "mTLS daemon did not become ready with trusted client cert"
        tail -20 "$mtls_log"
        return 1
    fi

    if curl -ksf "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
        log_error "/health responded without any client cert (mTLS not enforced)"
        return 1
    fi

    if curl -ksf --cert "$mtls_dir/wrong-client.pem" --key "$mtls_dir/wrong-client.key" \
        "https://127.0.0.1:$HTTP_PORT/health" > /dev/null 2>&1; then
        log_error "/health responded with a wrong-CA client cert (CA pinning broken)"
        return 1
    fi

    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""

    log_info "HTTPS mTLS enforcement OK (trusted/no-cert/wrong-CA = pass/refuse/refuse)"
    return 0
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
        log_info "No unexpected errors in daemon logs"
        return 0
    else
        log_warn "Found $error_count ERROR entries in daemon logs"
        log_warn "Last 10 ERROR lines:"
        grep "ERROR" "${TEST_DIR}/daemon.log" | tail -10
        return 0  # warn only, don't fail
    fi
}

main() {
    echo "========================================"
    echo "thurvsa Smoke Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon

    local passed=0
    local failed=0

    # The order matters: create before INQUIRY/list/info/destroy, audit-log
    # check after the lifecycle ops have run.
    local tests=(
        "test_http_health"
        "test_http_metrics"
        "test_http_admin_password_gate"
        "test_volume_create"
        "test_volume_list"
        "test_volume_info"
        "test_webui"
        "test_iscsi_inquiry"
        "test_volume_destroy"
        "test_audit_log_writes"
        "test_daemon_logs"
        "test_http_tls_auto_gen"
        "test_http_mtls"
    )

    echo ""
    echo "Running tests..."
    echo "----------------"
    for test in "${tests[@]}"; do
        if $test; then
            passed=$((passed + 1))
        else
            failed=$((failed + 1))
        fi
        echo ""
    done

    echo "========================================"
    echo "Test Summary"
    echo "========================================"
    echo "Total tests: $((passed + failed))"
    echo "Passed: $passed"
    echo "Failed: $failed"
    echo ""

    if [[ $failed -eq 0 ]]; then
        log_info "All tests passed!"
        echo "Test artifacts:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data:  ${TEST_DIR}/data"
        exit 0
    else
        log_error "$failed test(s) failed"
        echo "Debug information:"
        echo "  - Daemon log: ${TEST_DIR}/daemon.log"
        echo "  - Test data:  ${TEST_DIR}/data"
        exit 1
    fi
}

main
