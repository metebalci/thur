# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# scripts/coverage-report.py — per-crate line-coverage table from a
# cargo-llvm-cov JSON export. Invoked by coverage.sh; not run directly.
#
# Groups instrumented files by workspace-member directory, sums line
# counts per crate, and compares each crate against its tier floor.

import json
import os
import sys

# Workspace members, grouped by coverage floor (line %).
#
# Two critical tiers — separated by failure mode, not by criticality
# level. Tier 1 is data-path: bugs corrupt or lose on-disk / storage
# data silently. Tier 2 is control-plane: bugs cause silent
# operational failures (admin socket down, alert never fires, integrity
# check skipped) or unrecoverable backups.
CRITICAL_DATA_PATH_80 = [
    "core/stream", "core/mediachanger", "core/block",
    "scsi/spc", "scsi/ssc", "scsi/smc", "scsi/sbc",
    "nvme/base", "nvme/nvm", "nvme/tcp",
    "shared/crypto", "shared/pool", "shared/iscsi",
    "shared/audit", "shared/keystore", "shared/object-store",
]
CRITICAL_CONTROL_PLANE_80 = [
    "shared/admin-server",   # admin Unix-socket bind, peer-cred, jobs
    "shared/verify-core",    # pool / storage integrity sweep
    "shared/upload-worker",  # storage PUT + HEAD-probe primitive
    "shared/dedup-stats",    # operator-visible dedup math
]
PRODUCTS_30 = ["vtl/daemon", "vtl/cli", "vsa/daemon", "vsa/cli"]
SHARED_50 = [
    "shared/admin-audit", "shared/admin-client", "shared/admin-http",
    "shared/admin-iscsi", "shared/admin-proto",
    "shared/alerting", "shared/cli", "shared/cli-alerting",
    "shared/cli-iscsi", "shared/cli-system", "shared/object-store-bench",
    "shared/health", "shared/naming",
    "shared/telemetry",
]

# Two floor maps: one for `--crates` (unit tests only), one for
# `--integrated`. Control-plane critical crates carry an integrated-
# mode floor of 80% but stay at 50% in unit mode — their request
# paths fire mainly via the shell suites, so demanding the strict
# bar in unit mode would gate CI on tests that already exist (just
# in a different runner).
FLOOR_UNIT = {}
for d in CRITICAL_DATA_PATH_80:
    FLOOR_UNIT[d] = 80
for d in CRITICAL_CONTROL_PLANE_80:
    FLOOR_UNIT[d] = 50
for d in PRODUCTS_30:
    FLOOR_UNIT[d] = 30
for d in SHARED_50:
    FLOOR_UNIT[d] = 50

FLOOR_INTEGRATED = {}
for d in CRITICAL_DATA_PATH_80:
    FLOOR_INTEGRATED[d] = 80
for d in CRITICAL_CONTROL_PLANE_80:
    FLOOR_INTEGRATED[d] = 80
for d in PRODUCTS_30:
    FLOOR_INTEGRATED[d] = 30
for d in SHARED_50:
    FLOOR_INTEGRATED[d] = 50

MEMBERS = sorted(FLOOR_UNIT, key=len, reverse=True)


def crate_for(path, root):
    """Map an absolute source path to its workspace-member directory."""
    rel = os.path.relpath(path, root)
    for m in MEMBERS:
        if rel == m or rel.startswith(m + os.sep):
            return m
    return None


def main():
    json_path = sys.argv[1]
    extra = sys.argv[2:]
    gate = "--gate" in extra
    # `--integrated` picks the stricter floor map; default is unit-mode.
    integrated = "--integrated" in extra
    FLOOR = FLOOR_INTEGRATED if integrated else FLOOR_UNIT
    root = os.getcwd()

    with open(json_path) as fh:
        data = json.load(fh)

    # crate -> [covered_lines, total_lines]
    agg = {m: [0, 0] for m in FLOOR}
    for export in data.get("data", []):
        for f in export.get("files", []):
            crate = crate_for(f["filename"], root)
            if crate is None:
                continue
            lines = f["summary"]["lines"]
            agg[crate][0] += lines["covered"]
            agg[crate][1] += lines["count"]

    cp_floor = 80 if integrated else 50
    tiers = [
        ("Critical data-path (floor 80%)", CRITICAL_DATA_PATH_80, 80),
        (f"Critical control-plane (floor {cp_floor}%)",
         CRITICAL_CONTROL_PLANE_80, cp_floor),
        ("Shared (floor 50%)", SHARED_50, 50),
        ("Products (floor 30%)", PRODUCTS_30, 30),
    ]

    below = []
    for title, crates, floor in tiers:
        print()
        print(title)
        print("  {:<24} {:>7}  {:>6}  {}".format(
            "crate", "line %", "floor", "status"))
        for c in sorted(crates):
            covered, total = agg[c]
            pct = (100.0 * covered / total) if total else 0.0
            crate_floor = FLOOR[c]
            if pct + 1e-9 >= crate_floor:
                status = "ok"
            else:
                status = "LOW  (-{:.0f})".format(crate_floor - pct)
                below.append((c, pct, crate_floor))
            print("  {:<24} {:>6.1f}  {:>6}  {}".format(
                c, pct, crate_floor, status))

    print()
    if below:
        print("{} crate(s) below floor:".format(len(below)))
        for c, pct, floor in below:
            print("  {:<24} {:.1f}% < {}%".format(c, pct, floor))
        if gate:
            sys.exit(1)
    else:
        print("All crates at or above their coverage floor.")


if __name__ == "__main__":
    main()
