#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
#
# Thur VTL Bareos-Driven Backup Test
#
# Drives a real Bareos backup server (in podman) against thurvtld. Catches
# gaps that test-monte-carlo.sh misses: Bareos volume-label format, the
# mtx-changer script invocations Bareos actually issues, autochanger
# reservation under concurrent multi-drive jobs.
#
# Workflow:
#   1. thurvtld up with 2 drives, ~6 cartridges, local backend.
#   2. iSCSI login on the host; resolve /dev/sgN changer + /dev/nstN drives.
#   3. Build a small Bareos all-in-one container (debian:12 + bareos-21 +
#      SQLite catalog) from an inline Containerfile. Image cached after
#      the first build; tag includes a Containerfile-content fingerprint.
#   4. Run the container --privileged with the 3 SCSI devices mapped in.
#   5. Generate dir/SD/FD/bconsole configs (autochanger + 2 devices, pool
#      of N tape volumes, one FileSet per backup job).
#   6. Label every cartridge via bconsole.
#   7. Run a seeded random number of small (1-10 MiB) backup jobs with
#      Maximum Concurrent Jobs = 2 so both drives engage.
#   8. Restore every job; diff the restored tree byte-for-byte against the
#      original fileset.
#
# Reproducibility: --seed N replays the exact same op sequence + content.
# --quick uses 4 jobs (default 8).
#
# Prerequisites (all auto-checked):
#   - podman             (sudo apt-get install podman)
#   - mtx, mt-st         (sudo apt-get install mtx mt-st)
#   - open-iscsi         (sudo apt-get install open-iscsi)
#   - lsscsi, curl       (sudo apt-get install lsscsi curl)
#   - iscsid running     (sudo systemctl enable --now iscsid open-iscsi)
#   - Root/sudo access (self-elevates via NOPASSWD sudoers)
#
# Usage:
#   ./vtl/scripts/test-app-bareos.sh [OPTIONS]
#
# Options:
#   --seed N              Reproduce a prior run
#   --jobs N              Override backup-job count (default 8)
#   --quick               4 jobs (~3 min total wall clock)
#   --release             Use ./target/release/ binaries
#   --daemon-path PATH    Override thurvtld
#   --cli-path PATH       Override thurvtl
#   --keep-data           Don't clean up test data directory
#   --keep-iscsi          Don't disconnect iSCSI on exit
#   --keep-container      Don't stop/remove the bareos container on exit
#   --iscsi-port PORT     Override iSCSI port
#   --http-port PORT      Override HTTP port
#

# Self-elevate via sudo. The script touches /dev/sgN + /dev/nstN, runs
# iscsiadm + mtx, and binds to privileged operations through podman
# --privileged. NOPASSWD sudoers entry assumed.
if [[ $EUID -ne 0 ]]; then
    echo "[INFO] Re-executing under sudo..."
    exec sudo "$0" "$@"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
source "${SCRIPT_DIR}/../../scripts/lib/test-helpers.sh"
source "${SCRIPT_DIR}/../../scripts/lib/monte-carlo.sh"

BUILD_PROFILE="debug"
DAEMON_PATH=""
CLI_PATH=""
TEST_DIR="/tmp/test-app-bareos-$$"
TEST_CONFIG="${TEST_DIR}/config.yaml"
ISCSI_PORT=""
HTTP_PORT=""
TARGET_IQN="iqn.2025-10.com.metebalci:thurvtl"
KEEP_DATA=0
KEEP_ISCSI=0
KEEP_CONTAINER=0
DAEMON_PID=""
ISCSI_CONNECTED=0
SEED=""
QUICK=0
JOBS=""
CHANGER_DEVICE_SG=""    # host path, e.g. /dev/sg3
TAPE_DEVICES_NST=()     # host paths for the 2 drive no-rewind devices

# Chassis: 6 cartridges across 10 slots, 2 drives.
CARTS=(BR01L8 BR02L8 BR03L8 BR04L8 BR05L8 BR06L8)
NUM_SLOTS=10
NUM_DRIVES=2

BAREOS_CONTAINER="thur-bareos-test-$$"
BAREOS_IMAGE=""        # filled in by build_bareos_image
BAREOS_DIR_PASS=""
BAREOS_SD_PASS=""
BAREOS_FD_PASS=""
BAREOS_MON_PASS=""

# Per-job state: parallel arrays indexed 1..N. JOB_IDS[i] holds the
# bareos JobId returned by `run` (resolved later via bconsole), used to
# correlate restore with backup.
declare -A JOB_BACKUP_ID   # bareos jobid for backup of job i
declare -A JOB_RESTORE_OK  # 1 if restore-and-diff succeeded for job i

while [[ $# -gt 0 ]]; do
    case $1 in
        --seed) SEED="$2"; shift 2 ;;
        --jobs) JOBS="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --release) BUILD_PROFILE="release"; shift ;;
        --daemon-path) DAEMON_PATH="$2"; shift 2 ;;
        --cli-path) CLI_PATH="$2"; shift 2 ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --keep-iscsi) KEEP_ISCSI=1; shift ;;
        --keep-container) KEEP_CONTAINER=1; shift ;;
        --iscsi-port) ISCSI_PORT="$2"; shift 2 ;;
        --http-port) HTTP_PORT="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

[[ $QUICK -eq 1 ]] && : "${JOBS:=4}"
: "${JOBS:=8}"

log_pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*"; }

cleanup() {
    local rc=$?
    log_info "Cleaning up..."
    if [[ $KEEP_CONTAINER -eq 0 ]] && podman container exists "$BAREOS_CONTAINER" 2>/dev/null; then
        podman stop -t 5 "$BAREOS_CONTAINER" >/dev/null 2>&1 || true
        podman rm -f "$BAREOS_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [[ $ISCSI_CONNECTED -eq 1 && $KEEP_ISCSI -eq 0 ]]; then
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --logout 2>/dev/null || true
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --op delete 2>/dev/null || true
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
        [podman]="sudo apt-get install podman"
        [mtx]="sudo apt-get install mtx"
        [mt]="sudo apt-get install mt-st"
        [iscsiadm]="sudo apt-get install open-iscsi"
        [lsscsi]="sudo apt-get install lsscsi"
        [curl]="sudo apt-get install curl"
        [openssl]="(usually present)"
        [cmp]="(diffutils — usually present)"
        [diff]="(diffutils — usually present)"
        [tar]="(usually present)"
    )
    for tool in podman mtx mt iscsiadm lsscsi curl openssl cmp diff tar; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool"); hints+=("  - $tool: ${HINTS[$tool]}")
        fi
    done
    if (( ${#missing[@]} > 0 )); then
        log_error "Missing prerequisites: ${missing[*]}"
        printf '%s\n' "${hints[@]}"
        exit 1
    fi
    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl is-active --quiet iscsid 2>/dev/null && ! systemctl is-active --quiet open-iscsi 2>/dev/null; then
            log_error "iscsid (open-iscsi) service is not running."
            echo "Start it with: sudo systemctl enable --now iscsid open-iscsi"
            exit 1
        fi
    fi
    log_info "All prerequisites met (daemon=$DAEMON_PATH, cli=$CLI_PATH)"
}

create_test_config() {
    log_info "Creating test configuration..."
    mkdir -p "$TEST_DIR/data" "$TEST_DIR/backup-src" "$TEST_DIR/restore" "$TEST_DIR/bareos-conf" "$TEST_DIR/bareos-log"
    if [[ -n "$SUDO_USER" ]]; then
        chown -R "$SUDO_USER":"$(id -gn "$SUDO_USER")" "$TEST_DIR"
    fi
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

# /tmp is often tmpfs with little headroom — disable the free-floor so
# chunk-seals aren't blocked by try_reserve. Same as test-monte-carlo.sh.
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
    export THURVTL_ADMIN_SOCKET="${TEST_DIR}/admin.sock"
    start_thur_daemon
}

create_cartridges() {
    local c
    for c in "${CARTS[@]}"; do
        if ! "$CLI_PATH" --config "$TEST_CONFIG" cartridge create "$c" --lto-generation 8 >/dev/null 2>&1; then
            log_error "cartridge create $c failed"
            tail -20 "${TEST_DIR}/daemon.log"
            exit 1
        fi
    done
    log_info "Created ${#CARTS[@]} cartridges: ${CARTS[*]}"
}

connect_iscsi() {
    # Stale sessions / node records from earlier test runs leave dead
    # /dev/sgN nodes on the host whose major:minor still resolves but
    # whose backend has gone away — the new run binds to one of those
    # and every SCSI op then returns ENXIO. Drop every existing record
    # for our IQN before discovering this run's target.
    log_info "Purging any stale iSCSI sessions/nodes for $TARGET_IQN..."
    while read -r portal; do
        [[ -n "$portal" ]] || continue
        iscsiadm -m node --targetname "$TARGET_IQN" --portal "$portal" --logout >/dev/null 2>&1 || true
    done < <(iscsiadm -m session 2>/dev/null | awk -v iqn="$TARGET_IQN" '$0 ~ iqn { print $3 }' | sed 's/,.*//')
    iscsiadm -m node --targetname "$TARGET_IQN" -o delete >/dev/null 2>&1 || true

    log_info "Connecting to iSCSI target..."
    iscsiadm -m discovery -t sendtargets -p "127.0.0.1:$ISCSI_PORT" >/dev/null 2>&1 || true
    iscsiadm -m node --targetname "$TARGET_IQN" --portal "127.0.0.1:$ISCSI_PORT" --login >/dev/null 2>&1 \
        || { log_error "iscsi login failed"; exit 1; }
    ISCSI_CONNECTED=1
    sleep 3

    CHANGER_DEVICE_SG=$(lsscsi -g | awk '/mediumx/{print $NF}' | head -1)
    [[ -n "$CHANGER_DEVICE_SG" ]] || { log_error "Changer sg device not found"; lsscsi -g; exit 1; }

    # Two tape devices in LUN order. The kernel scans LUN 1 (drive 0) and
    # LUN 2 (drive 1) into st devices in that order, so sort by name and
    # take the first two. Assumes no other tape devices on this host —
    # consistent with the rest of the integration-test fleet.
    local sts=()
    mapfile -t sts < <(lsscsi | awk '/tape/{print $NF}' | sort)
    if (( ${#sts[@]} < 2 )); then
        log_error "Expected at least 2 tape devices, got ${#sts[@]}: ${sts[*]}"
        lsscsi
        exit 1
    fi
    TAPE_DEVICES_NST=()
    local i
    for (( i=0; i<2; i++ )); do
        TAPE_DEVICES_NST+=( "${sts[$i]/\/st/\/nst}" )
    done
    log_info "Changer (sg): $CHANGER_DEVICE_SG"
    log_info "Drives (nst): ${TAPE_DEVICES_NST[*]}"

    # Warm up: clear pending unit-attention from the iSCSI login so the
    # container's first mtx call doesn't trip on a stale UA.
    mtx -f "$CHANGER_DEVICE_SG" status >/dev/null 2>&1 || true
}

# Bareos passwords — random per run. UUIDs are 36 chars of [0-9a-f-]; fine
# for bareos's password directive which accepts arbitrary tokens.
generate_bareos_passwords() {
    BAREOS_DIR_PASS=$(openssl rand -hex 16)
    BAREOS_SD_PASS=$(openssl rand -hex 16)
    BAREOS_FD_PASS=$(openssl rand -hex 16)
    BAREOS_MON_PASS=$(openssl rand -hex 16)
}

# Build (or reuse) the bareos container image. Tag carries a fingerprint
# of the Containerfile bytes, so any edit busts the cache without manual
# pruning.
build_bareos_image() {
    log_info "Preparing bareos container image..."
    local containerfile="$TEST_DIR/Containerfile.bareos"
    cat > "$containerfile" <<'EOFCF'
FROM debian:12

ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C

# Bareos was removed from Debian 12. Pull from the upstream community
# repo at download.bareos.org/current. PostgreSQL is the only catalog
# backend in current bareos (sqlite was dropped years ago) — postgres
# bootstraps at container start (see start-bareos.sh).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        curl ca-certificates gnupg \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://download.bareos.org/current/Debian_12/Release.key \
        | gpg --dearmor -o /etc/apt/keyrings/bareos.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/bareos.gpg] https://download.bareos.org/current/Debian_12 /" \
        > /etc/apt/sources.list.d/bareos.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        bareos-director \
        bareos-storage-tape \
        bareos-filedaemon \
        bareos-database-postgresql \
        bareos-database-tools \
        bareos-bconsole \
        bareos-tools \
        postgresql \
        mtx \
        mt-st \
        procps \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Configs + entrypoint come from host bind-mounts (/etc/bareos and
# /start-bareos.sh respectively), so editing them doesn't rebuild the
# image.
CMD ["/start-bareos.sh"]
EOFCF

    local fp
    fp=$(sha256sum "$containerfile" | cut -c1-12)
    BAREOS_IMAGE="thur-bareos:$fp"

    if podman image exists "$BAREOS_IMAGE"; then
        log_info "Reusing cached image $BAREOS_IMAGE"
        return 0
    fi
    log_info "Building $BAREOS_IMAGE (first build is ~60-90 s)..."
    if ! podman build --tag "$BAREOS_IMAGE" -f "$containerfile" "$TEST_DIR" >"$TEST_DIR/podman-build.log" 2>&1; then
        log_error "podman build failed; tail of build log:"
        tail -40 "$TEST_DIR/podman-build.log" | sed 's/^/  /'
        exit 1
    fi
    log_info "Built $BAREOS_IMAGE"
}

write_bareos_entrypoint() {
    cat > "$TEST_DIR/start-bareos.sh" <<'EOFENTRY'
#!/bin/bash
set -e
mkdir -p /var/log/bareos /var/run/bareos
chown -R bareos:bareos /var/log/bareos /var/run/bareos /etc/bareos /var/lib/bareos

# --- PostgreSQL bring-up -----------------------------------------------
# Bareos current dropped SQLite, so we need a real database. Postgres is
# bootstrapped on every container start (no systemd inside; use
# pg_ctlcluster directly). The catalog bootstrap is idempotent — if
# something replays the container, `create_bareos_database` will say
# "already exists" and we continue.
PG_VERSION=$(ls /usr/lib/postgresql/ | head -1)
# bareos-dir runs as the `bareos` OS user but peer-auths against the
# `bareos` DB user, which fails unless we relax pg_hba.conf. This is a
# throwaway test container — trust auth on the local socket is fine.
sed -i 's/\(local[[:space:]].*[[:space:]]\)peer$/\1trust/; s/\(local[[:space:]].*[[:space:]]\)md5$/\1trust/' \
    "/etc/postgresql/$PG_VERSION/main/pg_hba.conf"
echo "[entrypoint] starting postgresql $PG_VERSION..."
pg_ctlcluster "$PG_VERSION" main start
for _ in {1..30}; do
    if su -s /bin/sh postgres -c "psql -lqt" >/dev/null 2>&1; then break; fi
    sleep 1
done
if ! su -s /bin/sh postgres -c "psql -lqt" >/dev/null 2>&1; then
    echo "[entrypoint] postgresql failed to become ready" >&2
    exit 1
fi

# Bareos catalog bootstrap is idempotent if the DB already exists, but
# the make_bareos_tables script doesn't gate on existence — running it
# twice errors. Gate ourselves on the presence of the bareos DB.
if ! su -s /bin/sh postgres -c "psql -lqt | cut -d \\| -f1 | grep -qw bareos"; then
    echo "[entrypoint] creating bareos catalog..."
    su -s /bin/sh postgres -c "/usr/lib/bareos/scripts/create_bareos_database postgresql"
    su -s /bin/sh postgres -c "/usr/lib/bareos/scripts/make_bareos_tables    postgresql"
    su -s /bin/sh postgres -c "/usr/lib/bareos/scripts/grant_bareos_privileges postgresql"
fi

# Validate configs up front so syntax errors abort the container fast.
/usr/sbin/bareos-fd  -c /etc/bareos/bareos-fd.conf  -t
/usr/sbin/bareos-sd  -c /etc/bareos/bareos-sd.conf  -t
/usr/sbin/bareos-dir -c /etc/bareos/bareos-dir.conf -t

# All three daemons in foreground. wait -n returns on the first to exit
# so a crashing daemon brings the container down (visible to the host
# harness via container exit status).
/usr/sbin/bareos-fd  -c /etc/bareos/bareos-fd.conf  -f &
/usr/sbin/bareos-sd  -c /etc/bareos/bareos-sd.conf  -f &
/usr/sbin/bareos-dir -c /etc/bareos/bareos-dir.conf -f &

wait -n
echo "[entrypoint] a bareos daemon exited; tearing down" >&2
kill 0 2>/dev/null || true
wait
EOFENTRY
    chmod +x "$TEST_DIR/start-bareos.sh"
}

# Write Bareos configs to $TEST_DIR/bareos-conf. Mounted into the
# container at /etc/bareos.
#
# Resource passwords (DIR/SD/FD) link the three daemons:
#   - Director -> Storage: dir's `Storage { Password = SD_PASS }` must
#     match SD's `Director { Password = SD_PASS }`.
#   - Director -> Client (FD): same shape, FD_PASS.
#   - bconsole -> Director: bconsole's `Director { Password = DIR_PASS }`
#     must match director's `Director { Password = DIR_PASS }`.
write_bareos_configs() {
    local conf="$TEST_DIR/bareos-conf"
    rm -rf "$conf"
    mkdir -p "$conf"

    # --- bareos-dir.conf ---
    {
        cat <<EOFDIR
Director {
  Name = bareos-dir
  QueryFile = "/usr/lib/bareos/scripts/query.sql"
  Maximum Concurrent Jobs = 10
  Password = "$BAREOS_DIR_PASS"
  Messages = Daemon
  Auditing = no
}

JobDefs {
  Name = "DefaultJob"
  Type = Backup
  Level = Full
  Client = bareos-fd
  FileSet = "Empty"
  Storage = ThurVTL
  Messages = Standard
  Pool = TapePool
  Priority = 10
  Max Run Time = 1800
  Write Bootstrap = "/var/lib/bareos/%c.bsr"
}

FileSet {
  Name = "Empty"
  Include {
    Options { signature = MD5 }
    File = /dev/null
  }
}

Job {
  Name = "RestoreFiles"
  Type = Restore
  Client = bareos-fd
  FileSet = "Empty"
  Storage = ThurVTL
  Pool = TapePool
  Messages = Standard
  Where = /restore
}

Client {
  Name = bareos-fd
  Address = 127.0.0.1
  Password = "$BAREOS_FD_PASS"
  File Retention = 30 days
  Job Retention = 6 months
  AutoPrune = no
}

Storage {
  Name = ThurVTL
  Address = 127.0.0.1
  Password = "$BAREOS_SD_PASS"
  Device = ThurVTL
  Media Type = LTO-8
  Autochanger = yes
  Maximum Concurrent Jobs = 10
}

Catalog {
  Name = MyCatalog
  dbname = "bareos"
  user = "bareos"
  dbpassword = ""
}

Messages {
  Name = Standard
  console = all, !skipped, !saved, !audit
  catalog = all, !skipped, !audit
  append = "/var/log/bareos/bareos-dir.log" = all, !skipped, !audit
}

Messages {
  Name = Daemon
  console = all, !skipped, !audit
  append = "/var/log/bareos/bareos-dir.log" = all, !skipped, !audit
}

Pool {
  Name = TapePool
  Pool Type = Backup
  Recycle = no
  AutoPrune = no
  Maximum Volume Jobs = 200
  Storage = ThurVTL
  LabelFormat = "BR"
}

Console {
  Name = bareos-mon
  Password = "$BAREOS_MON_PASS"
  CommandACL = status, .status
}
EOFDIR

        # One Job + one FileSet per backup we plan to run. We could share
        # a single FileSet across jobs and override at run time, but
        # per-job FileSets keep the dir.conf self-documenting (the user
        # can read it and see the path mapping at a glance).
        local i
        for (( i=1; i<=JOBS; i++ )); do
            cat <<EOFJOB
FileSet {
  Name = "FS-$i"
  Include {
    Options { signature = MD5 }
    File = /backup-src/job-$i
  }
}

Job {
  Name = "Backup-$i"
  JobDefs = "DefaultJob"
  FileSet = "FS-$i"
  Maximum Concurrent Jobs = 2
}
EOFJOB
        done
    } > "$conf/bareos-dir.conf"

    # --- bareos-sd.conf ---
    cat > "$conf/bareos-sd.conf" <<EOFSD
Storage {
  Name = bareos-sd
  Maximum Concurrent Jobs = 20
  SDPort = 9103
}

Director {
  Name = bareos-dir
  Password = "$BAREOS_SD_PASS"
}

Director {
  Name = bareos-mon
  Password = "$BAREOS_MON_PASS"
  Monitor = yes
}

Autochanger {
  Name = ThurVTL
  Device = drive-0, drive-1
  Changer Device = /dev/sg0
  Changer Command = "/usr/lib/bareos/scripts/mtx-changer %c %o %S %a %d"
}

Device {
  Name = drive-0
  Drive Index = 0
  Media Type = LTO-8
  Archive Device = /dev/nst0
  AutoChanger = yes
  RemovableMedia = yes
  RandomAccess = no
  AlwaysOpen = yes
  AutomaticMount = yes
  Maximum Concurrent Jobs = 1
  Maximum File Size = 2 GB
}

Device {
  Name = drive-1
  Drive Index = 1
  Media Type = LTO-8
  Archive Device = /dev/nst1
  AutoChanger = yes
  RemovableMedia = yes
  RandomAccess = no
  AlwaysOpen = yes
  AutomaticMount = yes
  Maximum Concurrent Jobs = 1
  Maximum File Size = 2 GB
}

Messages {
  Name = Standard
  director = bareos-dir = all, !skipped, !audit
  append = "/var/log/bareos/bareos-sd.log" = all, !skipped, !audit
}
EOFSD

    # --- bareos-fd.conf ---
    cat > "$conf/bareos-fd.conf" <<EOFFD
Director {
  Name = bareos-dir
  Password = "$BAREOS_FD_PASS"
}

Director {
  Name = bareos-mon
  Password = "$BAREOS_MON_PASS"
  Monitor = yes
}

FileDaemon {
  Name = bareos-fd
  FDPort = 9102
  Maximum Concurrent Jobs = 10
}

Messages {
  Name = Standard
  director = bareos-dir = all, !skipped, !audit
  append = "/var/log/bareos/bareos-fd.log" = all, !skipped, !audit
}
EOFFD

    # --- mtx-changer.conf ---
    # The bareos package ships this file at /etc/bareos/mtx-changer.conf,
    # but our bind-mount over /etc/bareos hides it. Without it the
    # mtx-changer script bails on every autochanger call.
    cat > "$conf/mtx-changer.conf" <<'EOFMTX'
DEBUG_LOG_FILE_DEFAULT="/var/log/bareos/mtx-changer.log"
MTX="/usr/sbin/mtx"
SLEEP_TIME=1
LOADED_TIMEOUT=300
INVENTORY_TIMEOUT=300
LOAD_SLEEP=0
OFFLINE=0
OFFLINE_SLEEP=0
LOAD_TIMEOUT=300
UNLOAD_TIMEOUT=300
TAPE_DRIVE_REWIND_TIMEOUT=300
EOFMTX

    # --- bconsole.conf ---
    cat > "$conf/bconsole.conf" <<EOFBC
Director {
  Name = bareos-dir
  DIRport = 9101
  address = 127.0.0.1
  Password = "$BAREOS_DIR_PASS"
}
EOFBC

    chmod -R go+rX "$conf"
    log_info "Bareos configs written to $conf"
}

start_bareos_container() {
    log_info "Starting bareos container ($BAREOS_CONTAINER)..."
    # --device passes both the device node and the cgroup allow rule.
    # Normalize host /dev/sgN -> /dev/sg0 in container and the two
    # /dev/nstN -> /dev/nst0,/dev/nst1 so the SD config is portable
    # across runs (host indices vary by what other devices exist).
    local device_args=(
        --device "$CHANGER_DEVICE_SG:/dev/sg0"
        --device "${TAPE_DEVICES_NST[0]}:/dev/nst0"
        --device "${TAPE_DEVICES_NST[1]}:/dev/nst1"
    )
    # Do NOT use --privileged: it bind-mounts the host /dev into the
    # container, which silently overrides our --device path renames
    # (e.g. /dev/sg0 inside the container would be the host's /dev/sg0,
    # not our remapped changer). Pass just CAP_SYS_RAWIO + CAP_SYS_ADMIN
    # for the SG_IO ioctls plus pg_ctlcluster setuid; --device handles
    # the cgroup device-allow rule.
    # No --rm: we want logs accessible after early exit. cleanup() removes
    # the container.
    if ! podman run -d \
        --name "$BAREOS_CONTAINER" \
        --cap-add SYS_RAWIO \
        --cap-add SYS_ADMIN \
        --network host \
        "${device_args[@]}" \
        -v "$TEST_DIR/bareos-conf:/etc/bareos:Z" \
        -v "$TEST_DIR/start-bareos.sh:/start-bareos.sh:ro,Z" \
        -v "$TEST_DIR/backup-src:/backup-src:ro,Z" \
        -v "$TEST_DIR/restore:/restore:Z" \
        -v "$TEST_DIR/bareos-log:/var/log/bareos:Z" \
        "$BAREOS_IMAGE" >"$TEST_DIR/podman-run.log" 2>&1; then
        log_error "podman run failed:"
        cat "$TEST_DIR/podman-run.log" | sed 's/^/  /'
        exit 1
    fi
    log_info "Container started; waiting for director to respond..."

    # bconsole returns 0 + a banner once the director is listening on
    # 9101. Poll with a hard ceiling.
    local deadline=$(( $(date +%s) + 60 ))
    while (( $(date +%s) < deadline )); do
        if echo "quit" | bconsole_exec >/dev/null 2>&1; then
            log_info "Bareos director responsive"
            return 0
        fi
        sleep 1
    done
    log_error "Bareos director did not respond within 60 s"
    if podman container exists "$BAREOS_CONTAINER" 2>/dev/null; then
        log_error "Container status: $(podman inspect --format '{{.State.Status}}' "$BAREOS_CONTAINER" 2>&1)"
        log_error "Container exit code: $(podman inspect --format '{{.State.ExitCode}}' "$BAREOS_CONTAINER" 2>&1)"
        log_error "Container logs (tail 60):"
        podman logs "$BAREOS_CONTAINER" 2>&1 | tail -60 | sed 's/^/  /'
    else
        log_error "Container vanished before we could inspect — was likely a podman startup error."
    fi
    exit 1
}

# Pipe stdin into bconsole inside the container. Stdout is the bconsole
# transcript (banner + prompts + responses); callers grep for the bit
# they need.
bconsole_exec() {
    podman exec -i "$BAREOS_CONTAINER" \
        bconsole -c /etc/bareos/bconsole.conf 2>&1
}

label_cartridges() {
    log_info "Labeling ${#CARTS[@]} cartridges via bconsole..."
    local i=0 c
    for c in "${CARTS[@]}"; do
        i=$(( i + 1 ))
        # Slot is 1-indexed in mtx and Bareos. The cartridge was placed
        # in the next free slot at create time, so cartridge i lives in
        # slot i. We label via drive 0 sequentially — labeling is itself
        # serializing (one autoload per cart), so concurrent labels
        # would just contend on the same drive anyway.
        local out
        if ! out=$(bconsole_exec <<EOFLABEL
label storage=ThurVTL pool=TapePool volume=$c slot=$i drive=0
yes
quit
EOFLABEL
); then
            log_error "bconsole label exited non-zero for $c"
            echo "$out" | sed 's/^/  /'
            return 1
        fi
        if ! grep -q "OK label" <<< "$out"; then
            log_error "label of $c did not return 'OK label':"
            echo "$out" | sed 's/^/  /'
            return 1
        fi
        log_info "  labeled $c (slot $i)"
    done
    # After labels we explicitly unmount drive 0 so the first backup job
    # doesn't trip over a held volume.
    bconsole_exec <<EOFRELEASE >/dev/null 2>&1 || true
release storage=ThurVTL drive=0
release storage=ThurVTL drive=1
quit
EOFRELEASE
}

# Generate the per-job fixture trees that Bareos will back up. Each job
# gets a directory under $TEST_DIR/backup-src/job-N with 4-12 files of
# 1 KiB..1 MiB, total ~1-10 MiB. Content + counts + sizes are seeded so
# --seed N reproduces exactly.
generate_fixtures() {
    log_info "Generating $JOBS backup fixtures (seeded)..."
    local i j n size file
    for (( i=1; i<=JOBS; i++ )); do
        MC_OP_INDEX="$i"
        mkdir -p "$TEST_DIR/backup-src/job-$i"
        n=$(( $(mc_rng_u32 "n-files" 9) + 4 ))     # 4..12
        for (( j=0; j<n; j++ )); do
            MC_OP_INDEX="$((i * 100 + j))"
            # Sizes 1 KiB..1 MiB, log-ish distribution via two-stage bucket.
            local bucket
            bucket=$(mc_pick_weighted "size-bucket" \
                "40:tiny" "40:small" "20:medium")
            case "$bucket" in
                tiny)   size=$(( $(mc_rng_u32 "size" 4096) + 1024 )) ;;     # 1-5 KiB
                small)  size=$(( $(mc_rng_u32 "size" 102400) + 4096 )) ;;   # 4-104 KiB
                medium) size=$(( $(mc_rng_u32 "size" 950000) + 65536 )) ;;  # 64 KiB..~1 MiB
            esac
            file="$TEST_DIR/backup-src/job-$i/file-$j.bin"
            mc_content_to "job-$i" "$j" "$size" "$file"
        done
        local total
        total=$(du -sb "$TEST_DIR/backup-src/job-$i" | awk '{print $1}')
        log_info "  job-$i: $n files, total $((total/1024)) KiB"
    done
    # Permissions: the bind-mount carries host uid/gid. The bareos-fd in
    # the container reads as root (privileged container), so 0755 dirs +
    # 0644 files are visible. Stamp explicitly to defend against umask.
    chmod -R a+rX "$TEST_DIR/backup-src"
}

# Run all backup jobs. Each `run job=Backup-i yes` returns a JobId we
# need for later restore/diff. We submit all jobs back-to-back; the
# Director scheduler queues them and Max Concurrent Jobs = 2 keeps two
# running at any moment — both drives engage.
run_backup_jobs() {
    log_info "Submitting $JOBS backup jobs (concurrency cap = 2)..."
    local i out jobid
    for (( i=1; i<=JOBS; i++ )); do
        out=$(bconsole_exec <<EOFRUN
run job=Backup-$i yes
.
quit
EOFRUN
)
        # bconsole prints "Job queued. JobId=<N>" on success.
        jobid=$(echo "$out" | awk '/Job queued\. JobId=/ { sub(/.*JobId=/, ""); print; exit }')
        if [[ -z "$jobid" ]]; then
            log_error "Failed to queue Backup-$i; bconsole output:"
            echo "$out" | sed 's/^/  /'
            return 1
        fi
        JOB_BACKUP_ID[$i]="$jobid"
        log_info "  Backup-$i queued as JobId=$jobid"
    done

    # Wait for every queued job to terminate. `llist jobid=N` returns
    # one block of key:value lines including `jobstatus:` — single-char
    # codes (T=OK, E/e=Error, f=Fatal, A=Cancelled, R=Running, ...).
    log_info "Waiting for backups to complete..."
    local deadline=$(( $(date +%s) + 1800 ))   # 30 min ceiling
    local pending=$JOBS done_count=0
    while (( pending > 0 )); do
        if (( $(date +%s) > deadline )); then
            log_error "Timed out waiting for backups; ${done_count}/${JOBS} done"
            return 1
        fi
        pending=0
        done_count=0
        for (( i=1; i<=JOBS; i++ )); do
            jobid="${JOB_BACKUP_ID[$i]}"
            out=$(bconsole_exec <<EOFSTAT
llist jobid=$jobid
quit
EOFSTAT
)
            local status
            status=$(echo "$out" | awk -F':[ \t]*' '/jobstatus:/{print $2; exit}' | tr -d ' \t')
            case "$status" in
                # T = terminated OK, W = terminated with warnings (bareos
                # considers W a success — the warnings come from things
                # like the "files mismatch" diagnostic, not data loss).
                T|W) done_count=$((done_count + 1)) ;;
                E|e|f|A|I)
                    log_error "Backup-$i (JobId=$jobid) ended with bareos jobstatus=$status:"
                    echo "$out" | grep -E 'jobstatus|joberrors|jobname' | sed 's/^/  /'
                    bconsole_exec <<EOFLOG | tail -40 | sed 's/^/  /'
list joblog jobid=$jobid
quit
EOFLOG
                    return 1
                    ;;
                *) pending=$((pending + 1)) ;;
            esac
        done
        if (( pending > 0 )); then
            sleep 5
            log_info "  $done_count/$JOBS done, $pending in progress..."
        fi
    done
    log_info "All $JOBS backups completed (jobstatus=T)"
}

# Restore every backup job, then diff the restored tree against the
# original. Bareos's `restore where=<dir>` recreates the source path
# under <dir>, so /backup-src/job-i ends up at <dir>/backup-src/job-i.
verify_restores() {
    log_info "Restoring + diffing every job (each restore is a full tape rewind/read)..."
    local i jobid out passed=0 failed=0
    for (( i=1; i<=JOBS; i++ )); do
        jobid="${JOB_BACKUP_ID[$i]}"
        local restore_root="/restore/job-$i"
        # `select all done` selects the entire backup tree by default,
        # `where=<dir>` rewrites the destination, `yes` confirms.
        out=$(bconsole_exec <<EOFRESTORE
restore client=bareos-fd jobid=$jobid where=$restore_root all done yes
quit
EOFRESTORE
)
        local restore_jobid
        restore_jobid=$(echo "$out" | awk '/Job queued\. JobId=/ { sub(/.*JobId=/, ""); print; exit }')
        if [[ -z "$restore_jobid" ]]; then
            log_fail "restore of Backup-$i: no JobId returned"
            echo "$out" | sed 's/^/  /'
            failed=$((failed + 1))
            continue
        fi
        # Wait for restore to terminate.
        local deadline=$(( $(date +%s) + 600 ))
        local status="R"
        while [[ "$status" == "R" || "$status" == "C" || -z "$status" ]]; do
            if (( $(date +%s) > deadline )); then
                log_fail "restore JobId=$restore_jobid timed out"
                break
            fi
            sleep 3
            out=$(bconsole_exec <<EOFRSTAT
llist jobid=$restore_jobid
quit
EOFRSTAT
)
            status=$(echo "$out" | awk -F':[ \t]*' '/jobstatus:/{print $2; exit}' | tr -d ' \t')
        done
        if [[ "$status" != "T" && "$status" != "W" ]]; then
            log_fail "restore of Backup-$i ended with jobstatus=$status"
            bconsole_exec <<EOFLOG | tail -30 | sed 's/^/  /'
list joblog jobid=$restore_jobid
quit
EOFLOG
            failed=$((failed + 1))
            continue
        fi
        # Diff. The container's /restore/job-i path corresponds to host
        # $TEST_DIR/restore/job-i; bareos restores with leading path so
        # the actual files live under restore/job-i/backup-src/job-i/.
        local src="$TEST_DIR/backup-src/job-$i"
        local dst="$TEST_DIR/restore/job-$i/backup-src/job-$i"
        if [[ ! -d "$dst" ]]; then
            log_fail "restore of Backup-$i: destination $dst not found"
            ls -laR "$TEST_DIR/restore/job-$i" 2>/dev/null | head -20 | sed 's/^/  /'
            failed=$((failed + 1))
            continue
        fi
        if ! diff -r "$src" "$dst" >"$TEST_DIR/diff-job-$i.txt" 2>&1; then
            log_fail "restore of Backup-$i diverged from source:"
            head -20 "$TEST_DIR/diff-job-$i.txt" | sed 's/^/  /'
            failed=$((failed + 1))
            continue
        fi
        JOB_RESTORE_OK[$i]=1
        passed=$((passed + 1))
        log_info "  Backup-$i restore OK ($(du -sb "$src" | awk '{print $1}') bytes verified)"
    done
    echo ""
    if (( failed == 0 )); then
        log_pass "All $passed restores byte-for-byte match"
        return 0
    else
        log_fail "$failed of $JOBS restores failed"
        return 1
    fi
}

main() {
    echo "========================================"
    echo "Thur VTL Bareos-Driven Backup Test"
    echo "========================================"
    echo ""

    check_prerequisites
    assign_ports
    create_test_config
    start_daemon
    create_cartridges
    connect_iscsi

    generate_bareos_passwords
    build_bareos_image
    write_bareos_entrypoint
    write_bareos_configs
    start_bareos_container

    # Seeded RNG drives both the per-job file counts/sizes and content.
    mc_seed_init "$SEED" "$TEST_DIR/ops.log"

    if ! label_cartridges; then
        log_fail "Cartridge labeling failed"
        exit 1
    fi
    generate_fixtures
    if ! run_backup_jobs; then
        log_fail "Backup-job phase failed"
        exit 1
    fi
    if ! verify_restores; then
        log_fail "Restore-verify phase failed"
        echo ""
        echo "Reproduce with: --seed $MC_SEED --jobs $JOBS"
        exit 1
    fi

    echo ""
    echo "========================================"
    log_pass "$JOBS bareos backups + restores  (seed=$MC_SEED)"
    echo "========================================"
    echo "  reusable reproducer: --seed $MC_SEED --jobs $JOBS"
    echo "  container logs:      podman logs $BAREOS_CONTAINER"
    echo "  daemon log:          $TEST_DIR/daemon.log"
    exit 0
}

main
