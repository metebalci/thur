# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# Shared helpers for vtl/scripts/test-*.sh and vsa/scripts/test-*.sh.
#
# Sourced — not executed. Each test script declares its own intent and
# arg parsing; this file only carries the boilerplate that was identical
# across all ten test scripts (color codes, log helpers, port pickers).
# Behaviour is intentionally unchanged from the per-script copies — the
# only difference is the one-line source instead of ~20 lines of dupe.
#
# Usage:
#   SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"

# ANSI colors for the [INFO] / [WARN] / [ERROR] / [TEST] tags below.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_test()  { echo -e "${YELLOW}[TEST]${NC} $*"; }

# Pick a free TCP port. Uses python3 (most reliable — kernel hands us a
# port off the ephemeral range that nobody else is bound to) and falls
# back to a high random port if python3 isn't installed. There is a
# small race between print + the daemon binding, but the test scripts
# bind in seconds — collisions are vanishingly rare in practice.
pick_free_port() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()'
    else
        echo $((40000 + RANDOM % 20000))
    fi
}

# Populate ISCSI_PORT / HTTP_PORT only if the caller hasn't already set
# them (via --iscsi-port / --http-port flags). Logs the values so the
# CI run is reproducible from the captured stdout.
assign_ports() {
    [[ -z "$ISCSI_PORT" ]] && ISCSI_PORT=$(pick_free_port)
    [[ -z "$HTTP_PORT"  ]] && HTTP_PORT=$(pick_free_port)
    log_info "Using iSCSI port $ISCSI_PORT, HTTP port $HTTP_PORT"
}

# -----------------------------------------------------------------------
# Daemon lifecycle helpers — lifted from the ~80-LOC prologue that used
# to live near the top of every test script (check_prerequisites +
# start_daemon + cleanup).
#
# Convention: each test script sets BUILD_PROFILE (debug | release),
# optionally DAEMON_PATH / CLI_PATH overrides, TEST_DIR, TEST_CONFIG,
# HTTP_PORT, KEEP_DATA — then calls require_daemon_binaries +
# start_thur_daemon, and arranges its cleanup trap to call
# standard_cleanup (or stop_thur_daemon + a custom rm).
# -----------------------------------------------------------------------

# Resolve $DAEMON_PATH / $CLI_PATH from $BUILD_PROFILE if not already
# set, and verify both files exist. Caller passes the product short
# name (`thurvtl` | `thurvsa`); the daemon binary is `${product}d`, the
# CLI is `${product}`.
require_daemon_binaries() {
    local product="$1"
    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/${product}d}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/${product}}"
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"
    if [[ ! -f "$DAEMON_PATH" ]]; then
        log_error "Daemon not found at: $DAEMON_PATH"
        log_error "Build with: $build_cmd"
        exit 1
    fi
    if [[ ! -f "$CLI_PATH" ]]; then
        log_error "CLI not found at: $CLI_PATH"
        log_error "Build with: $build_cmd"
        exit 1
    fi
}

# Start the daemon in the background, capture its PID into $DAEMON_PID,
# poll /health until ready or 30s timeout. On timeout dumps the last 30
# lines of the daemon log and exits 1.
#
# Reads:   $DAEMON_PATH, $TEST_CONFIG, $HTTP_PORT
#          $DAEMON_LOG          (default $TEST_DIR/daemon.log)
#          $DAEMON_LOG_MODE     ("truncate" | "append"; default truncate)
#          $RUST_LOG            (default "info")
# Writes:  $DAEMON_PID
#
# Scripts that restart the daemon multiple times set DAEMON_LOG_MODE=
# append so every restart's output lands in the same file. The default
# truncates on each start, matching the original per-script copies.
start_thur_daemon() {
    local log_path="${DAEMON_LOG:-${TEST_DIR}/daemon.log}"
    log_info "Starting daemon..."
    if [[ "${DAEMON_LOG_MODE:-truncate}" == "append" ]]; then
        RUST_LOG="${RUST_LOG:-info}" "$DAEMON_PATH" --config "$TEST_CONFIG" >> "$log_path" 2>&1 &
    else
        RUST_LOG="${RUST_LOG:-info}" "$DAEMON_PATH" --config "$TEST_CONFIG" > "$log_path" 2>&1 &
    fi
    DAEMON_PID=$!
    local i
    for i in {1..30}; do
        if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
            log_info "Daemon ready (PID $DAEMON_PID)"
            return 0
        fi
        sleep 1
    done
    log_error "Daemon did not become ready in time"
    log_error "Last 30 lines of daemon log:"
    tail -30 "$log_path"
    exit 1
}

# Stop the running daemon. Idempotent — safe from a cleanup trap that
# fires before start_thur_daemon ran (DAEMON_PID empty), or after it
# already ran (caller cleared the var manually).
stop_thur_daemon() {
    if [[ -n "${DAEMON_PID:-}" ]]; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
}

# Standard cleanup body: stop daemon, remove $TEST_DIR unless KEEP_DATA=1.
# Scripts with extra cleanup (umount, iscsi logout, podman rm) put the
# extra steps in their own cleanup() and call this at the end.
standard_cleanup() {
    log_info "Cleaning up..."
    stop_thur_daemon
    if [[ "${KEEP_DATA:-0}" -eq 0 ]]; then
        log_info "Removing test directory: $TEST_DIR"
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
}

# -----------------------------------------------------------------------
# Common-flag argument parsing — lifted from the 30+ lines of identical
# `while/case` boilerplate at the top of every test script.
#
# Common flags:
#   --release             switch to ./target/release/ binaries
#   --daemon-path PATH    override daemon binary
#   --cli-path PATH       override CLI binary
#   --keep-data           don't rm -rf $TEST_DIR on exit
#   --iscsi-port PORT     override iSCSI listen port
#   --http-port  PORT     override HTTP listen port
#   -h | --help           print the script header comment block + exit 0
#
# Scripts come in two shapes:
#
# (a) ONLY common flags — replace the whole arg block with:
#         init_common_daemon_args
#         parse_common_daemon_args "$@"
#
# (b) Common flags + script-specific flags — keep the while/case, put
#     script-specific arms first, delegate the default arm:
#         init_common_daemon_args
#         while [[ $# -gt 0 ]]; do
#             case "$1" in
#                 --seed)  SEED="$2"; shift 2 ;;
#                 --quick) QUICK=1;   shift ;;
#                 *)
#                     if parse_common_daemon_arg "$@"; then
#                         shift "$_CONSUMED_ARGS"
#                     else
#                         echo "Unknown option: $1" >&2; exit 1
#                     fi
#                     ;;
#             esac
#         done
# -----------------------------------------------------------------------

# Initialize the seven common arg variables to their defaults if unset.
# Idempotent. Call before the arg loop; explicit per-script assignments
# (e.g. KEEP_DATA=0 at the top of the script) become redundant after.
init_common_daemon_args() {
    : "${BUILD_PROFILE:=debug}"
    : "${DAEMON_PATH:=}"
    : "${CLI_PATH:=}"
    : "${ISCSI_PORT:=}"
    : "${HTTP_PORT:=}"
    : "${KEEP_DATA:=0}"
    : "${DAEMON_PID:=}"
}

# Try to consume one common daemon flag. Sets $_CONSUMED_ARGS to the
# number of positional args consumed (1 for boolean flags, 2 for value
# flags); returns 0 on match, 1 on unknown. -h/--help prints the script
# header comment block (lines 2 to first blank-comment-line) and exits 0.
parse_common_daemon_arg() {
    case "$1" in
        --release)     BUILD_PROFILE="release"; _CONSUMED_ARGS=1 ;;
        --daemon-path) DAEMON_PATH="$2";        _CONSUMED_ARGS=2 ;;
        --cli-path)    CLI_PATH="$2";           _CONSUMED_ARGS=2 ;;
        --keep-data)   KEEP_DATA=1;             _CONSUMED_ARGS=1 ;;
        --iscsi-port)  ISCSI_PORT="$2";         _CONSUMED_ARGS=2 ;;
        --http-port)   HTTP_PORT="$2";          _CONSUMED_ARGS=2 ;;
        -h|--help)     sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *)             return 1 ;;
    esac
    return 0
}

# Drive the entire arg loop for scripts that accept ONLY common flags.
# Unknown flag -> "Unknown option: X" + exit 1. Scripts with extra
# flags keep their own while/case loop and call parse_common_daemon_arg
# from the default arm — see the (b) example in the header above.
parse_common_daemon_args() {
    while [[ $# -gt 0 ]]; do
        if parse_common_daemon_arg "$@"; then
            shift "$_CONSUMED_ARGS"
        else
            echo "Unknown option: $1" >&2
            exit 1
        fi
    done
}

# -----------------------------------------------------------------------
# Conffile YAML section emitters — each prints one stanza to stdout.
# Compose into the test conffile via command substitution inside one
# heredoc:
#
#   cat > "$TEST_CONFIG" <<EOF
#   $(yaml_header)
#
#   $(yaml_vtl_library 40 2 8)
#
#   $(yaml_iscsi "$TARGET_IQN")
#
#   $(yaml_local_backend)
#
#   keystore:
#     backends:
#       local: { type: local }
#   EOF
#
# Bash strips the trailing newline from $() output, so the blank lines
# between stanzas come from the heredoc itself — adjust the blank-line
# layout in the heredoc, not in the helpers.
#
# All helpers read $TEST_DIR / $HTTP_PORT / $ISCSI_PORT from the
# enclosing script. Keep the helpers minimal — anything that isn't
# emitted by literally every test stays in the per-script heredoc.
# -----------------------------------------------------------------------

# Universal pair: data_dir + http.listen.
yaml_header() {
    cat <<EOFY
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"
EOFY
}

# iscsi.listen + optional target_iqn (omitted if no arg given).
yaml_iscsi() {
    local target_iqn="${1:-}"
    echo "iscsi:"
    echo "  listen: \"127.0.0.1:$ISCSI_PORT\""
    [[ -n "$target_iqn" ]] && echo "  target_iqn: \"$target_iqn\""
}

# VTL chassis topology declaration. Args: num_slots, num_drives,
# lto_generation (defaults 40 / 2 / 8 — the common test shape).
yaml_vtl_library() {
    local slots="${1:-40}"
    local drives="${2:-2}"
    local lto="${3:-8}"
    cat <<EOFY
library:
  num_slots: $slots
  num_drives: $drives
  lto_generation: $lto
EOFY
}

# Single-backend "storage:" stanza pointing at the in-tree local
# backend. Default-named "local"; pass a name override for scripts
# that want a different key (some tests use "primary" / "testbackend").
yaml_local_backend() {
    local name="${1:-local}"
    cat <<EOFY
storage:
  backends:
    $name:
      type: local
      root_dir: "$TEST_DIR/local-backend"
EOFY
}

# -----------------------------------------------------------------------
# Transport login/logout helpers — the discover-and-login (iSCSI) and
# connect-and-locate-namespace (NVMe/TCP) flows are nearly identical
# across every test that exercises the data path. The device-find
# step (lsscsi → /dev/sdN, sg passthrough lookup) varies per script
# and stays in the caller.
# -----------------------------------------------------------------------

# Discover and log in to the iSCSI target running at $ISCSI_PORT on
# 127.0.0.1 with IQN $TARGET_IQN. Sets ISCSI_CONNECTED=1 and sleeps 3
# to let the kernel publish /dev/sdN nodes; the caller then picks the
# specific device out of `lsscsi`.
iscsi_discover_and_login() {
    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null
    ISCSI_CONNECTED=1
    sleep 3
}

# Log out and delete the iSCSI node for $TARGET_IQN @ $ISCSI_PORT.
# Idempotent — safe to call from a cleanup trap whether or not login
# succeeded.
iscsi_logout_and_delete() {
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete  >/dev/null 2>&1 || true
    ISCSI_CONNECTED=0
}

# Connect to the NVMe/TCP subsystem on 127.0.0.1:$NVMETCP_PORT,
# locate the controller name via `nvme list-subsys -o json` (with a
# /dev/nvme*n1 fallback for older nvme-cli builds), and set
# NVME_DEVICE (e.g. "nvme0") + NVME_CONNECTED=1. The caller's
# namespace device path is then "/dev/${NVME_DEVICE}n1".
#
# Reads:  $NVMETCP_PORT, $SUBNQN, $HOST_NQN, $TEST_DIR
# Writes: $NVME_DEVICE, $NVME_CONNECTED
nvme_tcp_connect() {
    log_info "Connecting via nvme-cli..."
    if ! nvme connect -t tcp -a 127.0.0.1 -s "$NVMETCP_PORT" \
        -n "$SUBNQN" --hostnqn "$HOST_NQN" \
        > "$TEST_DIR/nvme-connect.log" 2>&1; then
        log_error "nvme connect failed"
        cat "$TEST_DIR/nvme-connect.log"
        return 1
    fi
    NVME_CONNECTED=1
    # `nvme list-subsys` json shape varies across distros — walk both
    # the old (Subsystems→Paths) and new (top-level Paths) layouts.
    NVME_DEVICE=$(nvme list-subsys -o json 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
target = '$SUBNQN'
def walk(d):
    if isinstance(d, dict):
        if d.get('NQN') == target:
            for p in d.get('Paths', []):
                if 'Name' in p:
                    return p['Name']
        for v in d.values():
            r = walk(v)
            if r:
                return r
    elif isinstance(d, list):
        for v in d:
            r = walk(v)
            if r:
                return r
    return None
name = walk(data)
print(name or '')
" 2>/dev/null)
    if [[ -z "$NVME_DEVICE" ]]; then
        # Fallback: pick the newest /dev/nvme*n1 — the one we just
        # created should be the highest-numbered.
        NVME_DEVICE=$(ls -1 /dev/nvme*n1 2>/dev/null \
            | sort -V | tail -1 | xargs -n1 basename | sed 's/n1$//')
    fi
    if [[ -z "$NVME_DEVICE" ]]; then
        log_error "Could not locate the connected NVMe controller"
        return 1
    fi
    local block="/dev/${NVME_DEVICE}n1"
    [[ -b "$block" ]] || { log_error "$block is not a block device"; return 1; }
    log_info "thurvsa namespace -> $block"
}

# Disconnect from the NVMe/TCP subsystem $SUBNQN. Idempotent — safe
# from a cleanup trap whether or not nvme_tcp_connect ran.
nvme_tcp_disconnect() {
    if [[ "${NVME_CONNECTED:-0}" -eq 1 ]]; then
        nvme disconnect -n "$SUBNQN" >/dev/null 2>&1 || true
        NVME_CONNECTED=0
        # Give the kernel a moment to tear down /dev/nvmeXn1.
        sleep 1
    fi
}

# Poll a log file for an extended regex match until it appears or the
# timeout elapses. Returns 0 if matched, 1 on timeout. Both args
# required; useful for the storage-failure tests that assert specific
# error-class strings show up in the daemon log.
#
# Usage:
#   wait_for_log_pattern /path/to/daemon.log 'failed with permanent error \(AUTH\)' 20
wait_for_log_pattern() {
    local logfile="$1"
    local pattern="$2"
    local timeout="${3:-30}"
    local deadline=$(( $(date +%s) + timeout ))
    while (( $(date +%s) < deadline )); do
        if [[ -f "$logfile" ]] && grep -Eq "$pattern" "$logfile"; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

# Per-row performance measurement. Consumed by test-pipeline-layers.sh
# and the perf-layers.sh harnesses (both VTL and VSA flavors). Caller
# pattern:
#
#   local t0 t1 t2
#   t0=$(date +%s%N)
#   <host-write step>            # tar_to_tape / write_fixture
#   t1=$(date +%s%N)
#   <wait-for-storage step>      # storage_wait_for_key / wait_for_chunks
#   t2=$(date +%s%N)
#   perf_summary 1 baseline random "$FIXTURE_BYTES" "$t0" "$t1" "$t2" "$cb"
#
# Emits one parseable line and appends it to PERF_LINES (the caller
# declares this as an array). The wall-clock split is intentional:
# host_MiBps is what the iSCSI initiator sees, total_MiBps is the
# end-to-end cost including the daemon's chunk-seal + upload pipeline.
#
# The `fixture` field carries the workload tag (`random` |
# `compressible` | `mixed` — caller-chosen). The pipeline-layer matrix
# uses `mixed` (legacy 512 MiB zeros + 512 MiB ABCDE…); the perf-layers
# harness pairs `random` and `compressible` to attribute dedup /
# compression cost cleanly.
perf_summary() {
    local row="$1" label="$2" fixture="$3" bytes="$4" t0="$5" t1="$6" t2="$7" storage_bytes="${8:-}"
    local mib host_s total_s host_mibps total_mibps
    mib=$(awk -v b="$bytes" 'BEGIN { printf "%.1f", b/1048576 }')
    host_s=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", (b-a)/1e9 }')
    total_s=$(awk -v a="$t0" -v b="$t2" 'BEGIN { printf "%.3f", (b-a)/1e9 }')
    host_mibps=$(awk -v b="$bytes" -v s="$host_s" \
        'BEGIN { if (s+0 <= 0) print "inf"; else printf "%.1f", (b/1048576)/s }')
    total_mibps=$(awk -v b="$bytes" -v s="$total_s" \
        'BEGIN { if (s+0 <= 0) print "inf"; else printf "%.1f", (b/1048576)/s }')
    local sb_field=""
    [[ -n "$storage_bytes" ]] && sb_field=" storage_bytes=$storage_bytes"
    local line="[PERF] row=$row label=$label fixture=$fixture fixture_MiB=$mib host_s=$host_s host_MiBps=$host_mibps total_s=$total_s total_MiBps=$total_mibps$sb_field"
    echo -e "${YELLOW}${line}${NC}"
    PERF_LINES+=("$line")
}

# Print a layer-comparison table from the accumulated PERF_LINES. One
# section per fixture; each section lists each row's total_MiBps plus
# the cumulative delta from L1, so the cost of each added layer is
# visible at a glance. Pure formatting — no FAIL behaviour. Called at
# the end of a perf-layers.sh run; safe to call with an empty
# PERF_LINES (prints nothing).
#
# Lines that don't carry a `fixture=` field (e.g. legacy matrix output)
# are bucketed under `unknown` so older callers still get something.
perf_table_emit() {
    (( ${#PERF_LINES[@]} > 0 )) || return 0
    local fixtures
    fixtures=$(printf '%s\n' "${PERF_LINES[@]}" \
        | awk 'match($0, /fixture=[^ ]+/) { print substr($0, RSTART+8, RLENGTH-8) }' \
        | awk 'NF' \
        | sort -u)
    [[ -z "$fixtures" ]] && fixtures="unknown"
    echo ""
    while IFS= read -r fx; do
        echo "Layer comparison (${fx} fixture):"
        local first_mibps=""
        for line in "${PERF_LINES[@]}"; do
            # Skip lines that don't match this fixture (or for the
            # `unknown` bucket: lines that have no fixture field at all).
            if [[ "$fx" == "unknown" ]]; then
                [[ "$line" == *" fixture="* ]] && continue
            else
                [[ "$line" == *" fixture=$fx "* ]] || continue
            fi
            local label total_mibps
            label=$(echo "$line" | awk 'match($0, /label=[^ ]+/) { print substr($0, RSTART+6, RLENGTH-6) }')
            total_mibps=$(echo "$line" | awk 'match($0, /total_MiBps=[^ ]+/) { print substr($0, RSTART+12, RLENGTH-12) }')
            local row
            row=$(echo "$line" | awk 'match($0, /row=[^ ]+/) { print substr($0, RSTART+4, RLENGTH-4) }')
            local delta=""
            if [[ -z "$first_mibps" ]]; then
                first_mibps="$total_mibps"
            else
                delta=$(awk -v a="$first_mibps" -v b="$total_mibps" \
                    'BEGIN { if (a+0<=0 || b=="inf") print ""; else printf "  (delta %+.1f vs L1)", (b-a) }')
            fi
            printf "  L%s %-26s = %8s MiB/s%s\n" "$row" "$label" "$total_mibps" "$delta"
        done
        echo ""
    done <<< "$fixtures"
}

# -----------------------------------------------------------------------
# Storage helpers — lifted from the duplicated bodies that previously
# lived in `vtl/scripts/test-backup-storage.sh` and
# `vsa/scripts/test-fs-iscsi-storage.sh`. The matrix scripts
# (test-pipeline-layers.sh, future shared lifts) consume the same
# functions.
#
# All helpers read the same per-script globals:
#   BACKEND_TYPE                 s3 | gcs | azure
#   BACKEND_BUCKET               bucket name (S3/GCS)
#   BACKEND_ENDPOINT             S3 endpoint URL ("" for real AWS)
#   BACKEND_REGION               S3 region
#   BACKEND_ACCOUNT              Azure storage account
#   BACKEND_CONTAINER            Azure container
#   BACKEND_AUTH_AKID_ENV        env-var name for AWS key ID
#                                (when backend uses `auth: env`),
#                                or empty for the default AWS chain
#   BACKEND_AUTH_SECRET_ENV      same for the secret
#   TEST_PREFIX                  per-run sub-prefix (cleanup boundary)
# -----------------------------------------------------------------------

# Map BACKEND_TYPE -> the backend CLI we use for assertions / cleanup.
storage_cli_for_type() {
    case "$BACKEND_TYPE" in
        s3)    echo "aws" ;;
        gcs)   echo "gcloud" ;;
        azure) echo "az" ;;
        *)     echo "unknown" ;;
    esac
}

# List keys under ${TEST_PREFIX}${subpath}. One key per line, the
# bucket prefix stripped so the output is portable across backends.
# Empty output on no hits / cleanup race / cred miss; callers that
# need to distinguish use `storage_wait_for_key` instead.
storage_list() {
    local subpath="$1"
    local full="${TEST_PREFIX}${subpath}"
    case "$BACKEND_TYPE" in
        s3)
            local args=()
            [[ -n "$BACKEND_ENDPOINT" ]] && args+=(--endpoint-url "$BACKEND_ENDPOINT")
            [[ -n "$BACKEND_REGION" ]] && args+=(--region "$BACKEND_REGION")
            local aws_overrides=()
            if [[ -n "$BACKEND_AUTH_AKID_ENV" && -n "$BACKEND_AUTH_SECRET_ENV" ]]; then
                aws_overrides=(
                    "AWS_ACCESS_KEY_ID=${!BACKEND_AUTH_AKID_ENV}"
                    "AWS_SECRET_ACCESS_KEY=${!BACKEND_AUTH_SECRET_ENV}"
                )
            fi
            env "${aws_overrides[@]}" aws "${args[@]}" s3 ls "s3://${BACKEND_BUCKET}/${full}" --recursive 2>/dev/null \
                | awk '{ $1=""; $2=""; $3=""; sub(/^ +/, ""); print }'
            ;;
        gcs)
            gcloud storage ls --recursive "gs://${BACKEND_BUCKET}/${full}**" 2>/dev/null \
                | grep -v '/$' | sed "s|^gs://${BACKEND_BUCKET}/||"
            ;;
        azure)
            az storage blob list \
                --account-name "$BACKEND_ACCOUNT" \
                --container-name "$BACKEND_CONTAINER" \
                --prefix "$full" \
                --auth-mode login \
                --query "[].name" -o tsv 2>/dev/null
            ;;
        *)
            return 1
            ;;
    esac
}

# Snapshot the sorted set of chunk-object keys to a file. Used by
# the pipeline-layer matrix's row 2 dedup assertion: snap before +
# after the second write, take the set-difference, count new keys.
# Replaces the legacy byte-ratio check (`bytes_two ≤ 1.5 ×
# bytes_one`) which drifted out of calibration as fixture sizes grew
# — manifest-backup churn pushed the byte ratio past the ceiling
# even when dedup was working perfectly. Set-difference against
# chunks/ alone is fixture-size independent and tests the actual
# dedup behaviour, not a size proxy.
storage_chunks_snapshot() {
    local out="$1"
    storage_list "chunks/" | sort > "$out"
}

# Count keys present in `after` but not in `before`. Stdout is the
# integer; stderr is unused. Inputs must be the sorted output of
# storage_chunks_snapshot above.
storage_chunks_new_count() {
    local before="$1" after="$2"
    comm -23 "$after" "$before" | wc -l
}

# Block until at least one object exists under ${TEST_PREFIX}${subpath}.
# Returns 0 on hit, 1 on timeout. Used for manifest-backup / chunk-
# arrival assertions where the upload pipeline is asynchronous.
storage_wait_for_key() {
    local subpath="$1"
    local timeout="${2:-60}"
    local elapsed=0
    while (( elapsed < timeout )); do
        local count
        count=$(storage_list "$subpath" | wc -l)
        if (( count > 0 )); then
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
    done
    return 1
}

# Smoke-probe the backend with a bucket-scoped op (head-bucket, ls,
# blob list). Daemon + CLI use the same default credential chain, so
# a CLI-side reach failure is reliably the same as the daemon would
# hit at boot — catches the common "sudo stripped my creds" surprise
# before we burn a minute on the rest of the test.
verify_storage_creds() {
    log_info "Verifying storage credentials are visible to the daemon's environment..."
    local rc=0 out=""
    case "$BACKEND_TYPE" in
        s3)
            local args=()
            [[ -n "$BACKEND_ENDPOINT" ]] && args+=(--endpoint-url "$BACKEND_ENDPOINT")
            [[ -n "$BACKEND_REGION" ]] && args+=(--region "$BACKEND_REGION")
            local aws_overrides=()
            if [[ -n "$BACKEND_AUTH_AKID_ENV" && -n "$BACKEND_AUTH_SECRET_ENV" ]]; then
                aws_overrides=(
                    "AWS_ACCESS_KEY_ID=${!BACKEND_AUTH_AKID_ENV}"
                    "AWS_SECRET_ACCESS_KEY=${!BACKEND_AUTH_SECRET_ENV}"
                )
            fi
            out=$(env "${aws_overrides[@]}" aws "${args[@]}" s3api head-bucket --bucket "$BACKEND_BUCKET" 2>&1) || rc=$?
            ;;
        gcs)
            if [[ -r "${GOOGLE_APPLICATION_CREDENTIALS:-}" ]]; then
                gcloud auth activate-service-account --key-file="$GOOGLE_APPLICATION_CREDENTIALS" >/dev/null 2>&1 || true
            fi
            out=$(gcloud storage ls "gs://${BACKEND_BUCKET}/" 2>&1) || rc=$?
            ;;
        azure)
            if [[ -n "${AZURE_CLIENT_ID:-}" && -n "${AZURE_CLIENT_SECRET:-}" && -n "${AZURE_TENANT_ID:-}" ]]; then
                az login --service-principal \
                    -u "$AZURE_CLIENT_ID" \
                    -p "$AZURE_CLIENT_SECRET" \
                    --tenant "$AZURE_TENANT_ID" >/dev/null 2>&1 || true
            fi
            out=$(az storage blob list \
                --account-name "$BACKEND_ACCOUNT" \
                --container-name "$BACKEND_CONTAINER" \
                --auth-mode login \
                --num-results 1 --query "[0].name" -o tsv 2>&1) || rc=$?
            ;;
    esac
    if (( rc != 0 )); then
        log_error "Storage credential probe failed for backend type '$BACKEND_TYPE':"
        echo "$out" | sed 's/^/  /'
        return 1
    fi
    log_info "Storage credentials OK (cli=$(storage_cli_for_type) can reach bucket)"
    return 0
}

# -----------------------------------------------------------------------
# Low-level SCSI helpers (sg_raw wrappers) for the pipeline-layer
# matrix tests. Both build hand-crafted CDBs because sg3-utils doesn't
# ship higher-level commands for the SSC-4 Data Compression mode page
# or the SECURITY PROTOCOL OUT Set Data Encryption page (sg_modes only
# *reads*, sg_seek doesn't touch encryption, and `sg_persist` is for PR
# only). Test scripts using these helpers must already be running with
# root euid (post-self-elevation) and have $TAPE_SG_DEVICE bound to the
# drive's /dev/sgN passthrough.
# -----------------------------------------------------------------------

# Issue MODE SELECT(10) carrying the Data Compression mode page 0x0F
# with DCE=1 and DDE=1 (drive-side LZ4/zstd both directions). The
# drive's configured algorithm (`drive.compression.algorithm` in
# thurvtl.yaml) determines what cipher actually runs — the host only
# flips the on/off switch.
#
# Args:
#   $1 = sg device (e.g. /dev/sg2)
#   $2 = "on" | "off" (DCE+DDE state to write)
scsi_enable_dce() {
    local sg_dev="$1"
    local state="${2:-on}"
    local dce_byte=0x00
    local dde_byte=0x00
    if [[ "$state" == "on" ]]; then
        # bit 7 = DCE (writes compressed), bit 6 = DCC (compression
        # capability — read-only-ish, leave 0); we only set DCE.
        dce_byte=0x80
        # bit 7 = DDE (drive decompresses encrypted reads transparently),
        # bits 6-5 = RED (report exception data); we want DDE=1 RED=0.
        dde_byte=0x80
    fi
    # Parameter list = 8B mode header (zeros) + 16B page 0x0F.
    # Page 0x0F layout:
    #   byte 0: page code (0x0F)        PS=0, SPF=0
    #   byte 1: page length (14)
    #   byte 2: DCE / DCC / reserved
    #   byte 3: DDE / RED / reserved
    #   bytes 4-7:  compression algorithm   (0 = use drive default)
    #   bytes 8-11: decompression algorithm (0 = use drive default)
    #   bytes 12-15: reserved
    local payload_hex="00 00 00 00 00 00 00 00 0F 0E ${dce_byte#0x} ${dde_byte#0x} 00 00 00 00 00 00 00 00 00 00 00 00"
    # CDB:  55 10 00 00 00 00 00 00 18 00
    #   opcode=0x55 (MODE SELECT 10), PF=1 / SP=0 -> 0x10,
    #   reserved..reserved, alloc length = 24 (0x18), control=0
    log_info "sg_raw MODE SELECT page 0x0F DCE=$state on $sg_dev"
    # Binary payload bytes via xxd -r -p (canonical hex pairs ->
    # raw bytes). Crucially we cannot stage the bytes through a
    # bash variable because the header starts with `00 00 ...` and
    # bash strings truncate at the first null. Pipe them directly
    # into a process-substitution fd that sg_raw reads as the input
    # file.
    # shellcheck disable=SC2086
    sg_raw -s 24 -i <(printf '%s' "$payload_hex" | tr -d ' ' | xxd -r -p) \
        "$sg_dev" 55 10 00 00 00 00 00 00 18 00 2>&1 | tail -5
}

# Issue SECURITY PROTOCOL OUT (0xB5) with Set Data Encryption page
# 0x0010, installing a session AES-256 key on the drive. After this
# completes successfully, every WRITE encrypts with the key and
# every READ decrypts (until cartridge UNLOAD or explicit clear).
#
# Args:
#   $1 = sg device (drive LUN's /dev/sgN)
#   $2 = key as 64-char hex string (AES-256, no newline)
scsi_set_session_key() {
    local sg_dev="$1"
    local key_hex="$2"
    if [[ ${#key_hex} -ne 64 ]]; then
        log_error "scsi_set_session_key: key must be 64 hex chars, got ${#key_hex}"
        return 1
    fi
    # Set Data Encryption page layout (page 0x0010):
    #   bytes 0-1: page code  (0x00 0x10)
    #   bytes 2-3: page length after this field (= 42 = 0x002A)
    #   byte 4:    SCOPE (bits 7-5) | reserved | LOCK
    #              SCOPE=AllItNexus(0x02) -> 0x40
    #   byte 5:    CEEM/RDMC/SDK/CKOD       (all 0)
    #   byte 6:    ENCRYPTION_MODE = 0x02 (INTERNAL = "drive encrypts")
    #   byte 7:    DECRYPTION_MODE = 0x02 (drive decrypts on read)
    #   byte 8:    ALGORITHM_INDEX = 0x01 (AES-256-GCM)
    #   byte 9:    KEY_FORMAT = 0x00 (plaintext)
    #   byte 10:   reserved
    #   byte 11:   KAD_FORMAT = 0x00 (unspecified)
    #   bytes 12-13: KEY_LENGTH = 0x0020 (32 bytes)
    #   bytes 14-45: KEY (32 raw bytes)
    local header_hex="00 10 00 2A 40 00 02 02 01 00 00 00 00 20"
    local payload_hex="$header_hex $key_hex"
    # CDB:  B5 20 00 10 00 00 00 00 00 2E 00 00
    #   opcode=0xB5 (SPOUT), security protocol=0x20 (tape data enc),
    #   SPSP=0x0010 (Set Data Encryption), reserved, transfer length
    #   bytes 6-9 big-endian = 46 (0x2E), reserved, control=0.
    log_info "sg_raw SPOUT page 0x10 set session key on $sg_dev"
    # Pipe-direct binary input: bash variables truncate at the first
    # null byte, and the page-code header begins with 0x00. Process
    # substitution sidesteps that. (See scsi_enable_dce for the
    # earlier trap.)
    # shellcheck disable=SC2086
    sg_raw -s 46 -i <(printf '%s' "$payload_hex" | tr -d ' ' | xxd -r -p) \
        "$sg_dev" B5 20 00 10 00 00 00 00 00 2E 00 00 2>&1 | tail -5
}

# Recursively delete everything under ${TEST_PREFIX} on the backend.
# Always runs on cleanup; idempotent on a previously-clean prefix.
storage_purge_test_prefix() {
    [[ -z "$TEST_PREFIX" ]] && return 0
    case "$BACKEND_TYPE" in
        s3)
            local args=()
            [[ -n "$BACKEND_ENDPOINT" ]] && args+=(--endpoint-url "$BACKEND_ENDPOINT")
            [[ -n "$BACKEND_REGION" ]] && args+=(--region "$BACKEND_REGION")
            local aws_overrides=()
            if [[ -n "$BACKEND_AUTH_AKID_ENV" && -n "$BACKEND_AUTH_SECRET_ENV" ]]; then
                aws_overrides=(
                    "AWS_ACCESS_KEY_ID=${!BACKEND_AUTH_AKID_ENV}"
                    "AWS_SECRET_ACCESS_KEY=${!BACKEND_AUTH_SECRET_ENV}"
                )
            fi
            env "${aws_overrides[@]}" aws "${args[@]}" s3 rm "s3://${BACKEND_BUCKET}/${TEST_PREFIX}" --recursive >/dev/null 2>&1 || true
            ;;
        gcs)
            gcloud storage rm --recursive "gs://${BACKEND_BUCKET}/${TEST_PREFIX}**" >/dev/null 2>&1 || true
            ;;
        azure)
            az storage blob delete-batch \
                --account-name "$BACKEND_ACCOUNT" \
                --source "$BACKEND_CONTAINER" \
                --auth-mode login \
                --pattern "${TEST_PREFIX}*" >/dev/null 2>&1 || true
            ;;
    esac
}
