# shellcheck shell=bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# lib.sh — shared logic for the thur update-scripts.
#
# Sourced by update-{vsa,vtl}-{deb,rpm}.sh, which set $PKGFMT to
# "deb" or "rpm" first and then call update_vsa / update_vtl. Not
# meant to be run on its own.
#
# Model
# -----
# thurvsad and thurvtld are iSCSI / NVMe-TCP *targets* — they export
# block LUNs (VSA) and tape + changer devices (VTL); they do not
# mount anything themselves. These scripts handle the LOOPBACK case:
# the initiator that consumes the exported devices is THIS host.
# Initiators on remote hosts are not touched — stopping the daemon
# still drops their sessions, so quiesce those hosts yourself.
#
#   VSA: a LUN is logged in over iSCSI / NVMe-TCP and a filesystem is
#        mounted on it. Sequence: unmount -> log sessions out -> stop
#        -> install -> start -> log back in -> remount by filesystem
#        UUID (a device rename across re-login then doesn't matter).
#
#   VTL: clients usually drive the tape device through backup
#        software (no mount). The exception is LTFS — `ltfs` mounts a
#        cartridge as a FUSE filesystem. LTFS mounts are discovered
#        automatically from the running `ltfs` processes, unmounted
#        before the stop (which flushes the tape index), and the same
#        `ltfs` command line is replayed afterwards. No LTFS mounts ->
#        the run is just stop -> install -> start.
#
# --dry-run prints every system-changing command instead of running
# it; read-only discovery still runs so the plan is accurate.

# --- options / state, set by parse_args ----------------------------
DRYRUN=0
force=0
PKGDIR=$PWD
TAG=update
# DONE is global (not function-local) so the EXIT trap can still read
# it after update_vsa / update_vtl has returned.
DONE=0

log() { printf '[%s] %s\n' "$TAG" "$*"; }
die() { printf '[%s] ERROR: %s\n' "$TAG" "$*" >&2; exit 1; }

usage() {
  cat >&2 <<EOF
usage: $(basename "${0:-update script}") [--dry-run] [--force] [package-dir]

  --dry-run      show the commands that would run; change nothing
  --force        VTL only: proceed even if backup sessions are connected
  package-dir    directory holding the package (default: current dir);
                 the newest matching package in it is installed
EOF
}

parse_args() {
  local a
  for a in "$@"; do
    case "$a" in
      --dry-run) DRYRUN=1 ;;
      --force)   force=1 ;;
      -h|--help) usage; exit 0 ;;
      --*)       usage; die "unknown option: $a" ;;
      *)         PKGDIR=$a ;;
    esac
  done
  PKGDIR=$(cd "$PKGDIR" 2>/dev/null && pwd) || die "package dir not found: $PKGDIR"
  [[ $DRYRUN -eq 1 ]] && log "DRY-RUN: nothing will be changed; planned commands shown as '[would run] ...'"
  return 0
}

# run CMD... — execute, or in dry-run just print it.
run() {
  if [[ $DRYRUN -eq 1 ]]; then
    printf '  [would run] %s\n' "$*"
    return 0
  fi
  "$@"
}

need_root() {
  if [[ $EUID -ne 0 ]]; then
    [[ $DRYRUN -eq 1 ]] && { log "WARNING: not root — dry-run discovery may be incomplete"; return 0; }
    die "must run as root"
  fi
}

# pick_pkg NAME — echo the newest matching package file in $PKGDIR.
pick_pkg() {
  local name=$1 pat f
  case "$PKGFMT" in
    deb) pat="${name}_*.deb" ;;   # thurvsa_0.1.0-alpha.3_amd64.deb
    rpm) pat="${name}-*.rpm" ;;   # thurvsa-0.1.0-1.x86_64.rpm
    *)   die "PKGFMT must be 'deb' or 'rpm' (got '${PKGFMT:-unset}')" ;;
  esac
  f=$(find "$PKGDIR" -maxdepth 1 -type f -name "$pat" -printf '%T@ %p\n' 2>/dev/null \
        | sort -rn | head -1 | cut -d' ' -f2-)
  [[ -n "$f" ]] || die "no $pat found in $PKGDIR"
  printf '%s\n' "$f"
}

# install_pkg FILE — format-appropriate, dependency-resolving install.
install_pkg() {
  local f=$1
  log "installing $(basename "$f")"
  case "$PKGFMT" in
    deb)
      if [[ $DRYRUN -eq 1 ]]; then
        printf '  [would run] dpkg -i %s   (apt-get -f install -y if deps are missing)\n' "$f"
        return 0
      fi
      dpkg -i "$f" || { log "dpkg flagged dependencies — running apt-get -f install"; apt-get -f install -y; }
      ;;
    rpm)
      local -a cmd
      if   command -v dnf    >/dev/null 2>&1; then cmd=(dnf -y install "$f")
      elif command -v yum    >/dev/null 2>&1; then cmd=(yum -y install "$f")
      elif command -v zypper >/dev/null 2>&1; then cmd=(zypper --non-interactive install --allow-unsigned-rpm "$f")
      else
        log "no dnf/yum/zypper — falling back to rpm -U (no dependency resolution)"
        cmd=(rpm -U --replacepkgs "$f")
      fi
      run "${cmd[@]}"
      ;;
  esac
}

# stop_if_loaded SERVICE — `systemctl stop` only when systemd has
# the unit loaded. On a fresh host where the .deb / .rpm has never
# been installed, the unit file does not exist and `systemctl stop`
# would fail the `set -e` check and trigger the EXIT trap's
# "INTERRUPTED" notice for what is really a clean first-install run.
# `systemctl cat <svc>` returns non-zero when the unit isn't on disk
# and is the canonical existence probe.
stop_if_loaded() {
  local svc=$1
  if systemctl cat "$svc" >/dev/null 2>&1; then
    log "stopping $svc"
    run systemctl stop "$svc"
  else
    log "$svc not installed yet — skipping stop (first-install run)"
  fi
}

# wait_active SERVICE — block until systemd reports it active.
wait_active() {
  local svc=$1 i s
  if [[ $DRYRUN -eq 1 ]]; then
    log "[would wait] for $svc to report active"
    return 0
  fi
  for ((i=0; i<60; i++)); do
    s=$(systemctl is-active "$svc" || true)
    [[ "$s" == active ]] && { log "$svc is active"; return 0; }
    [[ "$s" == failed ]] && { systemctl --no-pager status "$svc" >&2; die "$svc failed to start"; }
    sleep 1
  done
  die "timed out waiting for $svc to become active"
}

# remount_fs_line "MOUNT|mp|uuid|fstype|opts" — VSA block-fs remount,
# keyed on filesystem UUID so a device rename doesn't matter.
remount_fs_line() {
  local _ mp uuid fstype opts
  IFS='|' read -r _ mp uuid fstype opts <<<"$1"
  if [[ -z "$uuid" ]]; then
    log "  SKIP remount of $mp — no filesystem UUID captured; mount it by hand"
    return 1
  fi
  run mount -o "${opts:-defaults}" ${fstype:+-t "$fstype"} "UUID=$uuid" "$mp" \
    || run mount "UUID=$uuid" "$mp"
}

# =====================================================================
# VSA — block LUNs over iSCSI / NVMe-TCP
# =====================================================================
update_vsa() {
  TAG=vsa-update
  local SERVICE=thurvsad IQN_MATCH=':thurvsa' NQN_MATCH='thurvsa'
  local STATE=/var/tmp/vsa-update.state DEV_WAIT=90
  DONE=0
  trap '[[ $DONE -eq 1 || $DRYRUN -eq 1 ]] || printf "[vsa-update] INTERRUPTED — teardown state is in %s\n" "'"$STATE"'" >&2' EXIT

  need_root
  local pkg; pkg=$(pick_pkg thurvsa)
  log "package: $pkg"

  # ---- discover (read-only) -----------------------------------------
  : > "$STATE"
  local -a DEVICES=()
  local _ sid portal iqn d c nqn addr traddr trsvcid ns

  if command -v iscsiadm >/dev/null 2>&1; then
    while read -r _ sid portal iqn _; do
      [[ "$iqn" == *"$IQN_MATCH"* ]] || continue
      sid=${sid//[\[\]]/}; portal=${portal%,*}
      echo "ISCSI|$iqn|$portal" >> "$STATE"
      log "found iSCSI session $sid -> $iqn @ $portal"
      while read -r d; do DEVICES+=("/dev/$d"); done < <(
        iscsiadm -m session -r "$sid" -P3 2>/dev/null | awk '/Attached scsi disk/ {print $4}')
    done < <(iscsiadm -m session 2>/dev/null || true)
  fi

  if command -v nvme >/dev/null 2>&1; then
    for c in /sys/class/nvme/nvme*; do
      [[ -r "$c/transport" ]] && [[ "$(cat "$c/transport")" == tcp ]] || continue
      nqn=$(cat "$c/subsysnqn" 2>/dev/null || true)
      [[ "$nqn" == *"$NQN_MATCH"* ]] || continue
      addr=$(cat "$c/address" 2>/dev/null || true)
      traddr=$(sed -n 's/.*traddr=\([^, ]*\).*/\1/p' <<<"$addr")
      trsvcid=$(sed -n 's/.*trsvcid=\([^, ]*\).*/\1/p' <<<"$addr")
      echo "NVME|$nqn|$traddr|$trsvcid" >> "$STATE"
      log "found NVMe-TCP $(basename "$c") -> $nqn @ $traddr:$trsvcid"
      for ns in "/dev/$(basename "$c")"n*; do [[ -b "$ns" ]] && DEVICES+=("$ns"); done
    done
  fi

  local tgt src fstype opts dev uuid
  if [[ ${#DEVICES[@]} -eq 0 ]]; then
    log "no thurvsa-backed block devices on this host"
  else
    while read -r tgt src fstype opts; do
      for dev in "${DEVICES[@]}"; do
        if [[ "$src" == "$dev" || "$src" == "$dev"[0-9]* || "$src" == "${dev}p"[0-9]* ]]; then
          uuid=$(lsblk -no UUID "$src" 2>/dev/null | head -1)
          echo "MOUNT|$tgt|$uuid|$fstype|$opts" >> "$STATE"
          log "found mount $tgt  (UUID=${uuid:-none} $fstype)"
          break
        fi
      done
    done < <(findmnt -rno TARGET,SOURCE,FSTYPE,OPTIONS)
  fi

  # ---- unmount (roll back on partial failure) -----------------------
  local -a OKLINES=()
  local line mp ok
  while read -r line; do
    IFS='|' read -r _ mp _ _ _ <<<"$line"
    log "unmounting $mp"
    if run umount "$mp"; then
      OKLINES+=("$line")
    else
      log "unmount of $mp failed — rolling back, daemon left running"
      for ok in "${OKLINES[@]}"; do remount_fs_line "$ok" || true; done
      die "could not unmount $mp — quiesce it (fuser -m $mp) and retry"
    fi
  done < <(grep '^MOUNT|' "$STATE" || true)

  # ---- detach sessions ----------------------------------------------
  while IFS='|' read -r _ iqn portal; do
    log "iSCSI logout $iqn"
    run iscsiadm -m node -T "$iqn" -p "$portal" --logout || true
  done < <(grep '^ISCSI|' "$STATE" || true)
  while IFS='|' read -r _ nqn _ _; do
    log "NVMe disconnect $nqn"
    run nvme disconnect -n "$nqn" || true
  done < <(grep '^NVME|' "$STATE" || true)

  # ---- swap the package ---------------------------------------------
  stop_if_loaded "$SERVICE"
  install_pkg "$pkg"
  log "starting $SERVICE"; run systemctl start "$SERVICE"
  wait_active "$SERVICE"
  [[ $DRYRUN -eq 1 ]] || sleep 2

  # ---- reattach sessions --------------------------------------------
  while IFS='|' read -r _ iqn portal; do
    log "iSCSI login $iqn"
    run iscsiadm -m node -T "$iqn" -p "$portal" --login || true
  done < <(grep '^ISCSI|' "$STATE" || true)
  while IFS='|' read -r _ nqn traddr trsvcid; do
    log "NVMe connect $nqn"
    run nvme connect -t tcp -a "$traddr" -s "$trsvcid" -n "$nqn" || true
  done < <(grep '^NVME|' "$STATE" || true)
  [[ $DRYRUN -eq 1 ]] || { command -v udevadm >/dev/null 2>&1 && udevadm settle || true; }

  # ---- remount ------------------------------------------------------
  local rc=0 j
  while read -r line; do
    IFS='|' read -r _ mp uuid _ _ <<<"$line"
    if [[ $DRYRUN -eq 0 && -n "$uuid" ]]; then
      log "waiting for UUID=$uuid"
      for ((j=0; j<DEV_WAIT; j++)); do blkid -U "$uuid" >/dev/null 2>&1 && break; sleep 1; done
    fi
    log "remounting $mp"
    remount_fs_line "$line" || rc=1
  done < <(grep '^MOUNT|' "$STATE" || true)

  DONE=1
  [[ $rc -eq 0 ]] || die "some volumes did not remount — see $STATE and mount them by hand"
  rm -f "$STATE"
  log "done — $SERVICE upgrade ${DRYRUN:+(dry-run) }complete"
}

# =====================================================================
# VTL — tape library; LTFS-aware
# =====================================================================
update_vtl() {
  TAG=vtl-update
  local SERVICE=thurvtld IQN_MATCH=':thurvtl'
  local STATED=/var/tmp/vtl-update.state.d DEV_WAIT=90
  DONE=0
  trap '[[ $DONE -eq 1 || $DRYRUN -eq 1 ]] || printf "[vtl-update] INTERRUPTED — teardown state is in %s\n" "'"$STATED"'" >&2' EXIT

  need_root
  local pkg; pkg=$(pick_pkg thurvtl)
  log "package: $pkg"
  rm -rf "$STATED"; mkdir -p "$STATED"

  # ---- discover LTFS mounts from the running ltfs processes ---------
  local -a LTFS_MP=() LTFS_ARGV=()
  local pid mp n=0
  if command -v pgrep >/dev/null 2>&1; then
    while read -r pid; do
      [[ -r /proc/$pid/cmdline ]] || continue
      mp=$(tr '\0' '\n' < "/proc/$pid/cmdline" | grep -v '^$' | tail -1)
      [[ -n "$mp" ]] || continue
      findmnt -rno FSTYPE --target "$mp" 2>/dev/null | grep -qi ltfs || continue
      cp "/proc/$pid/cmdline" "$STATED/ltfs.$n.argv"
      LTFS_MP+=("$mp"); LTFS_ARGV+=("$STATED/ltfs.$n.argv")
      log "found LTFS mount $mp (ltfs pid $pid)"
      n=$((n + 1))
    done < <(pgrep -x ltfs || true)
  fi

  # ---- discover iSCSI sessions to our target ------------------------
  local -a ISCSI=()
  local _ sid portal iqn
  if command -v iscsiadm >/dev/null 2>&1; then
    while read -r _ sid portal iqn _; do
      [[ "$iqn" == *"$IQN_MATCH"* ]] || continue
      ISCSI+=("$iqn|${portal%,*}")
      log "found iSCSI session ${sid//[\[\]]/} -> $iqn @ ${portal%,*}"
    done < <(iscsiadm -m session 2>/dev/null || true)
  fi

  # ---- gate: sessions but no LTFS mount => maybe a live backup ------
  if [[ ${#LTFS_MP[@]} -eq 0 && ${#ISCSI[@]} -gt 0 && $force -eq 0 ]]; then
    die "iSCSI session(s) to the thurvtl target are connected and no LTFS mount
       was found — backup software may be mid-job. Quiesce it and retry, or
       pass --force to proceed anyway."
  fi
  [[ ${#LTFS_MP[@]} -eq 0 && ${#ISCSI[@]} -eq 0 ]] && log "no LTFS mounts or sessions — plain stop/install/start"

  # ---- unmount LTFS (flushes the index to the virtual tape) ---------
  local i
  for i in "${!LTFS_MP[@]}"; do
    log "unmounting LTFS ${LTFS_MP[$i]}"
    run umount "${LTFS_MP[$i]}" \
      || die "could not unmount LTFS ${LTFS_MP[$i]} — quiesce it and retry (daemon still up)"
  done

  # ---- detach iSCSI sessions ----------------------------------------
  for i in "${ISCSI[@]}"; do
    IFS='|' read -r iqn portal <<<"$i"
    log "iSCSI logout $iqn"
    run iscsiadm -m node -T "$iqn" -p "$portal" --logout || true
  done

  # ---- swap the package ---------------------------------------------
  stop_if_loaded "$SERVICE"
  install_pkg "$pkg"
  log "starting $SERVICE"; run systemctl start "$SERVICE"
  wait_active "$SERVICE"
  [[ $DRYRUN -eq 1 ]] || sleep 2

  # ---- reattach iSCSI sessions --------------------------------------
  for i in "${ISCSI[@]}"; do
    IFS='|' read -r iqn portal <<<"$i"
    log "iSCSI login $iqn"
    run iscsiadm -m node -T "$iqn" -p "$portal" --login || true
  done
  [[ $DRYRUN -eq 1 ]] || { command -v udevadm >/dev/null 2>&1 && udevadm settle || true; }

  # ---- remount LTFS by replaying each captured `ltfs` command -------
  local rc=0 dev j
  local -a argv
  for i in "${!LTFS_MP[@]}"; do
    mapfile -d '' argv < "${LTFS_ARGV[$i]}"
    [[ ${#argv[@]} -gt 0 && -z "${argv[-1]}" ]] && unset 'argv[-1]'
    dev=$(printf '%s\n' "${argv[@]}" | sed -n 's/.*devname=\([^,]*\).*/\1/p' | head -1)
    if [[ $DRYRUN -eq 0 && -n "$dev" ]]; then
      log "waiting for tape device $dev"
      for ((j=0; j<DEV_WAIT; j++)); do [[ -e "$dev" ]] && break; sleep 1; done
    fi
    log "remounting LTFS ${LTFS_MP[$i]}"
    run "${argv[@]}" || { log "  LTFS remount failed — run by hand: ${argv[*]}"; rc=1; }
  done

  DONE=1
  [[ $rc -eq 0 ]] || die "some LTFS mounts did not come back — see messages above"
  rm -rf "$STATED"
  log "done — $SERVICE upgrade ${DRYRUN:+(dry-run) }complete"
}
