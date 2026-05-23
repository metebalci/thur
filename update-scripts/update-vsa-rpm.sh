#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vsa-rpm.sh — upgrade thurvsad from an .rpm package.
#
#   sudo ./update-vsa-rpm.sh [--dry-run]
#               [--dont-restart | --disconnect-only] [package-dir]
#
# Upgrade-only wrapper — refuses if thurvsad has never been
# installed; first install goes through `sudo rpm -i thurvsa-*.rpm`.
# Default behavior: unmount the loopback iSCSI / NVMe-TCP volumes,
# log the sessions out, stop thurvsad, install the newest
# thurvsa-*.rpm in package-dir (default: the current directory) via
# dnf/yum/zypper (falling back to rpm -U), start thurvsad, log back
# in, and remount (by filesystem UUID). --dont-restart stops after
# the package swap so the operator can review the conffile before
# starting the daemon back up. --disconnect-only quiesces the host
# (unmount + logout) without touching the daemon or the package.
# --dry-run shows the plan without changing anything. Full sequence
# + caveats are in lib.sh.
#
set -euo pipefail
PKGFMT=rpm
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vsa
