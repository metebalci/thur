#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# ghrelease.sh — tag the release commit and publish it to GitHub Releases.
#
# The second half of a release cut. Run this AFTER release/release.sh has
# built (and signed) the artifacts into release-artifacts/ and AFTER you
# have smoke-tested them. It performs the tag -> push -> publish steps:
#
#   1. Creates a signed git tag `v<version>` on HEAD. The version is read
#      from the root Cargo.toml — the same field release.sh stamps onto
#      the artifact filenames — so the tag cannot drift from the build.
#   2. Pushes the current branch and the tag to `origin`.
#   3. Creates the GitHub Release for the tag and uploads everything in
#      release-artifacts/ (.deb / .rpm + any .asc signatures).
#
# The GitHub Release body is the annotated tag's message
# (`gh release create --notes-from-tag`). A SemVer pre-release version
# (`-alpha` / `-beta` / `-rc`) is published as a GitHub pre-release.
#
# Prerequisites: `gh` installed and authenticated (`gh auth login`), an
# `origin` remote, and a git signing key (`git config user.signingkey`).

set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="release-artifacts"

usage() {
    cat <<'EOF'
ghrelease.sh — tag the release commit and publish it to GitHub Releases.
Run after release/release.sh and after smoke-testing the artifacts.

Usage:
  release/ghrelease.sh                 tag message via $EDITOR
  release/ghrelease.sh -m "summary"    tag message inline
  release/ghrelease.sh -F notes.md     tag message from a file
  release/ghrelease.sh -y              skip the confirmation prompt

The tag is v<version> (from the root Cargo.toml); the GitHub Release
body is the tag message; a -alpha/-beta/-rc version is a pre-release.
EOF
}

# ---- flags ----
TAG_MSG=""
TAG_MSG_FILE=""
ASSUME_YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        -m)              [ $# -ge 2 ] || { echo "error: -m needs a value" >&2; exit 1; }
                         TAG_MSG="$2"; shift 2 ;;
        -F|--notes-file) [ $# -ge 2 ] || { echo "error: -F needs a value" >&2; exit 1; }
                         TAG_MSG_FILE="$2"; shift 2 ;;
        -y|--yes)        ASSUME_YES=1; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               echo "error: unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done
if [ -n "$TAG_MSG" ] && [ -n "$TAG_MSG_FILE" ]; then
    echo "error: pass only one of -m / -F." >&2
    exit 1
fi
if [ -n "$TAG_MSG_FILE" ] && [ ! -f "$TAG_MSG_FILE" ]; then
    echo "error: notes file not found: $TAG_MSG_FILE" >&2
    exit 1
fi

# ---- preconditions ----
command -v gh >/dev/null 2>&1 || {
    echo "error: gh (GitHub CLI) not on PATH — see https://cli.github.com" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || {
    echo "error: gh is not authenticated — run 'gh auth login'." >&2; exit 1; }
git rev-parse --git-dir >/dev/null 2>&1 || {
    echo "error: not inside a git checkout." >&2; exit 1; }
git remote get-url origin >/dev/null 2>&1 || {
    echo "error: no 'origin' remote configured." >&2; exit 1; }

BRANCH=$(git branch --show-current)
[ -n "$BRANCH" ] || {
    echo "error: detached HEAD — check out the release branch first." >&2; exit 1; }

# A release tag must mark a clean, committed state: the artifacts were
# built from HEAD and the binaries embed its SHA.
if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty — commit or stash before releasing." >&2
    git status --short >&2
    exit 1
fi

# ---- version + tag name ----
VERSION=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)
[ -n "$VERSION" ] || {
    echo "error: could not read version from Cargo.toml." >&2; exit 1; }
TAG="v${VERSION}"

# SemVer pre-release (any '-' suffix) -> GitHub pre-release.
PRERELEASE_ARG=()
PRERELEASE_NOTE="no"
case "$VERSION" in
    *-*) PRERELEASE_ARG=(--prerelease); PRERELEASE_NOTE="yes" ;;
esac

# ---- artifacts ----
[ -d "$OUT_DIR" ] || {
    echo "error: $OUT_DIR/ not found — run release/release.sh first." >&2; exit 1; }

shopt -s nullglob
ASSETS=("$OUT_DIR"/*)
ASC=("$OUT_DIR"/*.asc)
shopt -u nullglob

[ ${#ASSETS[@]} -gt 0 ] || {
    echo "error: $OUT_DIR/ is empty — run release/release.sh first." >&2; exit 1; }

if [ ${#ASC[@]} -gt 0 ]; then
    SIG_NOTE="${#ASC[@]} present"
else
    SIG_NOTE="ABSENT (unsigned)"
fi

# ---- refuse to clobber an existing release ----
if gh release view "$TAG" >/dev/null 2>&1; then
    echo "error: a GitHub Release for $TAG already exists." >&2
    echo "       to add/replace its assets:  gh release upload $TAG $OUT_DIR/* --clobber" >&2
    echo "       otherwise bump the version and cut a new release — never reuse a tag." >&2
    exit 1
fi

# Reuse an existing local tag only if it already points at HEAD (a
# previous run that failed after tagging); otherwise refuse.
HEAD_SHA=$(git rev-parse HEAD)
TAG_EXISTS=0
if EXISTING=$(git rev-parse -q --verify "refs/tags/${TAG}^{commit}" 2>/dev/null); then
    if [ "$EXISTING" != "$HEAD_SHA" ]; then
        echo "error: tag $TAG already exists at ${EXISTING:0:9}, not HEAD (${HEAD_SHA:0:9})." >&2
        echo "       delete it (git tag -d $TAG) or check out the right commit." >&2
        exit 1
    fi
    TAG_EXISTS=1
fi

# ---- create the signed tag ----
if [ "$TAG_EXISTS" -eq 1 ]; then
    echo "==> reusing existing tag $TAG (already at HEAD)"
elif [ -n "$TAG_MSG" ]; then
    git tag -s "$TAG" -m "$TAG_MSG"
elif [ -n "$TAG_MSG_FILE" ]; then
    git tag -s "$TAG" -F "$TAG_MSG_FILE"
else
    git tag -s "$TAG"            # opens $EDITOR for the release notes
fi

# ---- summary + confirmation ----
echo
echo "  Repository  : $(git remote get-url origin)"
echo "  Branch      : $BRANCH"
echo "  Commit      : ${HEAD_SHA:0:9}  $(git log -1 --format='%s')"
echo "  Tag         : $TAG  (signed)"
echo "  Pre-release : $PRERELEASE_NOTE"
echo "  Signatures  : $SIG_NOTE"
echo "  Assets      : ${#ASSETS[@]} file(s)"
for a in "${ASSETS[@]}"; do echo "                  ${a##*/}"; done
echo
echo "  Release notes (tag message):"
git tag -l --format='%(contents)' "$TAG" | sed 's/^/      | /'
echo
echo "This pushes $BRANCH + $TAG to origin and creates the GitHub Release."

if [ "$ASSUME_YES" -ne 1 ]; then
    printf "Proceed? [y/N] "
    read -r REPLY || REPLY=""
    case "$REPLY" in
        y|Y|yes|YES) ;;
        *) echo "aborted."
           [ "$TAG_EXISTS" -eq 1 ] || \
               echo "note: local tag $TAG was created — remove it with: git tag -d $TAG"
           exit 1 ;;
    esac
fi

# ---- push + publish ----
echo "==> git push origin $BRANCH"
git push origin "$BRANCH"

echo "==> git push origin $TAG"
git push origin "$TAG"

echo "==> gh release create $TAG"
gh release create "$TAG" "${ASSETS[@]}" \
    --title "$TAG" \
    --notes-from-tag \
    --verify-tag \
    "${PRERELEASE_ARG[@]}"

echo "==> published $TAG to GitHub Releases"
