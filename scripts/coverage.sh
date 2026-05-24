#!/bin/bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# scripts/coverage.sh — workspace test-coverage report.
#
# Modes:
#   (no args)        cargo llvm-cov --workspace --summary-only (per-file table)
#   --crates         per-crate line coverage vs. the tier-floor table
#                    (unit tests only — `cargo test` paths)
#   --gate           like --crates, but exit 1 if any crate is below its floor
#   --integrated     end-to-end coverage: unit tests PLUS instrumented daemon
#                    runs from the shell suites under vtl/scripts/ and
#                    vsa/scripts/. Self-elevates via sudo for the kernel-
#                    initiator suites. Takes 10-20 minutes.
#   --integrated-gate same as --integrated, but exit 1 below floor
#   --zero           list non-trivial source files with no #[cfg(test)] block
#                    (reviewed-trivial files are listed in coverage-exempt.txt)
#
# Coverage floors (line %) — see scripts/coverage-report.py for the table.
# Tier-1 critical sits at 80%, tier-2 control-plane critical at 80%,
# standard shared at 50%, products at 30% (raised once integrated mode
# becomes the default measurement).

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

# --integrated mode: unit tests + shell-suite daemon runs, one merged
# report. The shell suites drive the compiled `thurv{tl,sa}d` binaries
# (which `cargo llvm-cov report` doesn't auto-discover), so we run the
# underlying `llvm-profdata merge` + `llvm-cov export` pipeline by hand
# with every binary the daemon-side could fire wired up via `-object`.
#
# Three phases:
#   1. Build the bin-target instrumented binaries + run unit tests
#      (these populate target/llvm-cov-target/thur-*.profraw).
#   2. Run the no-sudo shell suites with LLVM_PROFILE_FILE set to the
#      same profraw pattern, then re-invoke the sudo-required ones via
#      `sudo LLVM_PROFILE_FILE=... <script>` so the daemon child
#      process inherits the env var across the sudo boundary.
#   3. Merge every profraw into one profdata, export JSON via the
#      LLVM toolchain bundled with rustup, feed coverage-report.py.
integrated_report() {
    # Caller passes --gate (or nothing); we always tack on --integrated
    # so coverage-report.py applies the stricter floor map.
    local extra_args=("--integrated")
    if [[ "${1:-}" == "--gate" ]]; then
        extra_args+=("--gate")
    fi
    local llvm_bin
    llvm_bin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | awk '/^host:/{print $2}')/bin"
    local PROFDATA="${llvm_bin}/llvm-profdata"
    local LLVMCOV="${llvm_bin}/llvm-cov"
    if [[ ! -x "$PROFDATA" || ! -x "$LLVMCOV" ]]; then
        echo "rustup llvm-tools not found at ${llvm_bin}" >&2
        echo "  install with: rustup component add llvm-tools-preview" >&2
        exit 2
    fi

    local llvmcov_dir="target/llvm-cov-target"
    local shell_profraw_pattern="${PWD}/${llvmcov_dir}/thur-shell-%p-%8m.profraw"

    echo "[1/4] Unit tests via cargo-llvm-cov ..." >&2
    # Run cargo-llvm-cov BEFORE eval'ing show-env. The reverse order trips
    # the documented "cargo-llvm-cov subcommands other than report and
    # clean may not work correctly in context where environment variables
    # are set by show-env" warning, which silently produces non-
    # instrumented test bins for ~85% of workspace crates (and sometimes
    # SIGKILLs mid-build). Test bins land in target/llvm-cov-target/debug/deps/.
    cargo llvm-cov clean --workspace >&2
    cargo llvm-cov --no-report --workspace >&2

    echo "[2/4] Instrumented daemon binaries for shell suites ..." >&2
    # Now safe to eval show-env — we won't invoke cargo-llvm-cov again
    # under this env. The wrapper instruments `cargo build` outputs at
    # target/debug/, which is where the shell suites look for thurv{tl,sa}d.
    eval "$(cargo llvm-cov show-env --sh 2>/dev/null)"
    mkdir -p "$llvmcov_dir"
    export LLVM_PROFILE_FILE="$shell_profraw_pattern"
    cargo build --workspace --bins >&2

    echo "[3/4] Driving shell suites (sudo'd kernel-initiator runs included) ..." >&2
    # `set +e` for the loop: a single failing suite shouldn't abandon
    # the whole capture — we still want partial coverage data merged.
    set +e
    local no_sudo=(
        "vsa/scripts/test-smoke.sh"
        "vtl/scripts/test-smoke.sh"
        "vsa/scripts/test-iscsi-conformance.sh"
        "vtl/scripts/test-iscsi-conformance.sh"
        "vsa/scripts/test-crash-audit-append.sh"
        "vtl/scripts/test-crash-audit-append.sh"
        "vtl/scripts/test-backup-cloud-resume.sh"
        "scripts/test-coresident-smoke.sh"
    )
    local soak=(
        "THURVSA_SOAK=1 vsa/scripts/test-multi-volume-dedup.sh"
        "THURVTL_SOAK=1 vtl/scripts/test-many-cartridge-lifecycle.sh"
    )
    local sudo_set=(
        "vsa/scripts/test-iscsi-fs-workflow.sh"
        "vsa/scripts/test-scsi-conformance.sh"
        "vtl/scripts/test-scsi-conformance.sh"
        "vtl/scripts/test-backup-workflow.sh"
        "vsa/scripts/test-nvmetcp-conformance.sh"
        "vsa/scripts/test-nvme-fs-workflow.sh"
        "vsa/scripts/test-crash-page-flush.sh"
        "vtl/scripts/test-crash-chunk-seal.sh"
    )
    for s in "${no_sudo[@]}"; do
        echo "  - $s" >&2
        LLVM_PROFILE_FILE="$shell_profraw_pattern" ./"$s" >/dev/null 2>&1 || true
    done
    for entry in "${soak[@]}"; do
        echo "  - $entry" >&2
        env $entry LLVM_PROFILE_FILE="$shell_profraw_pattern" bash -c \
            'eval "$1 ./$2"' _ "${entry%% *}" "${entry##* }" >/dev/null 2>&1 || true
    done
    for s in "${sudo_set[@]}"; do
        echo "  - sudo $s" >&2
        sudo LLVM_PROFILE_FILE="$shell_profraw_pattern" ./"$s" >/dev/null 2>&1 || true
    done
    set -e

    echo "[4/4] Merging + exporting integrated report ..." >&2
    # Profraws from step [1/4] land where cargo-llvm-cov puts them
    # (target/llvm-cov-target/ and sometimes target/), step [3/4]'s
    # shell suites use $shell_profraw_pattern under target/llvm-cov-target/.
    # Glob the union.
    local profraws=()
    while IFS= read -r -d '' p; do
        profraws+=("$p")
    done < <(find target -name '*.profraw' -print0 2>/dev/null)
    echo "  collected ${#profraws[@]} profraw file(s)" >&2

    "$PROFDATA" merge -sparse "${profraws[@]}" -o /tmp/thur-integrated.profdata >&2

    # Build -object list: instrumented test bins (cargo-llvm-cov put
    # them in target/llvm-cov-target/debug/deps/) plus the four daemon
    # bins the shell suites spawn (target/debug/, built in step [2/4]).
    local obj_args=()
    local b
    while IFS= read -r -d '' b; do
        obj_args+=(-object="$b")
    done < <(find target/llvm-cov-target/debug/deps -maxdepth 1 -type f -executable \
        -not -name "*.d" -not -name "*.rlib" -not -name "*.so" -print0 2>/dev/null)
    for b in target/debug/thurvtld target/debug/thurvsad \
             target/debug/thurvtl target/debug/thurvsa; do
        [[ -x "$b" ]] && obj_args+=(-object="$b")
    done

    "$LLVMCOV" export \
        -instr-profile /tmp/thur-integrated.profdata \
        -format=text \
        --ignore-filename-regex='(rustc|\.cargo/registry|\.rustup|/target/)' \
        "${obj_args[@]}" > /tmp/thur-integrated.json 2>/dev/null

    python3 scripts/coverage-report.py /tmp/thur-integrated.json "${extra_args[@]}"
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
    --integrated)
        integrated_report
        ;;
    --integrated-gate)
        integrated_report --gate
        ;;
    --zero)
        zero_mode
        ;;
    *)
        echo "usage: $0 [--crates|--gate|--integrated|--integrated-gate|--zero]" >&2
        exit 2
        ;;
esac
