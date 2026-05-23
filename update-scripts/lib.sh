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
# These scripts are *upgrade* tools — a clean first-install of the
# .deb / .rpm goes through plain `dpkg -i` / `rpm -i`, which leaves
# the daemon stopped on purpose so the operator can edit the
# conffile before `systemctl enable --now`. The wrapper refuses to
# run on a host with no prior installation (see `require_installed`).
#
# Three mutually-exclusive end states:
#
#   default            unmount + logout + stop + install + start +
#                      login + remount.  Session-continuity for a
#                      routine upgrade.  State file removed on
#                      success.
#
#   --dont-restart     unmount + logout + stop + install.  Stops
#                      short of `systemctl start`, leaving the daemon
#                      down so the operator can review the conffile
#                      (matches the postinst's stance).  State file
#                      kept; paste-ready reattach commands printed.
#
#   --disconnect-only  unmount + logout only.  Daemon stays running;
#                      no package swap.  Use to quiesce the host
#                      before unrelated maintenance.  State file
#                      kept; paste-ready reattach commands printed.
#
# --dry-run and --force are orthogonal and compose with any of them.
# --dry-run prints every system-changing command instead of running
# it; read-only discovery still runs so the plan is accurate.
#
#   VSA: a LUN is logged in over iSCSI / NVMe-TCP and a filesystem is
#        mounted on it. The remount on the default path keys on
#        filesystem UUID, so a device rename across re-login doesn't
#        matter.
#
#   VTL: clients usually drive the tape device through backup
#        software (no mount). The exception is LTFS — `ltfs` mounts a
#        cartridge as a FUSE filesystem. LTFS mounts are discovered
#        automatically from the running `ltfs` processes, unmounted
#        before the stop (which flushes the tape index), and the same
#        `ltfs` command line is replayed afterwards. No LTFS mounts ->
#        the default run is just stop -> install -> start.

# --- options / state, set by parse_args ----------------------------
DRYRUN=0
force=0
DONT_RESTART=0
DISCONNECT_ONLY=0
PKGDIR=$PWD
TAG=update
# DONE is global (not function-local) so the EXIT trap can still read
# it after update_vsa / update_vtl has returned.
DONE=0

log() { printf '[%s] %s\n' "$TAG" "$*"; }
die() { printf '[%s] ERROR: %s\n' "$TAG" "$*" >&2; exit 1; }

usage() {
  cat >&2 <<EOF
usage: $(basename "${0:-update script}") [--dry-run] [--force]
              [--dont-restart | --disconnect-only] [package-dir]

  --dry-run          show the commands that would run; change nothing
  --force            VTL only: proceed even if backup sessions are connected
  --dont-restart     unmount + logout + stop + install, then exit;
                     leaves the daemon stopped so the operator can
                     review the conffile before starting it back up
  --disconnect-only  unmount + logout only; do not touch the daemon
                     or the package (no stop, no install, no start)
  package-dir        directory holding the package (default: current dir);
                     the newest matching package in it is installed
EOF
}

parse_args() {
  local a
  for a in "$@"; do
    case "$a" in
      --dry-run)         DRYRUN=1 ;;
      --force)           force=1 ;;
      --dont-restart)    DONT_RESTART=1 ;;
      --disconnect-only) DISCONNECT_ONLY=1 ;;
      -h|--help)         usage; exit 0 ;;
      --*)               usage; die "unknown option: $a" ;;
      *)                 PKGDIR=$a ;;
    esac
  done
  if [[ $DONT_RESTART -eq 1 && $DISCONNECT_ONLY -eq 1 ]]; then
    die "--dont-restart and --disconnect-only are mutually exclusive"
  fi
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

# require_installed SERVICE — refuse to run on a host where the
# daemon's systemd unit has never been on disk. This wrapper is the
# *upgrade* path; a clean first install goes through plain
# `dpkg -i` / `rpm -i`. Probed BEFORE the EXIT trap arms so die()
# exits cleanly without the trap's "INTERRUPTED" trailer.
# `systemctl cat <svc>` returns non-zero when the unit isn't on disk
# and is the canonical existence probe.
require_installed() {
  local svc=$1 hint
  if [[ $PKGFMT == deb ]]; then
    hint='sudo dpkg -i thurv{sa,tl}_*.deb'
  else
    hint='sudo rpm -i thurv{sa,tl}-*.rpm'
  fi
  if ! systemctl cat "$svc" >/dev/null 2>&1; then
    die "$svc is not installed on this host — first-install path is \`$hint\`; this script only handles in-place upgrades of an already-installed daemon."
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

# print_vsa_next_steps SERVICE STATE DAEMON_UP — paste-ready
# reattach instructions for --dont-restart / --disconnect-only on
# VSA. DAEMON_UP=1 means the daemon is still active (so step 1 is
# reattach); DAEMON_UP=0 means it was stopped and the package swapped
# (so the operator must edit the conffile + start it first).
print_vsa_next_steps() {
  local svc=$1 state=$2 up=$3
  local step=1 printed=0
  printf '\n[%s] next steps:\n' "$TAG"
  if [[ $up -eq 0 ]]; then
    printf '  %d. edit /etc/thurvsa/thurvsa.yaml if needed (the package has been swapped)\n' "$step"; step=$((step+1))
    printf '  %d. sudo systemctl start %s\n' "$step" "$svc"; step=$((step+1))
  fi
  printf '  %d. reattach sessions and remount (paste each line):\n' "$step"
  local _ iqn portal nqn traddr trsvcid mp uuid fstype opts cmd
  while IFS='|' read -r _ iqn portal; do
    [[ -n "$iqn" ]] || continue
    printf '       sudo iscsiadm -m node -T %s -p %s --login\n' "$iqn" "$portal"; printed=1
  done < <(grep '^ISCSI|' "$state" 2>/dev/null || true)
  while IFS='|' read -r _ nqn traddr trsvcid; do
    [[ -n "$nqn" ]] || continue
    printf '       sudo nvme connect -t tcp -a %s -s %s -n %s\n' "$traddr" "$trsvcid" "$nqn"; printed=1
  done < <(grep '^NVME|' "$state" 2>/dev/null || true)
  while IFS='|' read -r _ mp uuid fstype opts; do
    [[ -n "$mp" ]] || continue
    cmd='sudo mount'
    [[ -n "$fstype" ]] && cmd="$cmd -t $fstype"
    [[ -n "$opts"   ]] && cmd="$cmd -o $opts"
    if [[ -n "$uuid" ]]; then
      printf '       %s UUID=%s %s\n' "$cmd" "$uuid" "$mp"
    else
      printf '       # no UUID captured for %s — mount by hand\n' "$mp"
    fi
    printed=1
  done < <(grep '^MOUNT|' "$state" 2>/dev/null || true)
  [[ $printed -eq 0 ]] && printf '       (nothing to reattach — discovery found no thurvsa-backed sessions or mounts)\n'
  printf '  state file kept at %s\n\n' "$state"
}

# print_vtl_next_steps SERVICE STATED DAEMON_UP — paste-ready
# reattach + LTFS-replay instructions for --dont-restart /
# --disconnect-only on VTL. Reads $STATED/iscsi.list (one
# <iqn>|<portal> per line) and each $STATED/ltfs.<n>.argv (null-
# separated argv of the original `ltfs` invocation).
print_vtl_next_steps() {
  local svc=$1 stated=$2 up=$3
  local step=1 printed=0
  printf '\n[%s] next steps:\n' "$TAG"
  if [[ $up -eq 0 ]]; then
    printf '  %d. edit /etc/thurvtl/thurvtl.yaml if needed (the package has been swapped)\n' "$step"; step=$((step+1))
    printf '  %d. sudo systemctl start %s\n' "$step" "$svc"; step=$((step+1))
  fi
  printf '  %d. reattach sessions and replay LTFS mounts (paste each line):\n' "$step"
  local iqn portal f
  if [[ -f "$stated/iscsi.list" ]]; then
    while IFS='|' read -r iqn portal; do
      [[ -n "$iqn" ]] || continue
      printf '       sudo iscsiadm -m node -T %s -p %s --login\n' "$iqn" "$portal"; printed=1
    done < "$stated/iscsi.list"
  fi
  for f in "$stated"/ltfs.*.argv; do
    [[ -f "$f" ]] || continue
    local -a argv
    mapfile -d '' argv < "$f"
    [[ ${#argv[@]} -gt 0 && -z "${argv[-1]}" ]] && unset 'argv[-1]'
    [[ ${#argv[@]} -gt 0 ]] || continue
    printf '       sudo '
    printf '%q ' "${argv[@]}"
    printf '\n'
    printed=1
  done
  [[ $printed -eq 0 ]] && printf '       (nothing to reattach — discovery found no LTFS mounts or sessions)\n'
  printf '  state dir kept at %s\n\n' "$stated"
}

# =====================================================================
# VSA — block LUNs over iSCSI / NVMe-TCP
# =====================================================================
update_vsa() {
  TAG=vsa-update
  local SERVICE=thurvsad IQN_MATCH=':thurvsa' NQN_MATCH='thurvsa'
  local STATE=/var/tmp/vsa-update.state DEV_WAIT=90

  need_root
  local pkg; pkg=$(pick_pkg thurvsa)
  log "package: $pkg"
  require_installed "$SERVICE"      # upgrade-only gate (bails clean)

  DONE=0
  trap '[[ $DONE -eq 1 || $DRYRUN -eq 1 ]] || printf "[vsa-update] INTERRUPTED — teardown state is in %s\n" "'"$STATE"'" >&2' EXIT

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

  # ---- mode: --disconnect-only --------------------------------------
  if [[ $DISCONNECT_ONLY -eq 1 ]]; then
    DONE=1
    log "disconnect-only: stopped after unmount / logout / disconnect — daemon left running"
    print_vsa_next_steps "$SERVICE" "$STATE" 1
    return 0
  fi

  # ---- swap the package ---------------------------------------------
  log "stopping $SERVICE"; run systemctl stop "$SERVICE"
  install_pkg "$pkg"

  # ---- mode: --dont-restart -----------------------------------------
  if [[ $DONT_RESTART -eq 1 ]]; then
    DONE=1
    log "dont-restart: package swapped — daemon left stopped"
    print_vsa_next_steps "$SERVICE" "$STATE" 0
    return 0
  fi

  # ---- default path: restart, reattach, remount ---------------------
  log "starting $SERVICE"; run systemctl start "$SERVICE"
  wait_active "$SERVICE"
  [[ $DRYRUN -eq 1 ]] || sleep 2

  while IFS='|' read -r _ iqn portal; do
    log "iSCSI login $iqn"
    run iscsiadm -m node -T "$iqn" -p "$portal" --login || true
  done < <(grep '^ISCSI|' "$STATE" || true)
  while IFS='|' read -r _ nqn traddr trsvcid; do
    log "NVMe connect $nqn"
    run nvme connect -t tcp -a "$traddr" -s "$trsvcid" -n "$nqn" || true
  done < <(grep '^NVME|' "$STATE" || true)
  [[ $DRYRUN -eq 1 ]] || { command -v udevadm >/dev/null 2>&1 && udevadm settle || true; }

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
  # `${DRYRUN:+...}` would always fire here — DRYRUN is set to "0" or
  # "1", both non-empty, so the `:+` triggers regardless. Test the
  # actual value instead.
  local dry_tag=''
  [[ $DRYRUN -eq 1 ]] && dry_tag='(dry-run) '
  log "done — $SERVICE upgrade ${dry_tag}complete"
}

# =====================================================================
# VTL — tape library; LTFS-aware
# =====================================================================
update_vtl() {
  TAG=vtl-update
  local SERVICE=thurvtld IQN_MATCH=':thurvtl'
  local STATED=/var/tmp/vtl-update.state.d DEV_WAIT=90

  need_root
  local pkg; pkg=$(pick_pkg thurvtl)
  log "package: $pkg"
  require_installed "$SERVICE"      # upgrade-only gate (bails clean)

  DONE=0
  trap '[[ $DONE -eq 1 || $DRYRUN -eq 1 ]] || printf "[vtl-update] INTERRUPTED — teardown state is in %s\n" "'"$STATED"'" >&2' EXIT

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
  # Sessions go into both a bash array (for default-mode reattach)
  # and $STATED/iscsi.list (so print_vtl_next_steps can read state
  # uniformly from disk).
  local -a ISCSI=()
  local _ sid portal iqn
  : > "$STATED/iscsi.list"
  if command -v iscsiadm >/dev/null 2>&1; then
    while read -r _ sid portal iqn _; do
      [[ "$iqn" == *"$IQN_MATCH"* ]] || continue
      ISCSI+=("$iqn|${portal%,*}")
      echo "$iqn|${portal%,*}" >> "$STATED/iscsi.list"
      log "found iSCSI session ${sid//[\[\]]/} -> $iqn @ ${portal%,*}"
    done < <(iscsiadm -m session 2>/dev/null || true)
  fi

  # ---- gate: sessions but no LTFS mount => maybe a live backup ------
  if [[ ${#LTFS_MP[@]} -eq 0 && ${#ISCSI[@]} -gt 0 && $force -eq 0 ]]; then
    die "iSCSI session(s) to the thurvtl target are connected and no LTFS mount
       was found — backup software may be mid-job. Quiesce it and retry, or
       pass --force to proceed anyway."
  fi
  [[ ${#LTFS_MP[@]} -eq 0 && ${#ISCSI[@]} -eq 0 ]] && log "no LTFS mounts or sessions on this host"

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

  # ---- mode: --disconnect-only --------------------------------------
  if [[ $DISCONNECT_ONLY -eq 1 ]]; then
    DONE=1
    log "disconnect-only: stopped after LTFS unmount / iSCSI logout — daemon left running"
    print_vtl_next_steps "$SERVICE" "$STATED" 1
    return 0
  fi

  # ---- swap the package ---------------------------------------------
  log "stopping $SERVICE"; run systemctl stop "$SERVICE"
  install_pkg "$pkg"

  # ---- mode: --dont-restart -----------------------------------------
  if [[ $DONT_RESTART -eq 1 ]]; then
    DONE=1
    log "dont-restart: package swapped — daemon left stopped"
    print_vtl_next_steps "$SERVICE" "$STATED" 0
    return 0
  fi

  # ---- default path: restart, reattach, replay LTFS ----------------
  log "starting $SERVICE"; run systemctl start "$SERVICE"
  wait_active "$SERVICE"
  [[ $DRYRUN -eq 1 ]] || sleep 2

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
  # `${DRYRUN:+...}` would always fire here — DRYRUN is set to "0" or
  # "1", both non-empty, so the `:+` triggers regardless. Test the
  # actual value instead.
  local dry_tag=''
  [[ $DRYRUN -eq 1 ]] && dry_tag='(dry-run) '
  log "done — $SERVICE upgrade ${dry_tag}complete"
}
