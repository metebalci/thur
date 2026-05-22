#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# update-vtl-deb.sh — upgrade thurvtld from a .deb package.
#
#   sudo ./update-vtl-deb.sh [--dry-run] [--force] [package-dir]
#
# Discovers any LTFS mounts (from the running `ltfs` processes) and
# loopback iSCSI sessions, unmounts LTFS, logs the sessions out,
# stops thurvtld, installs the newest thurvtl_*.deb in package-dir
# (default: the current directory), starts thurvtld, logs back in,
# and replays each `ltfs` mount. With no LTFS mounts it is just
# stop/install/start. --force proceeds even if backup-software
# sessions are connected; --dry-run shows the plan without changing
# anything. Full sequence + caveats are in lib.sh.
#
set -euo pipefail
PKGFMT=deb
here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
parse_args "$@"
update_vtl
