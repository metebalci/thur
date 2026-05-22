#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# scripts/setup-system.sh — one-shot operator-friendly system prep for
# Thur VTL + Thur VSA. Mirrors what the .deb / .rpm postinst scripts
# do, minus the conffile drop (operator manages that themselves).
#
# What it does, per product (thurvtl + thurvsa):
#   1. Create system group + system user if missing (no shell, home =
#      data dir, system uid range).
#   2. Create /etc/<product>/ dir with root:<product> 0750 if missing.
#   3. Fix ownership on /etc/<product>/<product>.yaml and
#      /etc/<product>/<product>.env if either exists (root:<product>
#      0640 — daemon's group can read). NEVER touches the file
#      CONTENTS: this script only chown / chmod's existing config;
#      operator-authored YAML / env content is preserved verbatim.
#   4. Create /var/lib/<product>/ dir with <product>:<product> 0750
#      and recursively chown.
#   5. Create /run/<product>/ dir with <product>:<product> 0750
#      (matches RuntimeDirectoryMode= in the .service unit). Note:
#      /run is tmpfs, so this evaporates at reboot. Under systemd
#      `RuntimeDirectory=<product>` recreates it at unit start; for
#      direct (non-systemd) launches re-run this script after reboot
#      or override the admin socket path via THUR{VTL,VSA}_ADMIN_SOCKET.
#
# What it does NOT do:
#   - Drop a starter conffile or <product>.env (operator's
#      responsibility, and existing files are preserved byte-for-byte).
#   - Install/enable the systemd unit (run `systemctl enable --now
#      <product>d` after dropping a conffile).
#   - Edit /etc/sudoers (separate concern).
#
# Idempotent: safe to re-run on every checkout / after a code change
# / after a rebrand. Skips what's already correct, fixes drift on the
# rest. Output is a step-by-step log so you can see what changed.
#
# Identity values mirror shared_naming::TAPE_LIBRARY and
# shared_naming::DISK at shared/naming/src/lib.rs — keep in sync if
# those constants ever change.
#
# Usage:
#   ./scripts/setup-system.sh           # self-elevates via sudo
#   sudo ./scripts/setup-system.sh      # already-root path

set -e

# Self-elevate. The script needs root for groupadd/useradd/chown; the
# `exec sudo` trampoline keeps a single invocation in your shell
# history (no double-pasted sudo prefix) and inherits the operator's
# environment.
if [ "$(id -u)" -ne 0 ]; then
    exec sudo "$0" "$@"
fi

# Colors for the human-facing log. NO_COLOR= or non-tty disables.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    G='\033[0;32m'  # info
    Y='\033[1;33m'  # action
    N='\033[0m'
else
    G=''
    Y=''
    N=''
fi

log()  { printf '%b[INFO]%b %s\n' "$G" "$N" "$*"; }
do_()  { printf '%b[ACT ]%b %s\n' "$Y" "$N" "$*"; "$@"; }

# Per-product setup. Two args:
#   $1 = short product name (used as system user, group, dir basename)
#   $2 = human-readable comment for the GECOS field
#
# Hardcoded paths follow shared_naming::PRODUCT.{config_path,data_dir}
# convention: /etc/<name>/<name>.yaml and /var/lib/<name>/.
setup_product() {
    local name="$1"
    local desc="$2"

    log "==> Setting up $name ($desc)"

    # 1. Group
    if getent group "$name" >/dev/null; then
        log "    group  $name already exists (gid=$(getent group "$name" | cut -d: -f3))"
    else
        do_ groupadd --system "$name"
    fi

    # 2. User
    if getent passwd "$name" >/dev/null; then
        log "    user   $name already exists (uid=$(id -u "$name"))"
    else
        do_ useradd --system \
                    --gid "$name" \
                    --home-dir "/var/lib/$name" \
                    --shell /usr/sbin/nologin \
                    --comment "$desc" \
                    "$name"
    fi

    # 3. /etc/<name>/ — conffile dir
    if [ ! -d "/etc/$name" ]; then
        do_ mkdir -p "/etc/$name"
    fi
    # Always reconcile ownership + perms (postinst pattern: idempotent
    # repair of operator-fiddled state).
    do_ chown root:"$name" "/etc/$name"
    do_ chmod 0750         "/etc/$name"

    # 3b. Conffile (if present) — daemon reads it, so its group needs
    # read. Leave content alone.
    local conffile="/etc/$name/$name.yaml"
    if [ -f "$conffile" ]; then
        do_ chown root:"$name" "$conffile"
        do_ chmod 0640         "$conffile"
    else
        log "    conffile $conffile not present (operator drops it later)"
    fi

    # 3c. Optional daemon env file (cloud creds, ${ENV_VAR} secrets,
    # feature flags). Same pattern as the conffile.
    local envfile="/etc/$name/$name.env"
    if [ -f "$envfile" ]; then
        do_ chown root:"$name" "$envfile"
        do_ chmod 0640         "$envfile"
    fi

    # 4. /var/lib/<name>/ — data dir
    if [ ! -d "/var/lib/$name" ]; then
        do_ mkdir -p "/var/lib/$name"
    fi
    # Recursive chown is intentional — cartridges / volumes / chunk
    # pools / audit logs all live under here and must be daemon-
    # writable. Cheap on a fresh dir, idempotent on a populated one.
    do_ chown -R "$name":"$name" "/var/lib/$name"
    do_ chmod    0750            "/var/lib/$name"

    # 5. /run/<name>/ — runtime dir for the admin Unix socket. /run is
    # tmpfs, so this dies at reboot; under systemd RuntimeDirectory=
    # recreates it at unit start, but a direct (non-systemd) launch
    # needs it pre-created or the socket bind fails with EACCES.
    # Mode 0750 matches RuntimeDirectoryMode= in the .service unit.
    if [ ! -d "/run/$name" ]; then
        do_ mkdir -p "/run/$name"
    fi
    do_ chown "$name":"$name" "/run/$name"
    do_ chmod 0750            "/run/$name"

    log "    $name OK"
}

setup_product thurvtl "Thur VTL daemon"
setup_product thurvsa "Thur VSA daemon"

log "All set. Next steps:"
log "  - Drop a conffile at /etc/thurvtl/thurvtl.yaml (start from"
log "    release/thurvtl.yaml or 'thurvtl config defaults')."
log "  - Drop a conffile at /etc/thurvsa/thurvsa.yaml (start from"
log "    release/thurvsa.yaml or 'thurvsa config defaults')."
log "  - thurvtl needs 'thurvtl library init --slots N --drives M"
log "    --lto-generation G' before the daemon will start."
log "  - systemctl enable --now thurvtld thurvsad (once"
log "    you've installed the .service units), or run the binaries"
log "    directly under sudo -u <product>."
