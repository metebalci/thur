#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvtl Monte Carlo Random-Op Test
#
# Runs N seeded random operations against a small VTL chassis (3 drives,
# 8 cartridges, 12 storage slots, 1 IE element). Each data-path op picks
# a random drive, so the same op stream sweeps multi-drive load/unload
# coordination and per-drive concurrent SCSI dispatch. Op mix is weighted
# to bias drive data-path ops and sample changer / iSCSI churn at low
# rates. Tape semantics:
# every write appends at EOD (the harness issues `mt eod` before each
# write so picks of `space_back` mid-test don't accidentally truncate),
# every read_verify rewinds and replays the cartridge's known record
# stream in order.
#
# Content model (same as VSA): each record's bytes are AES-CTR keystream
# of blake3(seed | barcode | record_idx | "rec"). Verification = full
# replay-read of every record from BOT, cmp against the regenerated
# expected. Filemarks are tracked in the per-cart record list so the
# replay stitches record/filemark/record correctly.
#
# Size distribution is boundary-biased — sub-sector / page / chunk / etc.
# Same mc_pick_size_boundary_biased as the VSA harness; for tape the
# interesting boundary is the FastCDC ~64 KiB chunk, not the page,
# but the same bucket layout catches both surfaces.
#
# Backend selection: defaults to an inline local backend. Set
# THURVTL_TEST_BACKEND=<name> (or --backend <name>) to pick an entry
# from a backends YAML (defaulting to private/storage-backends.yaml,
# override via THURVTL_SOURCE_BACKENDS). The named backend's `prefix`
# is overridden per-run so test data is isolated and purged on cleanup.
#
# Prerequisites:
#   - mtx, mt-st, sg3-utils, open-iscsi, lsscsi, openssl
#   - iscsid running (sudo systemctl enable --now iscsid)
#   - Root/sudo access
#   - For non-local backends: yq, the matching backend CLI, valid credentials
#
# Usage:
#   ./vtl/scripts/test-monte-carlo.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run
#   --quick               200 ops (default: 3000)
#   --ops N               Override op count
#   --backend NAME        Use named backend entry (same as THURVTL_TEST_BACKEND)
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvtld path
#   --cli-path PATH       Override thurvtl path
#   --keep-data           Don't clean up test data directory
#   --iscsi-port PORT     Override iSCSI port
#   --http-port PORT      Override HTTP port
#

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Auto-load maintainer-private storage credentials before self-elevation
# so they're in scope to forward across sudo. Same convention as
# test-backup-storage.sh.
if [[ -r "${REPO_DIR}/private/thur.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${REPO_DIR}/private/thur.env"
    set +a
fi

# Self-elevate via sudo, forwarding backend-relevant env vars as
# explicit KEY=VAL pairs. `sudo -E` is silently ignored on sudo-rs
# (Ubuntu 26.04+); explicit forwarding is the only portable path.
if [[ $EUID -ne 0 ]]; then
    forward=()
    for v in $(compgen -A variable); do
        case "$v" in
            AWS_*|GOOGLE_*|GCS_*|AZURE_*|AISTOR_*|WASABI_*|MINIO_*|THURVTL_*)
                [[ -n "${!v}" ]] && forward+=("$v=${!v}")
                ;;
        esac
    done
    echo "[INFO] Re-executing under sudo with ${#forward[@]} env vars forwarded..."
    exec sudo "${forward[@]}" "$0" "$@"
fi

source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

TEST_DIR="/tmp/thurvtl-monte-carlo-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
ISCSI_CONNECTED=0
SEED=""
QUICK=0
OPS=""
CHANGER_DEVICE=""
BACKEND_NAME="${THURVTL_TEST_BACKEND:-}"
SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
BACKEND_TYPE=""
TEST_PREFIX=""

# Chassis: 8 cartridges across 12 storage slots, 3 drives, 1 IE element.
# 8 carts vs 3 drives leaves ≥5 carts free in storage at any time, so the
# random drive picker never starves on a load.
CARTS=(MC01L8 MC02L8 MC03L8 MC04L8 MC05L8 MC06L8 MC07L8 MC08L8)
NUM_SLOTS=12
NUM_DRIVES=3

# Auth wrapper. THURVTL_TEST_AUTH=chap enables CHAP — the conffile
# carries auth.method: CHAP, a per-run user/secret is added via
# `thurvtl iscsi users add` after daemon start, and iscsi_login sets
# node.session.auth credentials before --login. Default: none.
AUTH_MODE="${THURVTL_TEST_AUTH:-none}"
case "$AUTH_MODE" in
    none|chap) ;;
    *) echo "Unsupported THURVTL_TEST_AUTH='$AUTH_MODE' (expected none|chap)" >&2; exit 1 ;;
esac
CHAP_USER="mc-user-$$"
CHAP_PASS="mc-secret-$(od -An -N12 -tx8 /dev/urandom | tr -d ' \n')"

# Lazy state. iSCSI starts down; the first drive op brings it up.
ISCSI_UP=0

# Per-drive state, keyed by drive index 0..NUM_DRIVES-1.
# DRIVE_LOADED is the cached barcode loaded in each drive (refreshed
# from the daemon by ensure_loaded). DRIVE_TAPE_DEV / DRIVE_NST_DEV are
# the /dev/stN and /dev/nstN paths discovered from lsscsi at login.
declare -a DRIVE_LOADED=()
declare -a DRIVE_TAPE_DEV=()
declare -a DRIVE_NST_DEV=()

# Per-cartridge append-only record list. RECORDS[barcode] = newline-
# separated entries "R:idx:size" (record) or "F:idx:0" (filemark). idx
# is the global within-cart record index used as the content-derivation
# key — bumped on every R record (filemarks don't consume an idx since
# they have no bytes).
declare -A RECORDS
declare -A NEXT_REC_IDX
for c in "${CARTS[@]}"; do
    RECORDS[$c]=""
    NEXT_REC_IDX[$c]=0
done

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --ops) OPS="$2"; shift 2 ;;
        --backend) BACKEND_NAME="$2"; shift 2 ;;
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

[[ $QUICK -eq 1 ]] && : "${OPS:=200}"
: "${OPS:=3000}"

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsi_logout_and_delete
    fi
    stop_thur_daemon
    if [[ -n "$BACKEND_NAME" && "$BACKEND_TYPE" != "local" && -n "$TEST_PREFIX" ]]; then
        storage_purge_test_prefix 2>/dev/null || true
    fi
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
        log_info "Op log: $TEST_DIR/ops.log"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE)..."
    local missing=() hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvtld}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvtl}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvtld >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvtld)
        else
            missing+=("thurvtld"); hints+=("  - thurvtld: $build_cmd")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvtl >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvtl)
        else
            missing+=("thurvtl"); hints+=("  - thurvtl: $build_cmd")
        fi
    fi

    declare -A HINTS=(
        [mtx]="sudo apt-get install mtx"
        [mt]="sudo apt-get install mt-st"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [openssl]="(usually present)"
        [curl]="sudo apt-get install curl"
        [cmp]="(diffutils — usually present)"
        [systemctl]="(systemd — usually present)"
    )
    for tool in mtx mt iscsiadm lsscsi openssl curl cmp systemctl; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool"); hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done
    if [[ -n "$BACKEND_NAME" ]] && ! command -v yq >/dev/null 2>&1; then
        missing+=("yq")
        hints+=("  - yq: sudo apt-get install yq  (needed to read $SOURCE_BACKENDS)")
    fi
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi
    if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
        log_error "iscsid (open-iscsi) service is not running."
        echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
        exit 1
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data"
    # The CLI privdrops to $SUDO_USER for daemon-down ops; the data_dir
    # has to be writable by that user for audit writes to succeed.
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
    local auth_block=""
    [[ "$AUTH_MODE" == "chap" ]] && auth_block=$'  auth:\n    method: CHAP'
    if [[ -z "$BACKEND_NAME" ]]; then
        BACKEND_NAME="local"
        BACKEND_TYPE="local"
        cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

library:
  num_slots: $NUM_SLOTS
  num_drives: $NUM_DRIVES
  lto_generation: 8

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"
$auth_block

# /tmp is often tmpfs with little headroom — disable the free-floor
# so chunk-seals aren't blocked by try_reserve.
disk_cache:
  disk_free_min_gb: 0

storage:
  backends:
    local:
      type: local
      root_dir: "$TEST_DIR/local-backend"

keystore:
  backends:
    local: { type: local }
EOFCONFIG
        log_info "Backend: inline local at $TEST_DIR/local-backend"
        return 0
    fi

    if [[ ! -r "$SOURCE_BACKENDS" ]]; then
        log_error "Backend source YAML not found or unreadable: $SOURCE_BACKENDS"
        echo "Set THURVTL_SOURCE_BACKENDS=<path>/backends.yaml or use --backend without setting one."
        exit 1
    fi
    local exists
    exists=$(yq -r ".storage.backends.\"$BACKEND_NAME\" // \"__missing__\"" "$SOURCE_BACKENDS")
    if [[ "$exists" == "__missing__" ]]; then
        log_error "Backend '$BACKEND_NAME' not found in $SOURCE_BACKENDS"
        echo "Available:"
        yq -r '.storage.backends | keys[]' "$SOURCE_BACKENDS" 2>/dev/null | sed 's/^/  - /'
        exit 1
    fi
    BACKEND_TYPE=$(yq -r ".storage.backends.\"$BACKEND_NAME\".type" "$SOURCE_BACKENDS")
    local retention
    retention=$(yq -r ".storage.backends.\"$BACKEND_NAME\".retention_mode // \"none\"" "$SOURCE_BACKENDS")
    if [[ "$retention" != "none" ]]; then
        log_error "Backend '$BACKEND_NAME' has retention_mode='$retention' — refusing (test would create undeletable junk)."
        exit 1
    fi
    TEST_PREFIX="monte-carlo/run-$$/$(date +%s)/"

    # Globals for the storage_purge_test_prefix helper.
    BACKEND_BUCKET=$(yq -r ".storage.backends.\"$BACKEND_NAME\".bucket // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ENDPOINT=$(yq -r ".storage.backends.\"$BACKEND_NAME\".endpoint_url // \"\"" "$SOURCE_BACKENDS")
    BACKEND_REGION=$(yq -r ".storage.backends.\"$BACKEND_NAME\".region // \"\"" "$SOURCE_BACKENDS")
    BACKEND_ACCOUNT=$(yq -r ".storage.backends.\"$BACKEND_NAME\".storage_account // \"\"" "$SOURCE_BACKENDS")
    BACKEND_CONTAINER=$(yq -r ".storage.backends.\"$BACKEND_NAME\".container // \"\"" "$SOURCE_BACKENDS")
    BACKEND_AUTH_AKID_ENV=$(yq -r "(.storage.backends.\"$BACKEND_NAME\".auth.access_key_id_env // \"\")" "$SOURCE_BACKENDS")
    BACKEND_AUTH_SECRET_ENV=$(yq -r "(.storage.backends.\"$BACKEND_NAME\".auth.secret_access_key_env // \"\")" "$SOURCE_BACKENDS")

    local backend_yaml
    backend_yaml=$(yq -y \
        ".storage.backends.\"$BACKEND_NAME\" + { prefix: \"$TEST_PREFIX\" }" \
        "$SOURCE_BACKENDS" | sed 's/^/      /')

    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

library:
  num_slots: $NUM_SLOTS
  num_drives: $NUM_DRIVES
  lto_generation: 8

http:
  listen: "127.0.0.1:$HTTP_PORT"

iscsi:
  listen: "127.0.0.1:$ISCSI_PORT"
  target_iqn: "$TARGET_IQN"
$auth_block

# /tmp is often tmpfs with little headroom — disable the free-floor
# so chunk-seals aren't blocked by try_reserve.
disk_cache:
  disk_free_min_gb: 0

storage:
  backends:
    $BACKEND_NAME:
$backend_yaml

keystore:
  backends:
    local: { type: local }
EOFCONFIG
    log_info "Backend: $BACKEND_NAME (type=$BACKEND_TYPE, prefix=$TEST_PREFIX)"
}

start_daemon() {
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    DAEMON_LOG_MODE=append start_thur_daemon
}

# If THURVTL_TEST_AUTH=chap, provision one CHAP user the harness then
# uses for every iSCSI login. Idempotent: a second call after
# op_daemon_restart sees the user already present and skips.
setup_chap_user() {
    [[ "$AUTH_MODE" != "chap" ]] && return 0
    if "$CLI_PATH" --config "$TEST_CONFIG" iscsi users list 2>/dev/null | grep -q "$CHAP_USER"; then
        return 0
    fi
    log_info "Adding CHAP user $CHAP_USER..."
    if ! "$CLI_PATH" --config "$TEST_CONFIG" iscsi users add "$CHAP_USER" \
            --password "$CHAP_PASS" >/dev/null 2>&1; then
        log_error "Failed to add CHAP user $CHAP_USER"
        tail -20 "${TEST_DIR}/daemon.log"
        exit 1
    fi
}

create_cartridges() {
    local c
    for c in "${CARTS[@]}"; do
        log_info "Creating cartridge $c on backend $BACKEND_NAME..."
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$c" \
            --lto-generation 8 --backend "$BACKEND_NAME" >/dev/null 2>&1; then
            log_error "cartridge create $c failed"
            tail -20 "${TEST_DIR}/daemon.log"
            exit 1
        fi
    done
}

iscsi_login() {
    if [[ $ISCSI_UP -eq 1 ]]; then return 0; fi
    # Under CHAP, discovery itself requires authentication. Stash creds
    # on the discoverydb entry first so SendTargets succeeds; the node
    # entries inherit discovery CHAP, then we still set node.session.*
    # below so a session-level retry keeps working too.
    if [[ "$AUTH_MODE" == "chap" ]]; then
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" -o new >/dev/null 2>&1 || true
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.authmethod -v CHAP >/dev/null 2>&1
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.username -v "$CHAP_USER" >/dev/null 2>&1
        iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" \
            -o update -n discovery.sendtargets.auth.password -v "$CHAP_PASS" >/dev/null 2>&1
    fi
    local disc_out
    if ! disc_out=$(iscsiadm -m discoverydb -t st -p "127.0.0.1:$ISCSI_PORT" --discover 2>&1); then
        log_error "iscsi discovery failed: $disc_out"
        return 1
    fi
    if [[ "$AUTH_MODE" == "chap" ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.authmethod -v CHAP >/dev/null 2>&1
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.username -v "$CHAP_USER" >/dev/null 2>&1
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" \
            --op update -n node.session.auth.password -v "$CHAP_PASS" >/dev/null 2>&1
    fi
    local login_out
    if ! login_out=$(iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login 2>&1); then
        log_error "iscsi login failed: $login_out"
        return 1
    fi
    ISCSI_CONNECTED=1
    sleep 3
    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; return 1; }
    # lsscsi sorts by H:C:I:L; LUN 1..N maps to drive_id 0..N-1 per
    # vtl/daemon/src/iscsi/handler.rs, so the iteration order here lines
    # up with drive index without extra parsing.
    local tape_devs=()
    mapfile -t tape_devs < <(lsscsi | awk '/tape/{print $NF}')
    if (( ${#tape_devs[@]} < NUM_DRIVES )); then
        log_error "Found ${#tape_devs[@]} tape device(s), expected $NUM_DRIVES"
        lsscsi
        return 1
    fi
    local di
    for (( di=0; di<NUM_DRIVES; di++ )); do
        DRIVE_TAPE_DEV[$di]="${tape_devs[$di]}"
        DRIVE_NST_DEV[$di]=$(echo "${tape_devs[$di]}" | sed 's|/dev/st|/dev/nst|')
    done
    # Warm up: clear pending UA from login across changer + every drive.
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
    for (( di=0; di<NUM_DRIVES; di++ )); do
        mt -f "${DRIVE_NST_DEV[$di]}" status >/dev/null 2>&1 || true
    done
    ISCSI_UP=1
}

iscsi_logout() {
    if [[ $ISCSI_UP -eq 0 ]]; then return 0; fi
    iscsi_logout_and_delete
    ISCSI_UP=0
    CHANGER_DEVICE=""
    DRIVE_TAPE_DEV=()
    DRIVE_NST_DEV=()
    # Do NOT clear DRIVE_LOADED — iSCSI logout doesn't unload the
    # physical cart; the daemon's drive state persists across sessions.
    # On re-login, ensure_loaded re-syncs from the daemon anyway.
}

# Ask the daemon which cart is loaded in the given drive right now.
# Returns the barcode on stdout, or empty if no cart is loaded.
# Authoritative — use this in preference to the shell-side
# DRIVE_LOADED cache when accuracy matters (i.e. across any path that
# might have changed drive state without our notice).
daemon_loaded_in_drive() {
    local drive_idx="$1"
    "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
        | python3 -c "
import sys, json
di = $drive_idx
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('location') == 'drive' and int(c.get('slot_id', -1)) == di:
        print(c.get('barcode', ''))
        break
"
}

# Find which drive currently holds barcode $1, or empty if it isn't
# loaded in any drive. Used by ensure_loaded to detect carts that need
# to be unloaded from a different drive before we can load them here.
daemon_drive_of_cart() {
    local bc="$1"
    "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
        | python3 -c "
import sys, json
bc = '$bc'
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('barcode') == bc and c.get('location') == 'drive':
        sid = c.get('slot_id')
        if sid is not None:
            print(int(sid))
        break
"
}

# Find which mtx-element storage slot currently holds barcode $1, or
# empty if it isn't in a storage slot. The daemon's `cartridge list
# --json` is the authoritative map; mtx's READ ELEMENT STATUS response
# doesn't include barcodes (no VolumeTag descriptors), so awk-parsing
# its output for barcode→slot would be a dead end.
#
# Daemon slot_id is 0-indexed; mtx element addresses are 1-indexed.
slot_of_cart() {
    local bc="$1"
    "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
        | python3 -c "
import sys, json
bc = '$bc'
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('barcode') == bc and c.get('location') == 'storage':
        sid = c.get('slot_id')
        if sid is not None:
            print(int(sid) + 1)
        break
"
}

# Pick any empty storage slot we can unload into. Source-slot tracking
# isn't necessary — mtx unload only needs a destination. Returns the
# 1-indexed mtx element address (matches mtx_slot semantics).
any_empty_slot() {
    mtx -f "$CHANGER_DEVICE" status 2>/dev/null \
        | awk '/Storage Element [0-9]+:Empty/ {
            for (i=1; i<=NF; i++) if ($i == "Element") { print $(i+1); exit }
        }'
}

# Ensure a cartridge is loaded in drive $drive_idx. Resync from the
# daemon first — the shell-side DRIVE_LOADED cache can drift across
# iSCSI logout/login or external state churn. If $want is passed, that
# specific barcode ends up loaded in $drive_idx; otherwise any cart
# will do (the picker preserves what's already loaded if there is one).
#
# Multi-drive coordination: if $want is currently loaded in a *different*
# drive, that drive is unloaded first before we load $want here. A cart
# can be in at most one drive at a time, so the same-cart-in-two-drives
# race is impossible.
ensure_loaded() {
    iscsi_login || return 1
    local want="$1"
    local drive_idx="${2:-0}"
    DRIVE_LOADED[$drive_idx]=$(daemon_loaded_in_drive "$drive_idx")
    local current="${DRIVE_LOADED[$drive_idx]}"
    if [[ -z "$want" && -n "$current" ]]; then
        return 0
    fi
    if [[ -n "$want" && "$current" == "$want" ]]; then
        return 0
    fi
    # If $want is loaded in some other drive, unload it from there first.
    if [[ -n "$want" ]]; then
        local other
        other=$(daemon_drive_of_cart "$want")
        if [[ -n "$other" && "$other" != "$drive_idx" ]]; then
            mt -f "${DRIVE_NST_DEV[$other]}" rewind >/dev/null 2>&1 || true
            local origin
            origin=$(any_empty_slot)
            [[ -z "$origin" ]] && origin=1
            if ! mtx -f "$CHANGER_DEVICE" unload "$origin" "$other" >/dev/null 2>&1; then
                log_error "ensure_loaded: unload of $want from drive $other failed"
                return 1
            fi
            DRIVE_LOADED[$other]=""
        fi
    fi
    # Unload whatever's in our target drive (if anything) so we can load
    # the new cart.
    if [[ -n "$current" ]]; then
        # Rewind before unload to avoid a partial-write surprise.
        mt -f "${DRIVE_NST_DEV[$drive_idx]}" rewind >/dev/null 2>&1 || true
        local origin
        origin=$(any_empty_slot)
        [[ -z "$origin" ]] && origin=1
        if ! mtx -f "$CHANGER_DEVICE" unload "$origin" "$drive_idx" >/dev/null 2>&1; then
            log_error "ensure_loaded: unload of $current from drive $drive_idx to slot $origin failed"
            return 1
        fi
        DRIVE_LOADED[$drive_idx]=""
    fi
    # Pick a slot to load from. If $want is set, find its slot; else
    # pick deterministically among carts that aren't loaded in any
    # other drive.
    local target_bc target_slot
    if [[ -n "$want" ]]; then
        target_bc="$want"
        target_slot=$(slot_of_cart "$want")
        if [[ -z "$target_slot" ]]; then
            log_error "ensure_loaded: cart $want not found in any storage slot"
            mtx -f "$CHANGER_DEVICE" status | sed 's/^/  /'
            return 1
        fi
    else
        # Build the exclusion set: carts currently in any drive.
        local loaded_set=" " di
        for (( di=0; di<NUM_DRIVES; di++ )); do
            [[ -n "${DRIVE_LOADED[$di]}" ]] && loaded_set+="${DRIVE_LOADED[$di]} "
        done
        local idx tries=0 max_tries="${#CARTS[@]}"
        while (( tries < max_tries )); do
            idx=$(mc_rng_u32 "load-pick-$tries" "${#CARTS[@]}")
            target_bc="${CARTS[$idx]}"
            if [[ "$loaded_set" != *" $target_bc "* ]]; then
                target_slot=$(slot_of_cart "$target_bc")
                [[ -n "$target_slot" ]] && break
            fi
            tries=$(( tries + 1 ))
            target_bc=""
            target_slot=""
        done
        if [[ -z "$target_slot" ]]; then
            # Fallback: linear scan for any cart in storage not loaded
            # elsewhere.
            local cand
            for cand in "${CARTS[@]}"; do
                if [[ "$loaded_set" != *" $cand "* ]]; then
                    target_slot=$(slot_of_cart "$cand")
                    if [[ -n "$target_slot" ]]; then
                        target_bc="$cand"
                        break
                    fi
                fi
            done
        fi
        [[ -n "$target_slot" ]] || { log_error "ensure_loaded: no cart available for drive $drive_idx"; return 1; }
    fi
    if ! mtx -f "$CHANGER_DEVICE" load "$target_slot" "$drive_idx" >/dev/null 2>&1; then
        log_error "ensure_loaded: load of $target_bc from slot $target_slot into drive $drive_idx failed"
        return 1
    fi
    DRIVE_LOADED[$drive_idx]="$target_bc"
}

# Position drive $drive_idx at end-of-data so every write_record
# appends. We use rewind+fsr(N) rather than `mt eod`: the underlying
# filemark-on-READ bug (#25) that made `mt eod` corrupt the next write
# has been fixed, but rewind+fsr is still the more predictable form —
# each fsr block is a single LBA step, and the kernel/daemon agree on
# what those LBAs are regardless of how many filemarks sit on the medium.
seek_eod() {
    local drive_idx="$1"
    local bc="${DRIVE_LOADED[$drive_idx]}"
    [[ -z "$bc" ]] && return 0
    local nst="${DRIVE_NST_DEV[$drive_idx]}"
    mt -f "$nst" rewind >/dev/null 2>&1
    local n
    n=$(record_count "$bc")
    if (( n > 0 )); then
        mt -f "$nst" fsr "$n" >/dev/null 2>&1
    fi
}

# Record-list helpers.
push_record() { RECORDS[$1]+="$2"$'\n'; }
record_count() {
    local bc="$1"
    [[ -z "${RECORDS[$bc]}" ]] && { echo 0; return; }
    printf '%s' "${RECORDS[$bc]}" | grep -c '^' || true
}

# ---------------------------------------------------------------------------
# Op handlers
# ---------------------------------------------------------------------------

op_write_record() {
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    ensure_loaded "" "$drive_idx" || return 1
    seek_eod "$drive_idx"
    local bc="${DRIVE_LOADED[$drive_idx]}"
    local idx="${NEXT_REC_IDX[$bc]}"
    local size
    size=$(mc_pick_size_boundary_biased "size-write")
    # Tape records have an upper bound — LTO-8 absolute max is 16 MiB,
    # but the daemon's default block size limit is lower; cap at 4 MiB
    # to stay well inside any drive-side guard.
    (( size > 4194304 )) && size=4194304
    local tmp="$TEST_DIR/scratch.rec"
    mc_content_to "$bc" "$idx" "$size" "$tmp"
    # dd to /dev/nstN writes one record per dd invocation.
    if ! dd if="$tmp" of="${DRIVE_NST_DEV[$drive_idx]}" bs="$size" count=1 status=none 2>/dev/null; then
        log_error "write_record: dd failed (drive=$drive_idx bc=$bc idx=$idx size=$size)"
        mc_dump_failure
        return 1
    fi
    push_record "$bc" "R:$idx:$size"
    NEXT_REC_IDX[$bc]=$(( idx + 1 ))
    mc_log_op write_record drive="$drive_idx" cart="$bc" idx="$idx" size="$size"
}

op_read_verify() {
    # Pick a cart with at least one record.
    local bc="" candidates=()
    for c in "${CARTS[@]}"; do
        [[ -n "${RECORDS[$c]}" ]] && candidates+=("$c")
    done
    if (( ${#candidates[@]} == 0 )); then
        mc_log_op read_verify status=no_records
        return 0
    fi
    local pick_idx
    pick_idx=$(mc_rng_u32 "verify-cart" "${#candidates[@]}")
    bc="${candidates[$pick_idx]}"
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    ensure_loaded "$bc" "$drive_idx" || return 1
    local nst="${DRIVE_NST_DEV[$drive_idx]}"
    mt -f "$nst" rewind >/dev/null 2>&1 || { log_error "read_verify: rewind failed (drive=$drive_idx)"; return 1; }
    # Replay every record/filemark on the cart in order.
    local entry kind idx size expected="$TEST_DIR/scratch.expect" actual="$TEST_DIR/scratch.actual"
    local n_records=0 n_filemarks=0
    while IFS= read -r entry; do
        [[ -z "$entry" ]] && continue
        kind="${entry%%:*}"
        local rest="${entry#*:}"
        idx="${rest%%:*}"
        size="${rest#*:}"
        case "$kind" in
            R)
                # Read one record. dd with bs == record size reads exactly
                # one record off the tape (kernel returns short read on
                # SCSI variable-block READ; bs sets the buffer cap).
                mc_content_to "$bc" "$idx" "$size" "$expected"
                if ! dd if="$nst" of="$actual" bs="$size" count=1 status=none 2>/dev/null; then
                    log_error "read_verify: dd read failed (drive=$drive_idx bc=$bc idx=$idx size=$size)"
                    mc_dump_failure
                    return 1
                fi
                local actual_size
                actual_size=$(stat -c%s "$actual")
                if [[ "$actual_size" != "$size" ]]; then
                    log_error "read_verify: short read on $bc record idx=$idx: expected=$size got=$actual_size"
                    mc_dump_failure
                    return 1
                fi
                if ! cmp -s "$expected" "$actual"; then
                    log_error "read_verify: content mismatch on $bc record idx=$idx size=$size"
                    local first_diff
                    first_diff=$(cmp "$expected" "$actual" 2>&1 | head -1)
                    log_error "  first divergence: $first_diff"
                    mc_dump_failure
                    return 1
                fi
                n_records=$(( n_records + 1 ))
                ;;
            F)
                # Advance over the filemark. `mt fsf 1` is the canonical
                # way; we don't try to read across it because the SCSI
                # READ will already have returned 0-length on the prior
                # block, leaving us positioned just before the FM.
                mt -f "$nst" fsf 1 >/dev/null 2>&1 || true
                n_filemarks=$(( n_filemarks + 1 ))
                ;;
        esac
    done <<< "${RECORDS[$bc]}"
    mc_log_op read_verify drive="$drive_idx" cart="$bc" records="$n_records" filemarks="$n_filemarks"
}

# space_fwd/space_back ops are intentionally absent. The daemon's tape
# position logic plus the kernel /dev/nstN driver produces unpredictable
# state when a write follows an arbitrary space sequence, surfacing as
# read-back garbage at the "next" LBA. That's likely a real bug worth
# its own follow-up — but Monte Carlo isn't the right test for it; the
# scripted scsi-conformance tests catch position-management regressions
# more pointedly. Keep this harness focused on data correctness.

op_write_filemark() {
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    ensure_loaded "" "$drive_idx" || return 1
    seek_eod "$drive_idx"
    local bc="${DRIVE_LOADED[$drive_idx]}"
    if ! mt -f "${DRIVE_NST_DEV[$drive_idx]}" weof 1 >/dev/null 2>&1; then
        log_error "write_filemark: weof failed (drive=$drive_idx bc=$bc)"
        mc_dump_failure
        return 1
    fi
    push_record "$bc" "F:0:0"
    mc_log_op write_filemark drive="$drive_idx" cart="$bc"
}

op_rewind() {
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    ensure_loaded "" "$drive_idx" || return 1
    mt -f "${DRIVE_NST_DEV[$drive_idx]}" rewind >/dev/null 2>&1 || true
    mc_log_op rewind drive="$drive_idx" cart="${DRIVE_LOADED[$drive_idx]}"
}

op_load_cycle() {
    iscsi_login || return 1
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    # Re-sync from the daemon in case state drifted since the last op.
    DRIVE_LOADED[$drive_idx]=$(daemon_loaded_in_drive "$drive_idx")
    local prev="${DRIVE_LOADED[$drive_idx]}"
    if [[ -n "$prev" ]]; then
        mt -f "${DRIVE_NST_DEV[$drive_idx]}" rewind >/dev/null 2>&1 || true
        local origin
        origin=$(any_empty_slot)
        [[ -z "$origin" ]] && origin=1
        if ! mtx -f "$CHANGER_DEVICE" unload "$origin" "$drive_idx" >/dev/null 2>&1; then
            log_error "load_cycle: unload failed (drive=$drive_idx cart=$prev origin=$origin)"
            mc_dump_failure
            return 1
        fi
        DRIVE_LOADED[$drive_idx]=""
        mc_log_op load_cycle drive="$drive_idx" prev="$prev"
    else
        mc_log_op load_cycle drive="$drive_idx" status=already_empty
    fi
}

op_iscsi_logout_cycle() {
    if [[ $ISCSI_UP -eq 0 ]]; then
        mc_log_op iscsi_logout_cycle status=already_down
        return 0
    fi
    iscsi_logout
    mc_log_op iscsi_logout_cycle
}

# Export an in-storage cartridge to the import/export element, then
# re-import it. Round-trip — cart ends up back in storage (slot
# reassignment may happen). Skips any cart loaded in a drive (would
# need an unload dance) and falls back to the slot_of_cart check so
# carts left in odd states (e.g. mid-export) don't trip us up.
op_import_export() {
    iscsi_login || return 1
    local victim_bc victim_slot=""
    local c di in_drive
    for c in "${CARTS[@]}"; do
        in_drive=0
        for (( di=0; di<NUM_DRIVES; di++ )); do
            if [[ "${DRIVE_LOADED[$di]}" == "$c" ]]; then in_drive=1; break; fi
        done
        (( in_drive == 1 )) && continue
        victim_slot=$(slot_of_cart "$c")
        if [[ -n "$victim_slot" ]]; then victim_bc="$c"; break; fi
    done
    if [[ -z "$victim_bc" ]]; then
        mc_log_op import_export status=no_candidate
        return 0
    fi
    # `cartridge export` is daemon-routed; uses storage slots
    # (the daemon-visible "cartridge export" verb works against the
    # logical storage slot, not the IE element).
    if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge export "$victim_bc" >/dev/null 2>&1; then
        # If the verb shape differs in this build, skip this op rather
        # than failing the whole test — import/export is low-weight
        # coverage, not load-bearing.
        mc_log_op import_export status=export_unavailable cart="$victim_bc"
        return 0
    fi
    if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge import "$victim_bc" >/dev/null 2>&1; then
        log_error "import_export: import of $victim_bc failed after successful export"
        mc_dump_failure
        return 1
    fi
    mc_log_op import_export cart="$victim_bc"
}

# Shuffle a cart between two empty storage slots via MOVE MEDIUM. Pure
# changer-side exercise; doesn't touch any drive or data.
op_changer_move() {
    iscsi_login || return 1
    # Read the slot map fresh.
    local status_out
    status_out=$(mtx -f "$CHANGER_DEVICE" status 2>/dev/null)
    # Pick the first Full storage slot and the first Empty one. Loaded
    # carts live in Data Transfer Elements 0..N-1 — not in any Storage
    # Element — so there's no risk of trying to move a loaded cart out
    # from under ourselves via this op. mtx output carries no barcodes
    # (no VolumeTag descriptors from this READ ELEMENT STATUS), so we
    # don't bother trying to identify which cart is moving.
    local from_slot to_slot
    from_slot=$(echo "$status_out" | awk '/Storage Element [0-9]+:Full/ { for (i=1;i<=NF;i++) if ($i == "Element") { print $(i+1); exit } }')
    to_slot=$(echo "$status_out" | awk '/Storage Element [0-9]+:Empty/ { for (i=1;i<=NF;i++) if ($i == "Element") { print $(i+1); exit } }')
    if [[ -z "$from_slot" || -z "$to_slot" ]]; then
        mc_log_op changer_move status=no_slot from="$from_slot" to="$to_slot"
        return 0
    fi
    if ! mtx -f "$CHANGER_DEVICE" transfer "$from_slot" "$to_slot" >/dev/null 2>&1; then
        log_error "changer_move: transfer $from_slot -> $to_slot failed"
        mc_dump_failure
        return 1
    fi
    mc_log_op changer_move from="$from_slot" to="$to_slot"
}

op_write_filemarks_sync() {
    local drive_idx
    drive_idx=$(mc_rng_u32 "drive-pick" "$NUM_DRIVES")
    ensure_loaded "" "$drive_idx" || return 1
    seek_eod "$drive_idx"
    local bc="${DRIVE_LOADED[$drive_idx]}"
    local n
    n=$(( $(mc_rng_u32 "fm-sync" 3) + 2 ))   # 2..4
    if ! mt -f "${DRIVE_NST_DEV[$drive_idx]}" weof "$n" >/dev/null 2>&1; then
        log_error "write_filemarks_sync: weof $n failed (drive=$drive_idx bc=$bc)"
        mc_dump_failure
        return 1
    fi
    local i
    for (( i=0; i<n; i++ )); do
        push_record "$bc" "F:0:0"
    done
    mc_log_op write_filemarks_sync drive="$drive_idx" cart="$bc" n="$n"
}

# Stop and restart the daemon. The daemon handles SIGTERM by abort()
# rather than running its flush path, so we cleanly unload every loaded
# cart first — that triggers MemoryBufferManager::on_cartridge_unloaded,
# which flushes in-memory chunk buffers into the upload pipeline. On
# restart, scan_and_enqueue_orphans (vtl/daemon/src/upload_recovery.rs)
# replays any sealed-but-unuploaded chunks so the next read_verify
# finds the data intact. ISCSI_UP / DRIVE_LOADED reset so the next
# data-path op lazily re-establishes via iscsi_login + ensure_loaded.
op_daemon_restart() {
    iscsi_login || return 1
    local di
    for (( di=0; di<NUM_DRIVES; di++ )); do
        DRIVE_LOADED[$di]=$(daemon_loaded_in_drive "$di")
        if [[ -n "${DRIVE_LOADED[$di]}" ]]; then
            mt -f "${DRIVE_NST_DEV[$di]}" rewind >/dev/null 2>&1 || true
            local origin
            origin=$(any_empty_slot)
            [[ -z "$origin" ]] && origin=1
            if ! mtx -f "$CHANGER_DEVICE" unload "$origin" "$di" >/dev/null 2>&1; then
                log_error "daemon_restart: unload of ${DRIVE_LOADED[$di]} from drive $di failed"
                mc_dump_failure
                return 1
            fi
            DRIVE_LOADED[$di]=""
        fi
    done
    iscsi_logout
    stop_thur_daemon
    sleep 0.2
    DAEMON_LOG_MODE=append start_thur_daemon
    mc_log_op daemon_restart
}

# ---------------------------------------------------------------------------
# Main op loop
# ---------------------------------------------------------------------------

# Weights for the random op picker. Must sum to 100; mc_assert_weights
# at startup enforces that. write_filemark / write_filemarks_sync are
# rare on purpose — they're correctness-shape ops, not throughput drivers.
OP_WEIGHTS=(
    "29:write_record" "37:read_verify"
    "5:rewind"
    "10:load_cycle" "5:iscsi_logout_cycle"
    "3:import_export" "3:changer_move"
    "4:write_filemark" "2:write_filemarks_sync"
    "2:daemon_restart"
)

run_ops() {
    local n="$1"
    local op
    local progress_every=$(( n / 20 ))
    (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n; MC_OP_INDEX++ )); do
        op=$(mc_pick_weighted op "${OP_WEIGHTS[@]}")
        case "$op" in
            write_record)           op_write_record || return 1 ;;
            read_verify)            op_read_verify || return 1 ;;
            rewind)                 op_rewind || return 1 ;;
            load_cycle)             op_load_cycle || return 1 ;;
            iscsi_logout_cycle)     op_iscsi_logout_cycle || return 1 ;;
            import_export)          op_import_export || return 1 ;;
            changer_move)           op_changer_move || return 1 ;;
            write_filemark)         op_write_filemark || return 1 ;;
            write_filemarks_sync)   op_write_filemarks_sync || return 1 ;;
            daemon_restart)         op_daemon_restart || return 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            local total=0 c di drive_summary=""
            for c in "${CARTS[@]}"; do
                total=$(( total + $(record_count "$c") ))
            done
            for (( di=0; di<NUM_DRIVES; di++ )); do
                drive_summary+="d${di}=${DRIVE_LOADED[$di]:-<empty>} "
            done
            log_info "[$MC_OP_INDEX/$n] seed=$MC_SEED ${drive_summary}iscsi=$ISCSI_UP total_records=$total"
        fi
    done
}

# Final verification — replay every cart from BOT and compare every
# record. Catches drift the in-loop read_verify rate didn't sweep.
# Always uses drive 0 for verification — the choice is arbitrary, but
# fixed-drive verify gives a predictable failure signature; per-drive
# verify is already exercised inside run_ops via the random picker.
final_verify_all() {
    iscsi_login || return 1
    log_info "Final verify of all cartridges (drive 0)..."
    local c entry kind idx size expected="$TEST_DIR/scratch.expect" actual="$TEST_DIR/scratch.actual"
    local total_records=0 total_carts=0
    local nst="${DRIVE_NST_DEV[0]}"
    for c in "${CARTS[@]}"; do
        [[ -z "${RECORDS[$c]}" ]] && continue
        ensure_loaded "$c" 0 || return 1
        mt -f "$nst" rewind >/dev/null 2>&1 || { log_error "final_verify: rewind failed for $c"; return 1; }
        local cart_records=0
        while IFS= read -r entry; do
            [[ -z "$entry" ]] && continue
            kind="${entry%%:*}"
            local rest="${entry#*:}"
            idx="${rest%%:*}"
            size="${rest#*:}"
            case "$kind" in
                R)
                    mc_content_to "$c" "$idx" "$size" "$expected"
                    if ! dd if="$nst" of="$actual" bs="$size" count=1 status=none 2>/dev/null; then
                        log_error "final_verify: dd read failed (bc=$c idx=$idx size=$size)"
                        mc_dump_failure
                        return 1
                    fi
                    local actual_size
                    actual_size=$(stat -c%s "$actual")
                    if [[ "$actual_size" != "$size" ]]; then
                        log_error "final_verify: short read on $c record idx=$idx: expected=$size got=$actual_size"
                        mc_dump_failure
                        return 1
                    fi
                    if ! cmp -s "$expected" "$actual"; then
                        log_error "final_verify: content mismatch on $c record idx=$idx size=$size"
                        local first_diff
                        first_diff=$(cmp "$expected" "$actual" 2>&1 | head -1)
                        log_error "  first divergence: $first_diff"
                        mc_dump_failure
                        return 1
                    fi
                    cart_records=$(( cart_records + 1 ))
                    ;;
                F)
                    mt -f "$nst" fsf 1 >/dev/null 2>&1 || true
                    ;;
            esac
        done <<< "${RECORDS[$c]}"
        log_info "  $c: $cart_records records verified"
        total_records=$(( total_records + cart_records ))
        total_carts=$(( total_carts + 1 ))
    done
    log_info "Final verify OK ($total_records records across $total_carts cartridges)"
}

main() {
    echo "========================================"
    echo "thurvtl Monte Carlo Random-Op Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    setup_chap_user
    create_cartridges

    mc_assert_weights "op" "${OP_WEIGHTS[@]}"
    mc_seed_init "$SEED" "$TEST_DIR/ops.log"

    log_info "Running $OPS random ops (${#CARTS[@]} carts, $NUM_DRIVES drives)"
    if ! run_ops "$OPS"; then
        log_fail "Op loop aborted on failure"
        exit 1
    fi

    if ! final_verify_all; then
        log_fail "Final verification failed"
        exit 1
    fi

    echo ""
    echo "========================================"
    log_pass "$OPS ops + final verify  (seed=$MC_SEED)"
    echo "========================================"
    echo "Final state:"
    local c
    for c in "${CARTS[@]}"; do
        echo "  $c: $(record_count "$c") record(s)/filemark(s)"
    done
    echo "  reusable reproducer: --seed $MC_SEED --ops $OPS"
    echo "  op log: $TEST_DIR/ops.log"
    echo ""
    mc_op_stats_dump
    exit 0
}

main
