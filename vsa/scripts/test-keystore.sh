#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvsa keystore round-trip test (per-backend integration)
#
# Same shape as test-fs-iscsi-storage.sh, but the moving part is the keystore
# backend instead of the storage backend. Pick a keystore entry by name
# and the script exercises:
#
#   1. wrap  (volume create --encrypt --keystore <name>) — the daemon
#      mints / wraps the DEK via the chosen backend, stamps the
#      manifest with keystore_backend + wrapped_dek.
#   2. unwrap (daemon restart) — discovery re-opens the volume,
#      which means the backend's unwrap call worked.
#   3. wrap-target move (volume key migrate --to local --purge-local)
#      — unwraps via the source backend, rewraps via `local`,
#      verifies the migrated volume re-opens cleanly.
#
# No iSCSI, no mkfs, no fixture: that's already covered by
# test-fs-iscsi-storage.sh / test-pipeline-layers.sh row 3. This script is
# narrowly the wrap/unwrap exercise so the per-backend run is fast
# and credential-light.
#
# Selection: set THURVSA_TEST_KEYSTORE to the name of an entry under
# `keystore.backends:` in your keystore-backends.yaml source file. The script
# copies that entry verbatim into the test config so the operator's
# auth shape (Static / Env / Profile / SP / SP-Env / SA-Key / ADC /
# Token / AppRole / ...) is exercised end-to-end.
#
# Refusals: none — every backend type is fair game including `local`.
#
# Cleanup: tears down the daemon + per-run /tmp dir on exit. The
# script never touches the operator-provisioned key material at the
# backend (CMK / KV key / CryptoKey / vault key); only per-volume
# state.
#
# Prerequisites:
#   - jq, yq (kislyuk/yq — the jq-based wrapper; yq parses the source
#     keystore-backends.yaml, jq parses daemon-written volume manifests)
#   - Backend-side: real CMK / KV key / CryptoKey (or `vault server
#     -dev` started by the operator and reachable at the address in
#     keystore-backends.yaml) — provisioned out-of-band by the
#     operator. For `local` no setup is needed.
#   - Backend-side credentials in env (same chain the daemon uses).
#
# Usage (invoke from repo root):
#   THURVSA_TEST_KEYSTORE=<name> ./vsa/scripts/test-keystore.sh [OPTIONS]
#
# NOTE on credentials: from a fresh checkout, drop your maintainer
# storage credentials into `$REPO/private/thur.env` (KEY=VAL per line,
# AWS_* / GOOGLE_* / AZURE_* / per-backend `auth: env` names) and
# your backend entries in `$REPO/private/keystore-backends.yaml`.
# Override the source path with THURVSA_SOURCE_KEYSTORES.
#
# Options:
#   --release         Use ./target/release/ binaries (default: ./target/debug/)
#   --daemon-path P   Override path to thurvsad binary
#   --cli-path P      Override path to thurvsa binary
#   --keep-data       Don't clean up local test data directory
#
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials if the file exists.
# Same convention as test-fs-iscsi-storage.sh: anything KEY=VAL in thur.env
# becomes exported for the daemon (which inherits our env). Skipped
# on packaged installs — operators put creds in /etc/thurvsa/thurvsa.env
# there, picked up by the systemd unit.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
SOURCE_KEYSTORES="${THURVSA_SOURCE_KEYSTORES:-${REPO_DIR}/private/keystore-backends.yaml}"
TEST_DIR="/tmp/thurvsa-test-keystore-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
DAEMON_PID=""
KEEP_DATA=0
KEYSTORE_TYPE=""

# A fallback `local` keystore entry the script ALWAYS adds alongside
# the operator's chosen backend, used as the --to target for the
# migrate exercise. Naming it explicitly so we don't collide if the
# operator already has a `local` entry pointing somewhere we don't
# want to touch.
FALLBACK_LOCAL_NAME="testkeystore-local-fallback"
TEST_KEYSTORE_NAME="testkeystore"
VOLUME_NAME="v-keystore-test"
# Second volume covering the --dek-source backend code path. Only
# awskms + vault honor the Backend variant (KMS GenerateDataKey,
# Vault transit/datakey/plaintext); local/azurekv/gcpkms silently
# collapse to Daemon. We still create the volume for the latter
# three to verify the collapse round-trips cleanly.
VOLUME_BACKEND_RNG="v-keystore-test-backendrng"

log_pass()  { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC} $*"; }

while [[ $# -gt 0 ]]; do
    case $1 in
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

cleanup() {
    local rc=$?
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

resolve_keystore() {
    if [[ -z "${THURVSA_TEST_KEYSTORE:-}" ]]; then
        log_error "THURVSA_TEST_KEYSTORE is not set."
        echo "Set it to the name of an entry in $SOURCE_KEYSTORES"
        echo "Example: THURVSA_TEST_KEYSTORE=kms-prod $0"
        exit 1
    fi
    if [[ ! -r "$SOURCE_KEYSTORES" ]]; then
        log_error "Cannot read source keystore file: $SOURCE_KEYSTORES"
        echo "Override with THURVSA_SOURCE_KEYSTORES=<path>/keystore-backends.yaml"
        exit 1
    fi
    for tool in yq jq; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            log_error "$tool is required (yq parses $SOURCE_KEYSTORES, jq parses volume manifests)"
            exit 1
        fi
    done
    local exists
    exists=$(yq -r ".keystore.backends.\"$THURVSA_TEST_KEYSTORE\" // \"__missing__\"" "$SOURCE_KEYSTORES")
    if [[ "$exists" == "__missing__" || "$exists" == "null" ]]; then
        log_error "Keystore '$THURVSA_TEST_KEYSTORE' not found in $SOURCE_KEYSTORES"
        echo "Available keystores:"
        yq -r '.keystore.backends | keys | .[]' "$SOURCE_KEYSTORES" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi
    KEYSTORE_TYPE=$(yq -r ".keystore.backends.\"$THURVSA_TEST_KEYSTORE\".type" "$SOURCE_KEYSTORES")
    log_info "Source keystore:   $THURVSA_TEST_KEYSTORE (type=$KEYSTORE_TYPE)"
}

check_prerequisites() {
    require_daemon_binaries thurvsa
    log_info "Binaries:          daemon=$DAEMON_PATH, cli=$CLI_PATH"
}

create_test_config() {
    log_info "Creating test config under $TEST_DIR ..."
    mkdir -p "$TEST_DIR/data"

    # Splice the operator's chosen entry under TEST_KEYSTORE_NAME, plus
    # a `local`-typed fallback for the migrate exercise. JSON is valid
    # YAML so each entry inlines straight under `keystore.backends:`.
    local keystore_entry_json
    keystore_entry_json=$(yq -c \
        ".keystore.backends.\"$THURVSA_TEST_KEYSTORE\"" "$SOURCE_KEYSTORES")

    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"
http:
  listen: "127.0.0.1:$HTTP_PORT"
iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
disk_cache:
  disk_free_min_gb: 0

# A single local-filesystem backend. The keystore test isn't testing
# the storage-backend path — pointing at /tmp keeps it self-contained.
storage:
  backends:
    testbackend:
      type: local
      root_dir: "$TEST_DIR/storage"

keystore:
  backends:
    $TEST_KEYSTORE_NAME: $keystore_entry_json
    $FALLBACK_LOCAL_NAME: { type: local }
EOFCONFIG

    mkdir -p "$TEST_DIR/storage"
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" \
        >> "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            log_info "Daemon ready (PID $DAEMON_PID)"
            return 0
        fi
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            log_error "Daemon died at boot — last 30 lines of log:"
            tail -30 "${TEST_DIR}/daemon.log" | sed 's/^/  /'
            exit 1
        fi
        sleep 1
    done
    log_error "Daemon did not become ready in 30 s"
    tail -30 "${TEST_DIR}/daemon.log" | sed 's/^/  /'
    exit 1
}

stop_daemon() {
    stop_thur_daemon
}

manifest_path() {
    local name="${1:-$VOLUME_NAME}"
    echo "$TEST_DIR/data/volumes/$name/manifest.json"
}

assert_manifest_after_create() {
    local name="${1:-$VOLUME_NAME}"
    local m; m=$(manifest_path "$name")
    [[ -r "$m" ]] || { log_error "manifest not at $m"; exit 1; }
    local algo ks wrapped uuid
    algo=$(jq -r '.encryption.algorithm // ""' "$m")
    ks=$(jq -r '.encryption.keystore_backend // ""' "$m")
    wrapped=$(jq -r '.encryption.wrapped_dek // ""' "$m")
    uuid=$(jq -r '.uuid // ""' "$m")

    # serde's `rename_all = "snake_case"` doesn't split on digit
    # boundaries, so `Aes256Gcm` serializes as `aes256_gcm` even
    # though the parallel `as_str()` helper returns "aes_256_gcm".
    [[ "$algo" == "aes256_gcm" ]] \
        || { log_error "expected encryption.algorithm=aes256_gcm, got '$algo'"; exit 1; }
    [[ "$ks" == "$TEST_KEYSTORE_NAME" ]] \
        || { log_error "expected encryption.keystore_backend='$TEST_KEYSTORE_NAME', got '$ks'"; exit 1; }

    case "$KEYSTORE_TYPE" in
        local)
            [[ -z "$wrapped" ]] \
                || { log_error "local backend: expected wrapped_dek absent, got '$wrapped'"; exit 1; }
            local keyfile="$TEST_DIR/data/keys/${uuid}.key"
            [[ -f "$keyfile" ]] \
                || { log_error "local backend: sidecar missing at $keyfile"; exit 1; }
            local mode; mode=$(stat -c %a "$keyfile")
            [[ "$mode" == "600" ]] \
                || { log_error "local sidecar mode=$mode, expected 600"; exit 1; }
            ;;
        awskms|gcpkms|vault)
            [[ -n "$wrapped" ]] \
                || { log_error "$KEYSTORE_TYPE: wrapped_dek empty in manifest"; exit 1; }
            ;;
        azurekv)
            [[ -n "$wrapped" ]] \
                || { log_error "azurekv: wrapped_dek empty in manifest"; exit 1; }
            # Envelope schema (see shared/keystore/src/azurekv.rs):
            # base64(JSON({v:1, uuid:<hex>, ct:<base64>})).
            local env_uuid env_v
            env_v=$(printf '%s' "$wrapped" | base64 -d 2>/dev/null | jq -r '.v // ""' 2>/dev/null)
            env_uuid=$(printf '%s' "$wrapped" | base64 -d 2>/dev/null | jq -r '.uuid // ""' 2>/dev/null)
            [[ "$env_v" == "1" ]] \
                || { log_error "azurekv envelope: expected v=1, got '$env_v'"; exit 1; }
            [[ "$env_uuid" == "$uuid" ]] \
                || { log_error "azurekv envelope uuid '$env_uuid' != manifest uuid '$uuid'"; exit 1; }
            ;;
        *)
            log_warn "unknown keystore type '$KEYSTORE_TYPE'; manifest shape not asserted"
            ;;
    esac
    log_info "manifest OK: algorithm=$algo, keystore_backend=$ks, wrapped_dek=$([[ -n "$wrapped" ]] && echo "<${#wrapped} chars>" || echo "<none>")"
}

assert_volume_attached() {
    local name="${1:-$VOLUME_NAME}"
    # `volume info NAME` over the admin socket requires the daemon
    # to have successfully unwrapped the DEK at discovery — otherwise
    # the volume refuses to attach and the call 404s.
    if ! "$CLI_PATH" --config "$TEST_CONFIG" volume info "$name" --json >/dev/null 2>&1; then
        log_error "volume info '$name' failed — unwrap likely refused. Daemon log tail:"
        tail -20 "${TEST_DIR}/daemon.log" | sed 's/^/  /'
        exit 1
    fi
    log_info "volume '$name' attached cleanly (= unwrap round-trip OK)"
}

# Step 1: create encrypted volume + verify wrap shape (default
# --dek-source daemon: OsRng on the daemon, then backend wraps).
phase_create() {
    log_test "Phase 1: create encrypted volume (--dek-source daemon, default)"
    start_daemon
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size 32M --backend testbackend --dedup local \
        --encrypt --keystore "$TEST_KEYSTORE_NAME" >/dev/null \
        || { log_error "volume create failed"; exit 1; }
    assert_manifest_after_create "$VOLUME_NAME"
    assert_volume_attached "$VOLUME_NAME"
    log_pass "Phase 1: wrap + manifest shape verified (daemon-side RNG)"
}

# Step 1B: same shape but --dek-source backend. For awskms /
# vault this exercises a different code path (`kms:GenerateDataKey`
# / `transit/datakey/plaintext` — one round-trip, HSM-grade RNG).
# For local / azurekv / gcpkms the daemon silently collapses
# Backend → Daemon (no remote RNG primitive available); the test
# still creates the volume to confirm the collapse round-trips
# cleanly.
phase_create_backend_rng() {
    local label
    case "$KEYSTORE_TYPE" in
        awskms) label="(kms:GenerateDataKey)" ;;
        vault)  label="(transit/datakey/plaintext)" ;;
        local|azurekv|gcpkms)
            label="(collapses to daemon — no backend RNG primitive)" ;;
        *) label="" ;;
    esac
    log_test "Phase 1B: create second volume with --dek-source backend $label"
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_BACKEND_RNG" \
        --size 32M --backend testbackend --dedup local \
        --encrypt --keystore "$TEST_KEYSTORE_NAME" --dek-source backend >/dev/null \
        || { log_error "volume create --dek-source backend failed"; exit 1; }
    assert_manifest_after_create "$VOLUME_BACKEND_RNG"
    assert_volume_attached "$VOLUME_BACKEND_RNG"
    log_pass "Phase 1B: backend-side RNG path round-trips"
}

# Step 2: daemon restart -> unwrap via the chosen backend. Both
# volumes (daemon-RNG and backend-RNG) must re-attach.
phase_restart_unwrap() {
    log_test "Phase 2: daemon restart -> unwrap both volumes via $TEST_KEYSTORE_NAME"
    stop_daemon
    start_daemon
    assert_volume_attached "$VOLUME_NAME"
    assert_volume_attached "$VOLUME_BACKEND_RNG"
    log_pass "Phase 2: unwrap round-trip verified for both DEK sources"
}

# Step 3: migrate wrap-target to the local fallback, verify both
# unwrap (from chosen backend) and wrap (to local) work.
#
# Skipped when the source backend is itself `local`: both source and
# destination would share the same `<data_dir>/keys/<uuid>.key`
# path and the destination's no-clobber wrap would (correctly)
# refuse. Local-to-local migration is a degenerate no-op anyway —
# nothing's actually moving.
phase_migrate() {
    if [[ "$KEYSTORE_TYPE" == "local" ]]; then
        log_info "Phase 3: skipped (source type=local; local->local-fallback shares sidecar path)"
        return 0
    fi
    log_test "Phase 3: volume key migrate $TEST_KEYSTORE_NAME -> $FALLBACK_LOCAL_NAME (daemon-up)"
    # manifest.json is creation-frozen; the daemon only mutates
    # runtime.json on the hot path, so migrate runs daemon-up safely.
    "$CLI_PATH" --config "$TEST_CONFIG" volume key migrate "$VOLUME_NAME" \
        --to "$FALLBACK_LOCAL_NAME" --purge-local >/dev/null \
        || { log_error "volume key migrate failed"; exit 1; }
    local m; m=$(manifest_path "$VOLUME_NAME")
    local ks; ks=$(jq -r '.encryption.keystore_backend' "$m")
    [[ "$ks" == "$FALLBACK_LOCAL_NAME" ]] \
        || { log_error "post-migrate keystore_backend='$ks', expected '$FALLBACK_LOCAL_NAME'"; exit 1; }
    local uuid; uuid=$(jq -r '.uuid' "$m")
    local sidecar="$TEST_DIR/data/keys/${uuid}.key"
    [[ -f "$sidecar" ]] \
        || { log_error "migrate: local fallback sidecar missing at $sidecar"; exit 1; }
    # Restart to pick up the new keystore binding (the in-memory
    # writer still holds the old wrapped-DEK reference).
    stop_daemon
    start_daemon
    assert_volume_attached "$VOLUME_NAME"
    log_pass "Phase 3: daemon-up migrate + post-restart unwrap verified"
}

# Step 4: passphrase-sealed DEK export/import round-trip.
#
# Exports the DEK to a /tmp file (JWE/PBES2), simulates keystore
# loss (rm local sidecar OR clear manifest.encryption.wrapped_dek
# for external backends), then imports back and verifies the volume
# re-attaches. Also exercises the refusal paths: existing-file
# export, working-DEK import, wrong passphrase, cross-UUID misuse.
#
# Uses --iter 100000 (MIN_P2C floor) to keep PBKDF2 cost low; prod
# uses 600 000.
phase_export_import() {
    log_test "Phase 4: passphrase-sealed DEK export/import (JWE/PBES2)"
    local export_path="${TEST_DIR}/exported.jwe"
    local passphrase="correct horse battery staple test"

    # Current keystore (changes across runs depending on phase 3 path).
    local effective_ks
    effective_ks=$(jq -r '.encryption.keystore_backend' "$(manifest_path "$VOLUME_NAME")")
    log_info "Phase 4: volume currently on keystore '$effective_ks'"

    # 4a. Export. Daemon-up: export reads manifest + unwraps via
    # backend; manifest is creation-frozen so no race with the daemon.
    THURVSA_PASSPHRASE="$passphrase" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key export "$VOLUME_NAME" \
        --to "$export_path" --iter 100000 >/dev/null \
        || { log_error "volume key export failed"; exit 1; }

    local mode; mode=$(stat -c %a "$export_path")
    [[ "$mode" == "600" ]] \
        || { log_error "export file mode=$mode, expected 600"; exit 1; }

    local seg_count; seg_count=$(awk -F'.' '{print NF}' < "$export_path")
    [[ "$seg_count" == "5" ]] \
        || { log_error "exported JWE has $seg_count segments, expected 5"; exit 1; }
    log_info "exported $(wc -c < "$export_path") bytes, 5-segment JWE, mode 0600"

    # 4b. Refusal: re-export to existing file.
    if THURVSA_PASSPHRASE="$passphrase" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key export "$VOLUME_NAME" \
        --to "$export_path" --iter 100000 >/dev/null 2>&1; then
        log_error "re-export to existing file should have failed"
        exit 1
    fi
    log_info "refusal: existing-file export refused"

    # 4c. Refusal: import while the current keystore still unwraps.
    if THURVSA_PASSPHRASE="$passphrase" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key import "$VOLUME_NAME" \
        --from "$export_path" --keystore "$effective_ks" >/dev/null 2>&1; then
        log_error "import should have refused (working DEK already present)"
        exit 1
    fi
    log_info "refusal: import-over-working-keystore refused"

    # 4d. Break the wrap — daemon-down. Local sidecar removal for
    # local-backed, manifest field clear for external-backed.
    stop_daemon
    local m; m=$(manifest_path "$VOLUME_NAME")
    local uuid; uuid=$(jq -r '.uuid' "$m")
    local sidecar="$TEST_DIR/data/keys/${uuid}.key"
    if [[ -f "$sidecar" ]]; then
        rm -f "$sidecar"
        log_info "removed sidecar at $sidecar to simulate keystore loss"
    else
        jq 'del(.encryption.wrapped_dek)' "$m" > "${m}.tmp" && mv "${m}.tmp" "$m"
        log_info "cleared manifest.encryption.wrapped_dek to simulate keystore loss"
    fi

    # 4e. Refusal: wrong passphrase.
    if THURVSA_PASSPHRASE="wrong-passphrase-for-test" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key import "$VOLUME_NAME" \
        --from "$export_path" --keystore "$effective_ks" >/dev/null 2>&1; then
        log_error "import should have refused with wrong passphrase"
        exit 1
    fi
    log_info "refusal: wrong-passphrase import refused"

    # 4f. Refusal: cross-volume UUID misuse — try importing this
    # envelope into the *other* volume created in phase 1B.
    if THURVSA_PASSPHRASE="$passphrase" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key import "$VOLUME_BACKEND_RNG" \
        --from "$export_path" --keystore "$effective_ks" >/dev/null 2>&1; then
        log_error "import should have refused cross-UUID misuse"
        exit 1
    fi
    log_info "refusal: cross-UUID import refused"

    # 4g. Successful import + restart + attach.
    THURVSA_PASSPHRASE="$passphrase" \
        "$CLI_PATH" --config "$TEST_CONFIG" volume key import "$VOLUME_NAME" \
        --from "$export_path" --keystore "$effective_ks" >/dev/null \
        || { log_error "volume key import failed"; exit 1; }
    start_daemon
    assert_volume_attached "$VOLUME_NAME"
    log_pass "Phase 4: export -> simulated keystore loss -> import round-trip verified"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

assign_ports
resolve_keystore
check_prerequisites
create_test_config

phase_create
phase_create_backend_rng
phase_restart_unwrap
phase_migrate
phase_export_import

echo ""
log_pass "keystore '$THURVSA_TEST_KEYSTORE' (type=$KEYSTORE_TYPE): all phases passed"
exit 0
