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
CRITICAL_80 = [
    "core/stream", "core/mediachanger", "core/block",
    "scsi/spc", "scsi/ssc", "scsi/smc", "scsi/sbc",
    "nvme/base", "nvme/nvm", "nvme/tcp",
    "shared/crypto", "shared/pool", "shared/iscsi",
    "shared/audit", "shared/keystore", "shared/cloud",
]
PRODUCTS_30 = ["vtl/daemon", "vtl/cli", "vsa/daemon", "vsa/cli"]
SHARED_50 = [
    "shared/admin-audit", "shared/admin-client", "shared/admin-http",
    "shared/admin-iscsi", "shared/admin-proto", "shared/admin-server",
    "shared/alerting", "shared/cli", "shared/cli-alerting",
    "shared/cli-iscsi", "shared/cli-system", "shared/cloud-bench",
    "shared/dedup-stats", "shared/health", "shared/naming",
    "shared/telemetry", "shared/upload-worker", "shared/verify-core",
]

FLOOR = {}
for d in CRITICAL_80:
    FLOOR[d] = 80
for d in PRODUCTS_30:
    FLOOR[d] = 30
for d in SHARED_50:
    FLOOR[d] = 50

MEMBERS = sorted(FLOOR, key=len, reverse=True)


def crate_for(path, root):
    """Map an absolute source path to its workspace-member directory."""
    rel = os.path.relpath(path, root)
    for m in MEMBERS:
        if rel == m or rel.startswith(m + os.sep):
            return m
    return None


def main():
    json_path = sys.argv[1]
    gate = "--gate" in sys.argv[2:]
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

    tiers = [
        ("Critical (floor 80%)", CRITICAL_80, 80),
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
            if pct + 1e-9 >= floor:
                status = "ok"
            else:
                status = "LOW  (-{:.0f})".format(floor - pct)
                below.append((c, pct, floor))
            print("  {:<24} {:>6.1f}  {:>6}  {}".format(
                c, pct, floor, status))

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
