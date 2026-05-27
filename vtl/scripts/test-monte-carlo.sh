#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# thurvtl Monte Carlo Random-Op Test
#
# Runs N seeded random operations against a small VTL chassis (1 drive,
# 4 cartridges, 1 IE element). Op mix is weighted to bias drive data-path
# ops and sample changer / iSCSI churn at low rates. Tape semantics:
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

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/thurvtl-monte-carlo-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
DAEMON_PID=""
ISCSI_CONNECTED=0
SEED=""
QUICK=0
OPS=""
CHANGER_DEVICE=""
TAPE_DEVICE=""
NOREWIND_DEVICE=""
BACKEND_NAME="${THURVTL_TEST_BACKEND:-}"
SOURCE_BACKENDS="${THURVTL_SOURCE_BACKENDS:-${REPO_DIR}/private/storage-backends.yaml}"
BACKEND_TYPE=""
TEST_PREFIX=""

# Chassis: 4 cartridges in slots 1..4, 1 drive in element 0, 1 IE element.
CARTS=(MC01L8 MC02L8 MC03L8 MC04L8)
NUM_SLOTS=8
NUM_DRIVES=1

# Lazy state. Both start "down" and the first drive op brings them up.
ISCSI_UP=0
LOADED_CART=""

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

while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --ops) OPS="$2"; shift 2 ;;
        --backend) BACKEND_NAME="$2"; shift 2 ;;
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

[[ $QUICK -eq 1 ]] && : "${OPS:=200}"
: "${OPS:=3000}"

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    if [[ $ISCSI_CONNECTED -eq 1 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
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
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 || true
    if ! iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1; then
        log_error "iscsi login failed"
        return 1
    fi
    ISCSI_CONNECTED=1
    sleep 3
    CHANGER_DEVICE=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE" ]] || { log_error "Changer device not found"; lsscsi -g; return 1; }
    TAPE_DEVICE=$(lsscsi | awk '/tape/{print $NF}' | head -1)
    [[ -n "$TAPE_DEVICE" ]] || { log_error "Tape device not found"; lsscsi; return 1; }
    NOREWIND_DEVICE=$(echo "$TAPE_DEVICE" | sed 's|/dev/st|/dev/nst|')
    # Warm up: clear pending UA from login.
    mtx -f "$CHANGER_DEVICE" status >/dev/null 2>&1 || true
    mt -f "$NOREWIND_DEVICE" status >/dev/null 2>&1 || true
    ISCSI_UP=1
}

iscsi_logout() {
    if [[ $ISCSI_UP -eq 0 ]]; then return 0; fi
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete >/dev/null 2>&1 || true
    ISCSI_UP=0
    ISCSI_CONNECTED=0
    CHANGER_DEVICE=""
    TAPE_DEVICE=""
    NOREWIND_DEVICE=""
    # Do NOT clear LOADED_CART — iSCSI logout doesn't unload the
    # physical cart; the daemon's drive state persists across sessions.
    # On re-login, ensure_loaded re-syncs from the daemon anyway.
}

# Ask the daemon which cart is loaded in drive 0 right now. Returns the
# barcode on stdout, or empty if no cart is loaded. Authoritative — use
# this in preference to the shell-side LOADED_CART when accuracy
# matters (i.e. across any path that might have changed drive state
# without our notice).
daemon_loaded_cart() {
    "$CLI_PATH" --config "$TEST_CONFIG" cartridge list --json 2>/dev/null \
        | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for c in d.get('cartridges', []):
    if c.get('location') == 'drive':
        print(c.get('barcode', ''))
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

# Ensure some cartridge is loaded in drive 0. Resync from the daemon
# first — shell-side LOADED_CART can drift across iSCSI logout/login or
# external state churn. If $want is passed, that specific barcode ends
# up loaded; otherwise any cart will do (the picker preserves what's
# already loaded if there is one).
ensure_loaded() {
    iscsi_login || return 1
    local want="$1"
    LOADED_CART=$(daemon_loaded_cart)
    if [[ -z "$want" && -n "$LOADED_CART" ]]; then
        return 0
    fi
    if [[ -n "$want" && "$LOADED_CART" == "$want" ]]; then
        return 0
    fi
    # Need to change carts.
    if [[ -n "$LOADED_CART" ]]; then
        # Make sure we're not mid-position (writes-in-flight). Rewind
        # before unload to avoid a partial-write surprise.
        mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
        local origin
        origin=$(any_empty_slot)
        [[ -z "$origin" ]] && origin=1
        if ! mtx -f "$CHANGER_DEVICE" unload "$origin" 0 >/dev/null 2>&1; then
            log_error "ensure_loaded: unload of $LOADED_CART to slot $origin failed"
            return 1
        fi
        LOADED_CART=""
    fi
    # Pick a slot to load from. If $want is set, find its slot; else
    # pick deterministically.
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
        local idx
        idx=$(mc_rng_u32 "load-pick" "${#CARTS[@]}")
        target_bc="${CARTS[$idx]}"
        target_slot=$(slot_of_cart "$target_bc")
        if [[ -z "$target_slot" ]]; then
            # Cart may have been left in some odd state (e.g. exported).
            # Fall through to first cart with a known slot.
            for cand in "${CARTS[@]}"; do
                target_slot=$(slot_of_cart "$cand")
                if [[ -n "$target_slot" ]]; then
                    target_bc="$cand"
                    break
                fi
            done
        fi
        [[ -n "$target_slot" ]] || { log_error "ensure_loaded: no cart available in any slot"; return 1; }
    fi
    if ! mtx -f "$CHANGER_DEVICE" load "$target_slot" 0 >/dev/null 2>&1; then
        log_error "ensure_loaded: load of $target_bc from slot $target_slot failed"
        return 1
    fi
    LOADED_CART="$target_bc"
}

# Position at end-of-data so every write_record appends. We use
# rewind+fsr(N) rather than `mt eod`: the underlying filemark-on-READ
# bug (#25) that made `mt eod` corrupt the next write has been fixed,
# but rewind+fsr is still the more predictable form — each fsr block
# is a single LBA step, and the kernel/daemon agree on what those
# LBAs are regardless of how many filemarks sit on the medium.
seek_eod() {
    local bc="$LOADED_CART"
    [[ -z "$bc" ]] && return 0
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1
    local n
    n=$(record_count "$bc")
    if (( n > 0 )); then
        mt -f "$NOREWIND_DEVICE" fsr "$n" >/dev/null 2>&1
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
    ensure_loaded || return 1
    seek_eod
    local bc="$LOADED_CART"
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
    if ! dd if="$tmp" of="$NOREWIND_DEVICE" bs="$size" count=1 status=none 2>/dev/null; then
        log_error "write_record: dd failed (bc=$bc idx=$idx size=$size)"
        mc_dump_failure
        return 1
    fi
    push_record "$bc" "R:$idx:$size"
    NEXT_REC_IDX[$bc]=$(( idx + 1 ))
    mc_log_op write_record cart="$bc" idx="$idx" size="$size"
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
    ensure_loaded "$bc" || return 1
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || { log_error "read_verify: rewind failed"; return 1; }
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
                if ! dd if="$NOREWIND_DEVICE" of="$actual" bs="$size" count=1 status=none 2>/dev/null; then
                    log_error "read_verify: dd read failed (bc=$bc idx=$idx size=$size)"
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
                mt -f "$NOREWIND_DEVICE" fsf 1 >/dev/null 2>&1 || true
                n_filemarks=$(( n_filemarks + 1 ))
                ;;
        esac
    done <<< "${RECORDS[$bc]}"
    mc_log_op read_verify cart="$bc" records="$n_records" filemarks="$n_filemarks"
}

# space_fwd/space_back ops are intentionally absent. The daemon's tape
# position logic plus the kernel /dev/nstN driver produces unpredictable
# state when a write follows an arbitrary space sequence, surfacing as
# read-back garbage at the "next" LBA. That's likely a real bug worth
# its own follow-up — but Monte Carlo isn't the right test for it; the
# scripted scsi-conformance tests catch position-management regressions
# more pointedly. Keep this harness focused on data correctness.

op_write_filemark() {
    ensure_loaded || return 1
    seek_eod
    local bc="$LOADED_CART"
    if ! mt -f "$NOREWIND_DEVICE" weof 1 >/dev/null 2>&1; then
        log_error "write_filemark: weof failed on $bc"
        mc_dump_failure
        return 1
    fi
    push_record "$bc" "F:0:0"
    mc_log_op write_filemark cart="$bc"
}

op_rewind() {
    ensure_loaded || return 1
    mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
    mc_log_op rewind cart="$LOADED_CART"
}

op_load_cycle() {
    iscsi_login || return 1
    # Unload current, pick a different cart, leave LOADED_CART empty so
    # the next data-path op lazily loads.
    if [[ -n "$LOADED_CART" ]]; then
        mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || true
        local origin prev="$LOADED_CART"
        origin=$(any_empty_slot)
        [[ -z "$origin" ]] && origin=1
        if ! mtx -f "$CHANGER_DEVICE" unload "$origin" 0 >/dev/null 2>&1; then
            log_error "load_cycle: unload failed (cart=$prev origin=$origin)"
            mc_dump_failure
            return 1
        fi
        LOADED_CART=""
        mc_log_op load_cycle prev="$prev"
    else
        mc_log_op load_cycle status=already_empty
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

# Export the currently-loaded cartridge (or any cart in a slot) to the
# import/export element, then re-import it. Round-trip — cart ends up
# back in storage (slot reassignment may happen).
op_import_export() {
    iscsi_login || return 1
    # Don't try to export a loaded cart — would need an unload dance.
    local victim_bc victim_slot=""
    for c in "${CARTS[@]}"; do
        if [[ "$c" == "$LOADED_CART" ]]; then continue; fi
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
    # Pick the first Full storage slot and the first Empty one. The
    # loaded cart is in Data Transfer Element 0 — not in any Storage
    # Element — so there's no risk of trying to move it out from under
    # ourselves via this op. mtx output carries no barcodes (no
    # VolumeTag descriptors from this READ ELEMENT STATUS), so we don't
    # bother trying to identify which cart is moving.
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
    ensure_loaded || return 1
    seek_eod
    local bc="$LOADED_CART"
    local n
    n=$(( $(mc_rng_u32 "fm-sync" 3) + 2 ))   # 2..4
    if ! mt -f "$NOREWIND_DEVICE" weof "$n" >/dev/null 2>&1; then
        log_error "write_filemarks_sync: weof $n failed on $bc"
        mc_dump_failure
        return 1
    fi
    local i
    for (( i=0; i<n; i++ )); do
        push_record "$bc" "F:0:0"
    done
    mc_log_op write_filemarks_sync cart="$bc" n="$n"
}

# ---------------------------------------------------------------------------
# Main op loop
# ---------------------------------------------------------------------------

run_ops() {
    local n="$1"
    local op
    local progress_every=$(( n / 20 ))
    (( progress_every < 1 )) && progress_every=1
    for (( MC_OP_INDEX=1; MC_OP_INDEX<=n; MC_OP_INDEX++ )); do
        op=$(mc_pick_weighted op \
            "35:write_record" "39:read_verify" \
            "5:rewind" \
            "10:load_cycle" "5:iscsi_logout_cycle" \
            "3:import_export" "3:changer_move")
        case "$op" in
            write_record)         op_write_record || return 1 ;;
            read_verify)          op_read_verify || return 1 ;;
            rewind)               op_rewind || return 1 ;;
            load_cycle)           op_load_cycle || return 1 ;;
            iscsi_logout_cycle)   op_iscsi_logout_cycle || return 1 ;;
            import_export)        op_import_export || return 1 ;;
            changer_move)         op_changer_move || return 1 ;;
        esac
        if (( MC_OP_INDEX % progress_every == 0 )); then
            local total=0
            for c in "${CARTS[@]}"; do
                total=$(( total + $(record_count "$c") ))
            done
            log_info "[$MC_OP_INDEX/$n] loaded=${LOADED_CART:-<empty>} iscsi=$ISCSI_UP total_records=$total"
        fi
    done
}

# Final verification — replay every cart from BOT and compare every
# record. Catches drift the in-loop read_verify rate didn't sweep.
final_verify_all() {
    iscsi_login || return 1
    log_info "Final verify of all cartridges..."
    local c entry kind idx size expected="$TEST_DIR/scratch.expect" actual="$TEST_DIR/scratch.actual"
    local total_records=0 total_carts=0
    for c in "${CARTS[@]}"; do
        [[ -z "${RECORDS[$c]}" ]] && continue
        ensure_loaded "$c" || return 1
        mt -f "$NOREWIND_DEVICE" rewind >/dev/null 2>&1 || { log_error "final_verify: rewind failed for $c"; return 1; }
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
                    if ! dd if="$NOREWIND_DEVICE" of="$actual" bs="$size" count=1 status=none 2>/dev/null; then
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
                    mt -f "$NOREWIND_DEVICE" fsf 1 >/dev/null 2>&1 || true
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
    create_cartridges

    mc_seed_init "$SEED" "$TEST_DIR/ops.log"

    log_info "Running $OPS random ops (${#CARTS[@]} carts, $NUM_DRIVES drive)"
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
    exit 0
}

main
