#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VSA Postgres-Driven Block-Storage Test
#
# Drives a real PostgreSQL workload (in podman) against thurvsad. Catches
# gaps that the synthetic / filesystem tests miss: fsync ordering under
# WAL, mixed sequential heap inserts plus random index updates,
# transactional crash recovery. This is the VSA counterpart to
# vtl/scripts/test-app-bareos.sh — a real program driving the storage
# with non-trivial workload, plus a verifiable post-condition.
#
# Workflow:
#   Phase A — bootstrap
#     1. thurvsad up with one volume, local backend.
#     2. Login / connect on host (iscsi or nvmetcp), resolve the block
#        device.
#     3. mkfs.ext4 + mount at $TEST_DIR/mnt.
#     4. Build small postgres container (debian:12 + postgresql) from an
#        inline Containerfile; cached after first build by Containerfile
#        fingerprint, same pattern as test-app-bareos.sh.
#     5. Start container: initdb the cluster onto the mounted volume,
#        start postgres in the foreground.
#   Phase B — pgbench init + initial invariant
#     6. `pgbench -i -s SCALE` — heavy bulk writes.
#     7. Verify row counts match the SCALE contract (branches=S,
#        tellers=10S, accounts=100000S) and the TPC-B invariant
#        (sum(accounts.abalance) == sum(tellers.tbalance) ==
#         sum(branches.bbalance) == sum(history.delta)) — all should be
#        zero at init.
#   Phase C — clean OLTP workload
#     8. `pgbench -c C -j J -T T` concurrent OLTP.
#     9. Re-verify the TPC-B invariant. With clean shutdown the four
#        sums must remain equal.
#   Phase D — crash recovery
#    10. Re-start pgbench, then `podman kill --signal=KILL` mid-workload.
#        Every postgres process dies abruptly with WAL records buffered
#        and/or unfsynced.
#    11. Stop container, umount, fsck.ext4 -fn (must succeed — the
#        filesystem layer must be intact).
#    12. Remount, start a fresh container — postgres replays WAL on
#        startup.
#    13. Re-verify the TPC-B invariant. If thurvsa's SBC / NVM fsync
#        ordering is correct, WAL replay restores a consistent state and
#        the four sums match.
#
# The TPC-B invariant is the load-bearing assertion: each pgbench
# transaction adds the same `delta` to one account, one teller, one
# branch and records it in history. A crash mid-transaction must either
# commit all four updates (visible after WAL replay) or none — never
# partial — so the four sums stay equal even across SIGKILL.
#
# Reproducibility: --seed N picks the same scale / concurrency / runtime
# bucket each run, so a failure is replayable. --quick locks scale=1 +
# T=30 s for a ~1 min smoke variant.
#
# Prerequisites:
#   - podman             (sudo apt-get install podman)
#   - mkfs.ext4 / fsck.ext4  (sudo apt-get install e2fsprogs)
#   - iSCSI mode:  open-iscsi, lsscsi; iscsid running
#                  (sudo systemctl enable --now iscsid)
#   - NVMe/TCP mode: nvme-cli; nvme_tcp kernel module
#                    (sudo modprobe nvme_tcp)
#   - curl
#   - Root/sudo access (self-elevates via NOPASSWD sudoers)
#
# Usage (invoke from repo root):
#   ./vsa/scripts/test-app-postgres.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run
#   --quick               scale=1, T=30 s (~1 min wall clock)
#   --transport T         iscsi (default) or nvmetcp
#   --debug               Use ./target/debug/ binaries (default: ./target/release/)
#   --daemon-path PATH    Override thurvsad
#   --cli-path PATH       Override thurvsa
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect iSCSI on exit (iscsi mode)
#   --keep-nvme           Don't disconnect NVMe on exit (nvmetcp mode)
#   --keep-container      Don't stop/remove the postgres container on exit
#   --iscsi-port PORT     Override iSCSI port (iscsi mode)
#   --nvmetcp-port PORT   Override NVMe/TCP port (nvmetcp mode)
#   --http-port PORT      Override HTTP port
#

# Self-elevate via sudo. The script does mkfs.ext4, mount, iscsiadm /
# nvme, and runs podman against block devices. NOPASSWD sudoers entry
# assumed.
if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

TEST_DIR="/tmp/test-app-postgres-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
TRANSPORT="iscsi"
NVMETCP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvsa"
SUBNQN="nqn.2025-10.com.metebalci:thurvsa"
HOST_NQN="nqn.2014-08.org.nvmexpress:uuid:thurvsa-fs-postgres-test"
KEEP_ISCSI=0
KEEP_NVME=0
KEEP_CONTAINER=0
ISCSI_CONNECTED=0
NVME_CONNECTED=0
NVME_DEVICE=""
MOUNT_POINT="${TEST_DIR}/mnt"
VOLUME_NAME="vol-pg"
VOLUME_SIZE_MIB=2048
SEED=""
QUICK=0
RW_DEVICE=""

PG_CONTAINER="thur-postgres-test-$$"
PG_IMAGE=""

# Workload params — seeded picks in main().
PG_SCALE=""
PG_CLIENTS=""
PG_JOBS=""
PG_TIME=""

init_common_daemon_args
while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --transport) TRANSPORT="$2"; shift 2 ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --keep-nvme) KEEP_NVME=1; shift ;;
        --keep-container) KEEP_CONTAINER=1; shift ;;
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

case "$TRANSPORT" in
    iscsi|nvmetcp) ;;
    *) echo "Unknown --transport '$TRANSPORT' (expected iscsi or nvmetcp)"; exit 1 ;;
esac

if [[ $QUICK -eq 1 ]]; then
    VOLUME_SIZE_MIB=512
fi

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    if [[ $KEEP_CONTAINER -eq 0 ]] && podman container exists "$PG_CONTAINER" 2>/dev/null; then
        podman stop -t 5 "$PG_CONTAINER" >/dev/null 2>&1 || true
        podman rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
    fi
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        umount "$MOUNT_POINT" 2>/dev/null || true
    fi
    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsi_logout_and_delete
    fi
    if [[ $NVME_CONNECTED -eq 1 && $KEEP_NVME -eq 0 ]]; then
        nvme_tcp_disconnect
    fi
    stop_thur_daemon
    if [[ $KEEP_DATA -eq 0 ]]; then
        rm -rf "$TEST_DIR"
    else
        log_info "Keeping test directory: $TEST_DIR"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

check_prerequisites() {
    log_info "Checking prerequisites (build profile: $BUILD_PROFILE, transport: $TRANSPORT)..."
    local missing=() hints=()
    local build_cmd="cargo build --profile dev"
    [[ "$BUILD_PROFILE" == "release" ]] && build_cmd="cargo build --release"

    : "${DAEMON_PATH:=./target/$BUILD_PROFILE/thurvsad}"
    : "${CLI_PATH:=./target/$BUILD_PROFILE/thurvsa}"

    if [[ ! -x "$DAEMON_PATH" ]]; then
        if command -v thurvsad >/dev/null 2>&1; then
            DAEMON_PATH=$(command -v thurvsad)
        else
            missing+=("thurvsad"); hints+=("  - thurvsad: $build_cmd")
        fi
    fi
    if [[ ! -x "$CLI_PATH" ]]; then
        if command -v thurvsa >/dev/null 2>&1; then
            CLI_PATH=$(command -v thurvsa)
        else
            missing+=("thurvsa"); hints+=("  - thurvsa: $build_cmd")
        fi
    fi

    declare -A HINTS=(
        [podman]="sudo apt-get install podman"
        [mkfs.ext4]="sudo apt-get install e2fsprogs"
        [fsck.ext4]="sudo apt-get install e2fsprogs"
        [mount]="(util-linux — usually present)"
        [umount]="(util-linux — usually present)"
        [curl]="sudo apt-get install curl"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [nvme]="sudo apt-get install nvme-cli"
    )
    local tools=(podman mkfs.ext4 fsck.ext4 mount umount curl)
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        tools+=(iscsiadm lsscsi)
    else
        tools+=(nvme)
    fi
    for tool in "${tools[@]}"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool"); hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done

    if [[ "$TRANSPORT" == "nvmetcp" ]]; then
        if ! lsmod | grep -q '^nvme_tcp\b' && ! modinfo nvme_tcp >/dev/null 2>&1; then
            missing+=("nvme_tcp kernel module")
            hints+=("  - nvme_tcp: sudo modprobe nvme_tcp")
        fi
    fi

    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi

    if [[ "$TRANSPORT" == "iscsi" ]]; then
        if command -v systemctl >/dev/null 2>&1; then
            if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
                log_error "iscsid (open-iscsi) service is not running."
                echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
                exit 1
            fi
        fi
    else
        if ! lsmod | grep -q '^nvme_tcp\b'; then
            log_info "Loading nvme_tcp kernel module"
            modprobe nvme_tcp || { log_error "Failed to load nvme_tcp"; exit 1; }
        fi
    fi

    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

assign_ports_pg() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        [[ -z "$ISCSI_PORT" ]] && ISCSI_PORT=$(pick_free_port)
    else
        [[ -z "$NVMETCP_PORT" ]] && NVMETCP_PORT=$(pick_free_port)
    fi
    [[ -z "$HTTP_PORT" ]] && HTTP_PORT=$(pick_free_port)
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        log_info "Using iSCSI port $ISCSI_PORT, HTTP port $HTTP_PORT"
    else
        log_info "Using NVMe/TCP port $NVMETCP_PORT, HTTP port $HTTP_PORT"
    fi
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data" "$MOUNT_POINT"
    local transport_block
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        transport_block=$'iscsi:\n  listen: "127.0.0.1:'"$ISCSI_PORT"'"'
    else
        transport_block=$'transports: [nvmetcp]\nnvmetcp:\n  listen: "0.0.0.0:'"$NVMETCP_PORT"'"'
    fi
    cat > "$TEST_CONFIG" <<EOFCONFIG
data_dir: "$TEST_DIR/data"

http:
  listen: "127.0.0.1:$HTTP_PORT"

$transport_block

# /tmp is often tmpfs with little headroom — disable the free-floor so
# page-seals aren't blocked by try_reserve. Same as test-monte-carlo.sh.
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
}

start_daemon() {
    export THURVSA_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    log_info "Starting thurvsad ($TRANSPORT)..."
    local probe_port
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        probe_port="$ISCSI_PORT"
    else
        probe_port="$NVMETCP_PORT"
    fi
    RUST_LOG=info "$DAEMON_PATH" --config "$TEST_CONFIG" > "${TEST_DIR}/daemon.log" 2>&1 &
    DAEMON_PID=$!
    for _ in {1..60}; do
        if ss -tln 2>/dev/null | grep -q ":$probe_port\b"; then
            log_info "Daemon ready (PID $DAEMON_PID, port $probe_port)"
            return 0
        fi
        sleep 0.5
    done
    log_error "Daemon did not become ready"
    tail -30 "${TEST_DIR}/daemon.log"
    exit 1
}

ensure_volume() {
    if "$CLI_PATH" --config "$TEST_CONFIG" volume list 2>/dev/null | grep -q "$VOLUME_NAME"; then
        log_info "Volume $VOLUME_NAME already present"
        return 0
    fi
    log_info "Creating $VOLUME_NAME (${VOLUME_SIZE_MIB} MiB)..."
    "$CLI_PATH" --config "$TEST_CONFIG" volume create "$VOLUME_NAME" \
        --size "${VOLUME_SIZE_MIB}M" --backend local >/dev/null
}

# Transport-up primitives. Cribbed from test-monte-carlo.sh but
# simplified — we never log out and back in mid-run, so no idempotent
# "is up" guard.
connect_iscsi() {
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null \
        || { log_error "iscsi login failed"; exit 1; }
    ISCSI_CONNECTED=1
    sleep 2
    local row
    for _ in 1 2 3 4 5; do
        row=$(lsscsi -g | awk '/THUR VSA/ {print; exit}')
        [[ -n "$row" ]] && break
        sleep 1
    done
    [[ -n "$row" ]] || { log_error "iscsi login OK but no THUR VSA device appeared"; lsscsi -g; exit 1; }
    RW_DEVICE=$(echo "$row" | awk '{print $(NF-1)}')
    [[ -b "$RW_DEVICE" ]] || { log_error "$RW_DEVICE is not a block device"; exit 1; }
    log_info "thurvsa LUN -> $RW_DEVICE"
}

connect_nvme() {
    nvme_tcp_connect || exit 1
    RW_DEVICE="/dev/${NVME_DEVICE}n1"
}

transport_connect() {
    if [[ "$TRANSPORT" == "iscsi" ]]; then
        connect_iscsi
    else
        connect_nvme
    fi
}

mkfs_and_mount() {
    log_info "mkfs.ext4 + mount $RW_DEVICE -> $MOUNT_POINT"
    mkfs.ext4 -F -q "$RW_DEVICE" || { log_error "mkfs.ext4 failed"; exit 1; }
    mount "$RW_DEVICE" "$MOUNT_POINT" || { log_error "mount failed"; exit 1; }
}

# Pick workload params deterministically from the seed. Buckets are
# chosen so the workload size is bounded — scale 20 is ~320 MiB raw
# data, fits comfortably in the 2 GiB default volume.
pick_workload_params() {
    if [[ $QUICK -eq 1 ]]; then
        PG_SCALE=1
        PG_CLIENTS=4
        PG_JOBS=2
        PG_TIME=30
        return
    fi
    local scale_pick clients_pick jobs_pick time_pick
    MC_OP_INDEX=1
    scale_pick=$(mc_pick_weighted "pg-scale" "25:5" "25:10" "25:15" "25:20")
    clients_pick=$(mc_pick_weighted "pg-clients" "50:4" "50:8")
    jobs_pick=$(mc_pick_weighted "pg-jobs" "50:2" "50:4")
    time_pick=$(mc_pick_weighted "pg-time" "50:60" "50:90")
    PG_SCALE="$scale_pick"
    PG_CLIENTS="$clients_pick"
    PG_JOBS="$jobs_pick"
    PG_TIME="$time_pick"
}

# Build (or reuse) the postgres container image. Tag carries the
# Containerfile fingerprint so any edit busts the cache without manual
# pruning — same pattern as test-app-bareos.sh.
build_postgres_image() {
    log_info "Preparing postgres container image..."
    local containerfile="$TEST_DIR/Containerfile.postgres"
    cat > "$containerfile" <<'EOFCF'
FROM debian:12

ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        postgresql \
        postgresql-contrib \
        procps \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Entrypoint comes from a host bind-mount (/start-postgres.sh) so
# editing it doesn't rebuild the image.
CMD ["/start-postgres.sh"]
EOFCF

    local fp
    fp=$(sha256sum "$containerfile" | cut -c1-12)
    PG_IMAGE="thur-postgres:$fp"

    if podman image exists "$PG_IMAGE"; then
        log_info "Reusing cached image $PG_IMAGE"
        return 0
    fi
    log_info "Building $PG_IMAGE (first build is ~30-60 s)..."
    if ! podman build --tag "$PG_IMAGE" -f "$containerfile" "$TEST_DIR" >"$TEST_DIR/podman-build.log" 2>&1; then
        log_error "podman build failed; tail of build log:"
        tail -40 "$TEST_DIR/podman-build.log" | sed 's/^/  /'
        exit 1
    fi
    log_info "Built $PG_IMAGE"
}

# Entrypoint: idempotent initdb against /pgmount/pgdata, then exec
# postgres in foreground. Uses a subdir of the mount because mkfs.ext4
# leaves a lost+found at the root and initdb refuses a non-empty target.
write_postgres_entrypoint() {
    cat > "$TEST_DIR/start-postgres.sh" <<'EOFENTRY'
#!/bin/bash
set -e

PGDATA=/pgmount/pgdata
PG_VERSION=$(ls /usr/lib/postgresql/ | head -1)
PG_BIN=/usr/lib/postgresql/$PG_VERSION/bin
SOCK_DIR=/tmp/pgsock

mkdir -p "$PGDATA" "$SOCK_DIR"
chown -R postgres:postgres "$PGDATA" "$SOCK_DIR"
chmod 700 "$PGDATA"

if [[ ! -f "$PGDATA/PG_VERSION" ]]; then
    echo "[entrypoint] initdb cluster at $PGDATA..."
    su -s /bin/sh postgres -c "$PG_BIN/initdb -D $PGDATA --auth-local=trust --no-sync"
fi

# fsync stays on (default) — we WANT to exercise our fsync ordering.
# unix_socket_directories points at a tmpfs path inside the container
# so pgbench / psql via -h /tmp/pgsock work without bind-mount hassle.
exec su -s /bin/sh postgres -c \
    "$PG_BIN/postgres -D $PGDATA -k $SOCK_DIR -c listen_addresses=''"
EOFENTRY
    chmod +x "$TEST_DIR/start-postgres.sh"
}

start_postgres_container() {
    log_info "Starting postgres container ($PG_CONTAINER)..."
    # --network none: postgres listens on a Unix socket only; pgbench
    # runs via `podman exec` inside the same container.
    # No --rm: keeps logs accessible after early exit; cleanup() removes.
    if ! podman run -d \
            --name "$PG_CONTAINER" \
            --network none \
            -v "$MOUNT_POINT:/pgmount:Z" \
            -v "$TEST_DIR/start-postgres.sh:/start-postgres.sh:ro,Z" \
            "$PG_IMAGE" >"$TEST_DIR/podman-run.log" 2>&1; then
        log_error "podman run failed:"
        cat "$TEST_DIR/podman-run.log" | sed 's/^/  /'
        exit 1
    fi
    wait_postgres_ready
}

wait_postgres_ready() {
    log_info "Waiting for postgres to accept connections..."
    local deadline=$(( $(date +%s) + 60 ))
    while (( $(date +%s) < deadline )); do
        if podman exec "$PG_CONTAINER" su -s /bin/sh postgres -c \
                "psql -h /tmp/pgsock -d postgres -t -A -c 'SELECT 1'" >/dev/null 2>&1; then
            log_info "Postgres is responsive"
            return 0
        fi
        sleep 1
    done
    log_error "Postgres did not become ready within 60 s"
    podman logs "$PG_CONTAINER" 2>&1 | tail -40 | sed 's/^/  /'
    exit 1
}

# Run psql inside the container, no decoration. Stdout is the query
# result (tuples-only, unaligned).
psql_q() {
    local sql="$1"
    podman exec "$PG_CONTAINER" su -s /bin/sh postgres -c \
        "psql -h /tmp/pgsock -d postgres -t -A -c \"$sql\"" 2>&1
}

# Run pgbench inside the container.
pgbench_run() {
    podman exec "$PG_CONTAINER" su -s /bin/sh postgres -c \
        "pgbench -h /tmp/pgsock $* postgres" 2>&1
}

# Phase B verifier: row counts must match the SCALE contract.
verify_init_row_counts() {
    local expected_branches=$PG_SCALE
    local expected_tellers=$(( PG_SCALE * 10 ))
    local expected_accounts=$(( PG_SCALE * 100000 ))
    local b t a
    b=$(psql_q "SELECT COUNT(*) FROM pgbench_branches" | tr -d '[:space:]')
    t=$(psql_q "SELECT COUNT(*) FROM pgbench_tellers" | tr -d '[:space:]')
    a=$(psql_q "SELECT COUNT(*) FROM pgbench_accounts" | tr -d '[:space:]')
    log_info "  row counts: branches=$b tellers=$t accounts=$a"
    if [[ "$b" != "$expected_branches" || "$t" != "$expected_tellers" || "$a" != "$expected_accounts" ]]; then
        log_error "row count mismatch — expected branches=$expected_branches tellers=$expected_tellers accounts=$expected_accounts"
        return 1
    fi
    return 0
}

# TPC-B invariant: each pgbench transaction adds the same delta to
# accounts, tellers, and branches (and records it in history). So
# sum(accounts.abalance) == sum(tellers.tbalance) ==
# sum(branches.bbalance) == sum(history.delta) at ALL times, including
# after a crash that takes out some transactions — those transactions
# either commit all four updates (visible after WAL replay) or none.
verify_tpcb_invariant() {
    local label="$1"
    local sums
    sums=$(psql_q "SELECT
        COALESCE((SELECT SUM(abalance) FROM pgbench_accounts), 0),
        COALESCE((SELECT SUM(tbalance) FROM pgbench_tellers), 0),
        COALESCE((SELECT SUM(bbalance) FROM pgbench_branches), 0),
        COALESCE((SELECT SUM(delta) FROM pgbench_history), 0)")
    # Tuples-only / unaligned output is one row, fields separated by |.
    local a t b h
    IFS='|' read -r a t b h <<< "$sums"
    log_info "  [$label] sum(accounts)=$a sum(tellers)=$t sum(branches)=$b sum(history)=$h"
    if [[ "$a" != "$t" || "$t" != "$b" || "$b" != "$h" ]]; then
        log_fail "[$label] TPC-B invariant violated — sums do not match"
        return 1
    fi
    return 0
}

# Phase B
phase_b_init_and_verify() {
    log_info "[Phase B] pgbench -i -s $PG_SCALE (bulk init)..."
    local out
    if ! out=$(pgbench_run -i -s "$PG_SCALE" -q); then
        log_error "[Phase B] pgbench -i failed:"
        echo "$out" | tail -30 | sed 's/^/  /'
        return 1
    fi
    # Drop the noisy progress lines, keep the timing summary.
    echo "$out" | tail -5 | sed 's/^/    /'
    log_info "[Phase B] verifying row counts + initial invariant..."
    verify_init_row_counts || return 1
    verify_tpcb_invariant "post-init" || return 1
    return 0
}

# Phase C — clean OLTP workload.
phase_c_clean_workload() {
    log_info "[Phase C] pgbench -c $PG_CLIENTS -j $PG_JOBS -T $PG_TIME (clean OLTP)..."
    local out
    if ! out=$(pgbench_run -c "$PG_CLIENTS" -j "$PG_JOBS" -T "$PG_TIME" -n); then
        log_error "[Phase C] pgbench OLTP run failed:"
        echo "$out" | tail -30 | sed 's/^/  /'
        return 1
    fi
    echo "$out" | tail -10 | sed 's/^/    /'
    # Quick sanity: history rowcount must have grown.
    local hist
    hist=$(psql_q "SELECT COUNT(*) FROM pgbench_history" | tr -d '[:space:]')
    log_info "[Phase C] history rows: $hist"
    if (( hist < 1 )); then
        log_error "[Phase C] no transactions recorded — workload did nothing"
        return 1
    fi
    log_info "[Phase C] verifying invariant after clean shutdown of workload..."
    verify_tpcb_invariant "post-clean" || return 1
    return 0
}

# Phase D — crash recovery. SIGKILL the container mid-workload, fsck
# the volume, restart postgres, re-verify invariant.
phase_d_crash_recovery() {
    local crash_time
    crash_time=$(( PG_TIME / 2 ))
    (( crash_time < 10 )) && crash_time=10

    log_info "[Phase D] launching pgbench in background ($PG_TIME s target)..."
    # Use a longer target than the kill delay so pgbench is *definitely*
    # mid-transaction when the kill lands.
    local bg_log="$TEST_DIR/pgbench-bg.log"
    podman exec "$PG_CONTAINER" su -s /bin/sh postgres -c \
        "pgbench -h /tmp/pgsock -c $PG_CLIENTS -j $PG_JOBS -T $(( PG_TIME * 2 )) -n postgres" \
        > "$bg_log" 2>&1 &
    local bg_pid=$!

    log_info "[Phase D] waiting ${crash_time} s, then SIGKILL the container..."
    sleep "$crash_time"

    # Snapshot the in-flight history row count so we can prove we
    # actually caught the workload in flight.
    local hist_pre
    hist_pre=$(psql_q "SELECT COUNT(*) FROM pgbench_history" 2>/dev/null | tr -d '[:space:]')
    log_info "[Phase D] history rows just before crash: ${hist_pre:-?}"

    # SIGKILL the container PID 1 (the entrypoint script that exec'd
    # postgres). Every postgres backend goes down without a chance to
    # flush. The `podman exec` pgbench child is killed too — that's
    # what we want.
    if ! podman kill --signal=KILL "$PG_CONTAINER" >/dev/null 2>&1; then
        log_error "[Phase D] podman kill failed"
        return 1
    fi
    # Reap the host-side pgbench wrapper. exec_pid exit is non-zero
    # (its server died) — that's fine.
    wait "$bg_pid" 2>/dev/null || true
    podman wait "$PG_CONTAINER" >/dev/null 2>&1 || true
    log_info "[Phase D] container exited; podman exit code: $(podman inspect --format '{{.State.ExitCode}}' "$PG_CONTAINER" 2>&1)"

    # Remove the stopped container so a fresh one can re-use the name.
    podman rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true

    # Unmount, fsck (ext4 must be intact), remount. fsck.ext4 -fn is
    # read-only check; non-zero rc means filesystem-level damage, which
    # would point at thurvsa losing previously-fsynced writes.
    log_info "[Phase D] umount + fsck.ext4 -fn (read-only integrity check)..."
    if ! umount "$MOUNT_POINT"; then
        log_error "[Phase D] umount failed"
        return 1
    fi
    local fsck_out fsck_rc
    fsck_out=$(fsck.ext4 -fn "$RW_DEVICE" 2>&1) || fsck_rc=$?
    fsck_rc=${fsck_rc:-0}
    echo "$fsck_out" | tail -5 | sed 's/^/    /'
    if (( fsck_rc != 0 )); then
        log_fail "[Phase D] fsck.ext4 returned $fsck_rc — filesystem damage after crash"
        return 1
    fi
    log_info "[Phase D] filesystem clean; remounting..."
    if ! mount "$RW_DEVICE" "$MOUNT_POINT"; then
        log_error "[Phase D] remount failed"
        return 1
    fi

    log_info "[Phase D] restarting postgres container — WAL replay should fire..."
    start_postgres_container

    local hist_post
    hist_post=$(psql_q "SELECT COUNT(*) FROM pgbench_history" | tr -d '[:space:]')
    log_info "[Phase D] history rows after WAL replay: $hist_post"

    log_info "[Phase D] verifying invariant after crash + WAL replay..."
    verify_tpcb_invariant "post-recovery" || return 1
    return 0
}

main() {
    echo "========================================"
    echo "Thur VSA Postgres-Driven Block-Storage Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports_pg
    create_test_config
    start_daemon
    ensure_volume
    transport_connect
    mkfs_and_mount

    # Seeded workload picks. mc_seed_init prints the seed banner.
    mc_seed_init "$SEED" "$TEST_DIR/ops.log"
    pick_workload_params
    log_info "Workload: scale=$PG_SCALE clients=$PG_CLIENTS jobs=$PG_JOBS T=${PG_TIME}s"

    build_postgres_image
    write_postgres_entrypoint
    start_postgres_container

    echo ""
    log_test "Phase B — pgbench -i + row counts + initial invariant"
    if phase_b_init_and_verify; then
        log_pass "Phase B"
    else
        log_fail "Phase B"
        exit 1
    fi

    echo ""
    log_test "Phase C — clean OLTP workload + invariant"
    if phase_c_clean_workload; then
        log_pass "Phase C"
    else
        log_fail "Phase C"
        exit 1
    fi

    echo ""
    log_test "Phase D — SIGKILL mid-workload + fsck + WAL replay + invariant"
    if phase_d_crash_recovery; then
        log_pass "Phase D"
    else
        log_fail "Phase D"
        echo ""
        echo "Reproduce with: --seed $MC_SEED --transport $TRANSPORT"
        exit 1
    fi

    echo ""
    echo "========================================"
    log_pass "All postgres workload phases passed (seed=$MC_SEED, transport=$TRANSPORT)"
    echo "========================================"
    echo "  reusable reproducer: --seed $MC_SEED --transport $TRANSPORT"
    echo "  container logs:      podman logs $PG_CONTAINER"
    echo "  daemon log:          $TEST_DIR/daemon.log"
    exit 0
}

main
