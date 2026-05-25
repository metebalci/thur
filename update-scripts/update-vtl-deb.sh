#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vtl-deb.sh — upgrade thurvtld from a .deb package.
#
#   sudo ./update-vtl-deb.sh [--dry-run] [--force]
#               [--dont-restart | --disconnect-only]
#               [--use-repo | package-dir]
#
# Upgrade-only wrapper — refuses if thurvtld has never been
# installed; first install goes through `sudo dpkg -i thurvtl_*.deb`.
# Default behavior: discover any LTFS mounts (from the running
# `ltfs` processes) and loopback iSCSI sessions, unmount LTFS, log
# the sessions out, stop thurvtld, install the newest thurvtl_*.deb
# in package-dir (default: the current directory), then (only if
# the daemon was active before the run) start thurvtld, log back
# in, and replay each `ltfs` mount. With no LTFS mounts it
# is just stop/install/start. --dont-restart stops after the
# package swap so the operator can review the conffile before
# starting the daemon back up. --disconnect-only quiesces the host
# (LTFS unmount + iSCSI logout) without touching the daemon or the
# package. --use-repo installs from the configured apt repository
# (apt-get update; apt-get install --only-upgrade thurvtl) instead
# of a local file — mutually exclusive with package-dir. --force
# proceeds even if backup-software sessions are connected;
# --dry-run shows the plan without changing anything. Full sequence
# + caveats are in lib.sh.
#
set -euo pipefail
PKGFMT=deb
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vtl
