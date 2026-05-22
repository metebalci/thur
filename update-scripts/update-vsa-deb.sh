#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vsa-deb.sh — upgrade thurvsad from a .deb package.
#
#   sudo ./update-vsa-deb.sh [--dry-run] [package-dir]
#
# Unmounts the loopback iSCSI / NVMe-TCP volumes, logs the sessions
# out, stops thurvsad, installs the newest thurvsa_*.deb in
# package-dir (default: the current directory), starts thurvsad,
# logs back in, and remounts (by filesystem UUID). --dry-run shows
# the plan without changing anything. Full sequence + caveats are in
# lib.sh.
#
set -euo pipefail
PKGFMT=deb
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vsa
