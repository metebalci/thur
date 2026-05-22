#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vsa-rpm.sh — upgrade thurvsad from an .rpm package.
#
#   sudo ./update-vsa-rpm.sh [--dry-run] [package-dir]
#
# Unmounts the loopback iSCSI / NVMe-TCP volumes, logs the sessions
# out, stops thurvsad, installs the newest thurvsa-*.rpm in
# package-dir (default: the current directory) via dnf/yum/zypper
# (falling back to rpm -U), starts thurvsad, logs back in, and
# remounts (by filesystem UUID). --dry-run shows the plan without
# changing anything. Full sequence + caveats are in lib.sh.
#
set -euo pipefail
PKGFMT=rpm
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vsa
