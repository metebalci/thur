#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# release.sh — build thurvtl + thurvsa release artifacts inside the
# Debian 11 builder image so glibc, openssl, and the cargo toolchain
# are pinned to a portable floor. Artifacts install cleanly on every
# mainstream Linux distro (RHEL 9 / SLES 15 SP6+ / Debian 12 /
# Ubuntu 24.04 / etc.).
#
# Produces under release-artifacts/:
#   thurvtl_<ver>-1_amd64.deb            .deb for Debian/Ubuntu (tape library)
#   thurvtl-<ver>-1.x86_64.rpm           .rpm for RHEL/SLES/openSUSE
#   thurvsa_<ver>-1_amd64.deb            .deb for Debian/Ubuntu (block target)
#   thurvsa-<ver>-1.x86_64.rpm           .rpm for RHEL/SLES/openSUSE
#
# This script always produces UNSIGNED artifacts. The canonical
# signed-and-validated release path is the tag-triggered
# .github/workflows/release.yml — it builds in the same container
# here, runs the per-distro smoke matrix, signs the artifacts, and
# publishes the GitHub Release. Use this script locally for dev
# iteration, smoke-testing changes to the packaging itself, and for
# the CI build step (which invokes it under the hood).
#
# Build cache: the cargo target dir is reused across runs by default
# (named podman volume `thur-builder-target`) so iteration is fast.
# Pass --no-cache to force a cold build (this is what release.yml
# does — promotable cuts must match what a fresh checkout produces).
#
# Usage:
#   release/release.sh                 # default: cache ON, clean tree required
#   release/release.sh --no-cache      # cold build (CI uses this)
#   release/release.sh --allow-dirty   # cut from a dirty working tree (local only)

set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="thur-builder:latest"

# Flags.
ALLOW_DIRTY=0
NO_CACHE=0
for arg in "$@"; do
    case "$arg" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --no-cache)    NO_CACHE=1 ;;
        *) echo "error: unknown flag: $arg" >&2; exit 1 ;;
    esac
done

# Two-stage script: outer half (host) builds the podman image and
# re-execs self inside it; inner half (container, sentinel env var
# set) does the actual cargo build + packaging.
if [ -z "${THUR_IN_BUILDER:-}" ]; then
    if ! command -v podman >/dev/null 2>&1; then
        echo "error: podman not on PATH (try: sudo apt install podman)" >&2
        exit 1
    fi

    # Refuse to cut a release with uncommitted or untracked changes.
    # A release must map to an identifiable commit — the binaries
    # embed the git SHA (THURVTL_VERSION / THURVSA_VERSION), and a
    # dirty tree stamps them `-dirty`, so the artifacts wouldn't be
    # reproducible from any tagged commit. Bypass with --allow-dirty
    # for local verification builds (artifacts will still build, just
    # don't publish them).
    if [ "$ALLOW_DIRTY" -eq 0 ]; then
        if ! git rev-parse --git-dir >/dev/null 2>&1; then
            echo "error: release.sh must run inside a git checkout " \
                 "(the working-tree cleanliness check needs git)" >&2
            exit 1
        fi
        if [ -n "$(git status --porcelain)" ]; then
            echo "error: working tree has uncommitted or untracked changes — refusing to cut a release." >&2
            echo "       release artifacts embed the git SHA; a dirty tree stamps them '-dirty' and they" >&2
            echo "       would not be reproducible from any tagged commit. Commit, stash, or remove the" >&2
            echo "       offending files." >&2
            echo "       Pass --allow-dirty to override (do not publish those artifacts)." >&2
            echo >&2
            git status --short >&2
            exit 1
        fi
    else
        echo "==> --allow-dirty: skipping working-tree cleanliness check"
    fi

    # Layer cache short-circuits this to seconds when the
    # Containerfile and its inputs haven't changed; first run is
    # ~10 min to install rustup + cargo-deb + cargo-generate-rpm.
    echo "==> podman build $IMAGE"
    podman build -t "$IMAGE" -f release/Containerfile.builder .

    # Volume strategy for /work/target (the cargo target dir):
    #   default       -- named podman volume `thur-builder-target`,
    #                    persisted across runs. Cuts incremental
    #                    builds from minutes to seconds.
    #   --no-cache    -- anonymous volume destroyed on container exit:
    #                    cold build, so what we ship matches what
    #                    cargo produces from a fresh checkout. CI
    #                    (.github/workflows/release.yml) passes this.
    # Either way, the host's own target/ is never mounted: it's built
    # against the host glibc and would fail with `GLIBC_2.32 not found`
    # inside the Debian 11 builder (cargo's fingerprint doesn't include
    # libc version).
    if [ "$NO_CACHE" -eq 1 ]; then
        echo "==> build cache OFF: cold build via throwaway target volume"
        TARGET_VOL_ARGS=(-v /work/target)
    else
        echo "==> build cache ON: reusing cargo target via named volume thur-builder-target"
        TARGET_VOL_ARGS=(-v thur-builder-target:/work/target)
    fi

    echo "==> podman run $IMAGE ./release/release.sh"
    INNER_ARGS=()
    [ "$NO_CACHE" -eq 1 ] && INNER_ARGS+=(--no-cache)
    exec podman run --rm \
        -v "$PWD:/work" \
        "${TARGET_VOL_ARGS[@]}" \
        -e THUR_IN_BUILDER=1 \
        "$IMAGE" \
        ./release/release.sh "${INNER_ARGS[@]}"
fi

# ---- Below this point we are inside the builder image. ----

OUT_DIR="release-artifacts"
# Always wipe stale artifacts from previous runs. The build cache is
# about cargo's target dir — orthogonal to the output dir. Mixed-version
# files in release-artifacts/ would make the final `ls -lh` ambiguous
# and confuse downstream tooling.
rm -rf -- "$OUT_DIR"
mkdir -p "$OUT_DIR"

# Pull the version out of the root Cargo.toml's [workspace.package]
# block — single source of truth for every crate (each inherits via
# `version.workspace = true`) and for the .deb / .rpm filenames
# stamped below.
VERSION=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)

# RPM Version: tags disallow `-`, so a SemVer pre-release like
# `0.1.0-alpha.1` cannot flow through verbatim. Split it across the
# Fedora-convention Version / Release fields:
#   0.1.0          -> Version 0.1.0, Release 1
#   0.1.0-alpha.1  -> Version 0.1.0, Release 0.alpha.1
# The leading `0.` on the prerelease Release is load-bearing — it
# sorts before the eventual GA `Release: 1`, so `rpm -q` / `dnf
# upgrade` order pre-release builds correctly relative to the final
# cut. cargo-deb does an equivalent translation (`-` -> `~`) for us.
if [[ "$VERSION" == *-* ]]; then
    RPM_VERSION="${VERSION%%-*}"
    RPM_RELEASE="0.${VERSION#*-}"
else
    RPM_VERSION="$VERSION"
    RPM_RELEASE="1"
fi

# Quality gates run inside the same isolated container as the build,
# against the same Rust toolchain and the same anonymous /work/target
# volume — so the bar that gates the release is the bar the artifacts
# were built against. `-D warnings` promotes any clippy warning to an
# error: a regression in a previously clean lint fails the cut here
# instead of shipping.
echo "==> cargo clippy --workspace --release --all-targets"
cargo clippy --workspace --release --all-targets -- -D warnings

echo "==> cargo test --workspace --release"
cargo test --workspace --release

echo "==> cargo build --release --workspace"
cargo build --release --workspace

echo "==> cargo deb (thurvtl, no rebuild)"
cargo deb --no-build --no-strip --package vtl-cli --output "$OUT_DIR/"

echo "==> cargo deb (thurvsa, no rebuild)"
cargo deb --no-build --no-strip --package vsa-cli --output "$OUT_DIR/"

# cargo-generate-rpm's `--package` is a *path* (cargo-deb's is a
# package *name*). After the Layout C reorg the crate paths are
# vtl/cli and vsa/cli — not vtl-cli / vsa-cli.
echo "==> cargo generate-rpm (thurvtl, no rebuild)"
cargo generate-rpm --package vtl/cli --output "$OUT_DIR/" \
    --set-metadata "version = \"$RPM_VERSION\"" \
    --set-metadata "release = \"$RPM_RELEASE\""

echo "==> cargo generate-rpm (thurvsa, no rebuild)"
cargo generate-rpm --package vsa/cli --output "$OUT_DIR/" \
    --set-metadata "version = \"$RPM_VERSION\"" \
    --set-metadata "release = \"$RPM_RELEASE\""

echo
echo "==> artifacts (unsigned — signing lives in .github/workflows/release.yml):"
ls -lh "$OUT_DIR"
