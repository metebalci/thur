#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# release.sh — build thurvtl + thurvsa release artifacts
# inside the Debian 11 builder image so glibc, openssl, and the cargo
# toolchain are pinned to a portable floor. Artifacts install cleanly
# on every mainstream Linux distro (RHEL 9 / SLES 15 SP6+ /
# Debian 12 / Ubuntu 24.04 / etc.).
#
# Produces under release-artifacts/:
#   thurvtl_<ver>-1_amd64.deb            .deb for Debian/Ubuntu (tape library)
#   thurvtl-<ver>-1.x86_64.rpm           .rpm for RHEL/SLES/openSUSE
#   thurvtl-<ver>-x86_64.tar.gz          Static binaries + reference config
#   thurvsa_<ver>-1_amd64.deb            .deb for Debian/Ubuntu (block target)
#   thurvsa-<ver>-1.x86_64.rpm           .rpm for RHEL/SLES/openSUSE
#   thurvsa-<ver>-x86_64.tar.gz          Static binaries + reference config
#
# Two product packages keep operators free to install only the pieces
# they need (`thurvtl` tape library on port 3260, `thurvsa` block target on
# port 3260; co-resident installs work because the system users /
# data dirs / conffile paths / unit names are disjoint).
#
# All artifacts ship under the Apache License, Version 2.0.
#
# Signing is opt-in via `--sign` (requires THUR_GPG_KEY_ID env var set
# to a gpg key fingerprint). For dev / alpha / beta cuts the flag is
# optional; for release-candidate and final cuts release.sh refuses
# to proceed without it (chain of trust to the release key is
# load-bearing once a cut hits a public repo). Your host's ~/.gnupg
# is mounted into the container only when --sign is passed.
#
# Build cache: the cargo target dir is reused across runs (a named
# podman volume) by default on dev / alpha cuts for fast iteration;
# pass --no-cache to force a cold build. beta / rc / final cuts
# always build cold — passing --keep-cache to one is a hard error.
#
# Usage:
#   release/release.sh                              # unsigned dev/alpha/beta cut
#   THUR_GPG_KEY_ID=ABC… release/release.sh --sign  # signed cut
#   release/release.sh --allow-dirty                # cut from a dirty
#                                                   # working tree (testing only)
#   release/release.sh --no-cache                   # force a cold build (already
#                                                   # the default on beta/rc/final)
#   release/release.sh --keep-cache                 # explicit cache opt-in; already
#                                                   # the dev/alpha default, and
#                                                   # an error on beta/rc/final

set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="thur-builder:latest"

# Parse host-side flags. `--sign` opts in to detach-signing every
# artifact via THUR_GPG_KEY_ID (required when --sign is passed; the
# env var alone no longer triggers signing). `--allow-dirty` skips
# the working-tree cleanliness gate (see below). `--keep-cache` /
# `--no-cache` request the build cache on / off; the effective
# decision (KEEP_CACHE) is resolved against the version channel just
# below. The two flags are mutually exclusive.
SIGN=0
ALLOW_DIRTY=0
WANT_KEEP_CACHE=0
WANT_NO_CACHE=0
for arg in "$@"; do
    case "$arg" in
        --sign) SIGN=1 ;;
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --keep-cache) WANT_KEEP_CACHE=1 ;;
        --no-cache) WANT_NO_CACHE=1 ;;
        *) echo "error: unknown flag: $arg" >&2; exit 1 ;;
    esac
done
if [ "$WANT_KEEP_CACHE" -eq 1 ] && [ "$WANT_NO_CACHE" -eq 1 ]; then
    echo "error: --keep-cache and --no-cache are mutually exclusive." >&2
    exit 1
fi

# Signing-enforcement gate. Read the workspace version off the host
# filesystem (root Cargo.toml's [workspace.package].version, same
# field the inner half stamps onto artifact filenames) and refuse to
# proceed unsigned if the cut is anything other than dev / alpha /
# beta. Conservative default: unknown / future prerelease channels
# (rc, preview, snapshot, …) and the final cut all require --sign.
# Done on the host BEFORE podman build so the operator gets a fast
# error instead of waiting through the container build first.
VERSION=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)
case "$VERSION" in
    *-alpha*|*-beta*|*-dev*) SIGN_REQUIRED=0 ;;
    *)                       SIGN_REQUIRED=1 ;;
esac
if [ "$SIGN_REQUIRED" -eq 1 ] && [ "$SIGN" -eq 0 ]; then
    echo "error: version $VERSION is a release-candidate or final cut — refusing to build unsigned." >&2
    echo "       set THUR_GPG_KEY_ID and pass --sign to release.sh." >&2
    exit 1
fi
if [ "$SIGN" -eq 1 ] && [ -z "${THUR_GPG_KEY_ID:-}" ]; then
    echo "error: --sign passed but THUR_GPG_KEY_ID is not set." >&2
    exit 1
fi

# Build-cache policy. The "cache" is the cargo target dir, reused
# across container runs via the named podman volume
# `thur-builder-target` (versus an anonymous volume thrown away when
# the container exits).
#
#   dev / alpha cuts  -- cache ON by default for fast iteration;
#                        pass --no-cache to force a cold build.
#   beta / rc / final -- always a cold build; passing --keep-cache to
#                        one is a hard error. A promotable cut must
#                        match what a fresh checkout produces, and
#                        incremental cargo state can mask
#                        reproducibility issues.
#
# Resolved in this shared section so the host half (target-volume
# choice) and the inner half (release-artifacts/ wipe) agree. Channel
# detection mirrors the signing gate above: anything that is not an
# explicit dev / alpha string is treated as cache-ineligible — the
# same conservative default the signing gate applies.
case "$VERSION" in
    *-alpha*|*-dev*) CACHE_ELIGIBLE=1 ;;
    *)               CACHE_ELIGIBLE=0 ;;
esac
if [ "$CACHE_ELIGIBLE" -eq 1 ]; then
    if [ "$WANT_NO_CACHE" -eq 1 ]; then KEEP_CACHE=0; else KEEP_CACHE=1; fi
else
    # beta / rc / final: a cold build is mandatory. --keep-cache is a
    # hard error rather than a silent downgrade — the operator asked
    # for something a promotable cut must not do.
    if [ "$WANT_KEEP_CACHE" -eq 1 ]; then
        echo "error: --keep-cache is not allowed for $VERSION — beta/rc/final cuts must" >&2
        echo "       build cold so the artifacts match a fresh checkout. Drop --keep-cache" >&2
        echo "       (the cut builds cold either way)." >&2
        exit 1
    fi
    KEEP_CACHE=0
fi

# Two-stage script: outer half (host) builds the podman image and
# re-execs self inside it; inner half (container, sentinel env var
# set) does the actual cargo build + packaging + signing.
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
    # don't publish them). Host-side only because once we exec into
    # the container the working tree is already locked in.
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

    # Mount the host GPG agent only when the operator opted into
    # signing. Without --sign the container has no path to the host's
    # secret keys even by accident.
    GPG_MOUNT=()
    if [ "$SIGN" -eq 1 ] && [ -d "${HOME}/.gnupg" ]; then
        GPG_MOUNT=(-v "${HOME}/.gnupg:/root/.gnupg")
    fi

    # Volume strategy for /work/target (the cargo target dir), driven
    # by the KEEP_CACHE decision resolved above:
    #   KEEP_CACHE=1  -- named podman volume `thur-builder-target`,
    #                    persisted across runs (the dev/alpha default).
    #                    Cuts incremental builds from minutes to
    #                    seconds during development.
    #   KEEP_CACHE=0  -- anonymous volume, destroyed on container exit
    #                    (--rm): a cold build, so what we ship matches
    #                    what cargo produces from a fresh checkout.
    #                    Always the case for beta/rc/final cuts.
    # Either way, the host's own target/ is never mounted: it's built
    # against the host glibc and would fail with `GLIBC_2.32 not found`
    # inside the Debian 11 builder (cargo's fingerprint doesn't include
    # libc version).
    # -it: allocate a TTY and keep stdin attached so gpg-agent's
    # pinentry can prompt for the signing-key passphrase. Without
    # this, signing fails with "Inappropriate ioctl for device".
    # Harmless for non-signing runs.
    if [ "$KEEP_CACHE" -eq 1 ]; then
        echo "==> build cache ON: reusing cargo target via named volume thur-builder-target"
        TARGET_VOL_ARGS=(-v thur-builder-target:/work/target)
    else
        echo "==> build cache OFF: cold build via throwaway target volume"
        TARGET_VOL_ARGS=(-v /work/target)
    fi

    echo "==> podman run $IMAGE ./release/release.sh"
    INNER_ARGS=()
    if [ "$SIGN" -eq 1 ]; then
        INNER_ARGS+=(--sign)
    fi
    # Forward the *resolved* cache decision, not the raw flags: the
    # inner half re-runs the same resolution and must land on the
    # same KEEP_CACHE without re-deriving the channel or re-printing
    # the ignored-flag warning.
    if [ "$KEEP_CACHE" -eq 1 ]; then
        INNER_ARGS+=(--keep-cache)
    else
        INNER_ARGS+=(--no-cache)
    fi
    exec podman run --rm -it \
        -v "$PWD:/work" \
        "${TARGET_VOL_ARGS[@]}" \
        "${GPG_MOUNT[@]}" \
        -e THUR_GPG_KEY_ID \
        -e THUR_IN_BUILDER=1 \
        "$IMAGE" \
        ./release/release.sh "${INNER_ARGS[@]}"
fi

# ---- Below this point we are inside the builder image. ----

OUT_DIR="release-artifacts"
# Wipe stale artifacts from previous runs. Without this, the signing
# loop further down (globs *.deb / *.rpm / *.tar.gz) would re-sign
# old version-stamped files alongside the new ones, and `ls -lh`
# at the end mixes them. We're inside the builder container with
# CWD pinned to the workspace root, so the path is unambiguous.
# When the build cache is on (KEEP_CACHE=1, the dev/alpha default)
# the wipe is skipped — fast-iteration mode, the operator accepts
# mixed-version artifacts piling up. beta/rc/final always wipe.
if [ "$KEEP_CACHE" -eq 0 ]; then
    rm -rf -- "$OUT_DIR"
fi
mkdir -p "$OUT_DIR"

# Pull the version + arch out of the root Cargo.toml's
# [workspace.package] block — single source of truth for every crate
# (each inherits via `version.workspace = true`) and for the tarball
# / .deb / .rpm filenames stamped below.
VERSION=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)
ARCH=$(uname -m)
THURVTL_TARBALL_DIR="thurvtl-${VERSION}-${ARCH}"
THURVTL_TARBALL="${THURVTL_TARBALL_DIR}.tar.gz"
THURVSA_TARBALL_DIR="thurvsa-${VERSION}-${ARCH}"
THURVSA_TARBALL="${THURVSA_TARBALL_DIR}.tar.gz"

# RPM Version: tags disallow `-`, so a SemVer pre-release like
# `0.1.0-alpha.1` cannot flow through verbatim. Split it across the
# Fedora-convention Version / Release fields:
#   0.1.0          -> Version 0.1.0, Release 1
#   0.1.0-alpha.1  -> Version 0.1.0, Release 0.alpha.1
# The leading `0.` on the prerelease Release is load-bearing — it
# sorts before the eventual GA `Release: 1`, so `rpm -q` / `dnf
# upgrade` order pre-release builds correctly relative to the final
# cut. cargo-deb does an equivalent translation (`-` -> `~`) for us;
# the tarball keeps the SemVer string verbatim.
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
# instead of shipping. The deny-level safety set
# (unwrap_used / panic / unwrap_in_result / unsafe_code) was already
# baked into the workspace lints; this just enforces the rest of the
# bar consistently.
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

echo "==> static binary tarball $THURVTL_TARBALL"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$THURVTL_TARBALL_DIR"
install -m 755 target/release/thurvtld              "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 755 target/release/thurvtl                 "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 release/thurvtld.service    "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 release/thurvtl.yaml              "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 release/thurvtl.env              "$STAGE/$THURVTL_TARBALL_DIR/thurvtl.env"
install -m 644 dist/thurvtl.defaults.yaml                 "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 dist/thurvtl-completion.bash           "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 dist/thurvtl-completion.zsh            "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 dist/thurvtl.1                         "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 release/thurvtld.8                     "$STAGE/$THURVTL_TARBALL_DIR/"
install -m 644 LICENSE                                   "$STAGE/$THURVTL_TARBALL_DIR/LICENSE"
install -m 644 release/thurvtl-tarball-README.md "$STAGE/$THURVTL_TARBALL_DIR/README.md"
tar --owner=0 --group=0 -czf "$OUT_DIR/$THURVTL_TARBALL" -C "$STAGE" "$THURVTL_TARBALL_DIR"

echo "==> static binary tarball $THURVSA_TARBALL"
mkdir -p "$STAGE/$THURVSA_TARBALL_DIR"
install -m 755 target/release/thurvsad          "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 755 target/release/thurvsa             "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 release/thurvsad.service    "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 release/thurvsa.yaml              "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 release/thurvsa.env              "$STAGE/$THURVSA_TARBALL_DIR/thurvsa.env"
install -m 644 dist/thurvsa.defaults.yaml                 "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 dist/thurvsa-completion.bash           "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 dist/thurvsa-completion.zsh            "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 dist/thurvsa.1                         "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 release/thurvsad.8                     "$STAGE/$THURVSA_TARBALL_DIR/"
install -m 644 LICENSE                                   "$STAGE/$THURVSA_TARBALL_DIR/LICENSE"
install -m 644 release/thurvsa-tarball-README.md "$STAGE/$THURVSA_TARBALL_DIR/README.md"
tar --owner=0 --group=0 -czf "$OUT_DIR/$THURVSA_TARBALL" -C "$STAGE" "$THURVSA_TARBALL_DIR"

if [ "$SIGN" -eq 1 ]; then
    # gpg-agent uses GPG_TTY to know where to send pinentry prompts.
    # The host's value (if any) doesn't apply inside the container —
    # set it to whatever TTY podman -it gave us.
    export GPG_TTY=$(tty)
    echo "==> signing artifacts with key $THUR_GPG_KEY_ID"
    for f in "$OUT_DIR"/*.deb "$OUT_DIR"/*.rpm "$OUT_DIR"/*.tar.gz; do
        [ -f "$f" ] || continue
        rm -f "${f}.asc"
        gpg --batch --yes \
            --local-user "$THUR_GPG_KEY_ID" \
            --detach-sign --armor \
            --output "${f}.asc" \
            "$f"
        echo "    signed ${f}.asc"
    done
else
    echo "==> --sign not passed — skipping signatures (allowed for $VERSION)"
fi

echo
echo "==> artifacts:"
ls -lh "$OUT_DIR"
