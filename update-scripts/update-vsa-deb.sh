#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vsa-deb.sh — upgrade thurvsad from a .deb package.
#
#   sudo ./update-vsa-deb.sh [--dry-run]
#               [--dont-restart | --disconnect-only]
#               [--use-repo | package-dir]
#
# Upgrade-only wrapper — refuses if thurvsad has never been
# installed; first install goes through `sudo dpkg -i thurvsa_*.deb`.
# Default behavior: unmount the loopback iSCSI / NVMe-TCP volumes,
# log the sessions out, stop thurvsad, install the newest
# thurvsa_*.deb in package-dir (default: the current directory),
# then (only if the daemon was active before the run) start
# thurvsad, log back in, and remount (by filesystem UUID).
# --dont-restart stops after the package swap so the operator can
# review the conffile before starting the daemon back up.
# --disconnect-only quiesces the host (unmount + logout) without
# touching the daemon or the package. --use-repo installs from the
# configured apt repository (apt-get update; apt-get install
# --only-upgrade thurvsa) instead of a local file — mutually
# exclusive with package-dir. --dry-run shows the plan without
# changing anything. Full sequence + caveats are in lib.sh.
#
set -euo pipefail
PKGFMT=deb
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vsa
