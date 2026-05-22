#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# scripts/coverage.sh — workspace test-coverage report.
#
# Modes:
#   (no args)   cargo llvm-cov --workspace --summary-only (per-file table)
#   --crates    per-crate line coverage vs. the tier-floor table
#   --gate      like --crates, but exit 1 if any crate is below its floor
#   --zero      list non-trivial source files with no #[cfg(test)] block
#               (reviewed-trivial files are listed in coverage-exempt.txt)
#
# Coverage floors (line %):
#   80  core/*, scsi/*, nvme/*, and the critical shared crates
#       (crypto, pool, iscsi, audit, keystore, cloud)
#   50  all other shared/* crates
#   30  product daemons + CLIs (vtl/*, vsa/*)
#
# The CI build-failure gate (--gate) is wired separately; a plain run is
# advisory. cargo-llvm-cov instruments only the `cargo test` suite — the
# end-to-end shell suites under vtl/scripts/ and vsa/scripts/ are not
# captured here, so daemon/CLI numbers read low by construction.

set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-summary}"

zero_mode() {
    local exempt="scripts/coverage-exempt.txt"
    local missing=0
    while IFS= read -r f; do
        # Skip the per-crate integration-test trees and build scripts.
        case "$f" in
            */tests/*|*/build.rs) continue ;;
        esac
        if grep -q '#\[cfg(test)\]' "$f"; then
            continue
        fi
        # Reviewed-trivial files (pure re-exports, bare type/enum/const
        # definitions) are exempt — see coverage-exempt.txt.
        if [[ -f "$exempt" ]] && grep -qxF "$f" "$exempt"; then
            continue
        fi
        echo "  $f"
        missing=$((missing + 1))
    done < <(find . \( -path ./target -o -path ./.git -o -path ./.claude \) -prune \
        -o -name '*.rs' -path '*/src/*' -print | sort)

    if [[ "$missing" -eq 0 ]]; then
        echo "All non-trivial source files have a #[cfg(test)] block."
        return 0
    fi
    echo
    echo "$missing non-trivial file(s) with no #[cfg(test)] block."
    echo "Add a test, or — if genuinely trivial — list the path in $exempt."
    return 1
}

crate_report() {
    local json="/tmp/thur-coverage.json"
    echo "Collecting coverage (cargo llvm-cov --workspace) ..." >&2
    cargo llvm-cov --workspace --json --output-path "$json" >&2
    python3 scripts/coverage-report.py "$json" "$@"
}

case "$MODE" in
    summary)
        cargo llvm-cov --workspace --summary-only
        ;;
    --crates)
        crate_report
        ;;
    --gate)
        crate_report --gate
        ;;
    --zero)
        zero_mode
        ;;
    *)
        echo "usage: $0 [--crates|--gate|--zero]" >&2
        exit 2
        ;;
esac
