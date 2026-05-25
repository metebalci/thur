#!/usr/bin/env bash
#
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# tag-release.sh — sign and push the release tag.
#
# The thin operator-side step in the release flow. Builds nothing,
# signs no artifacts — it only:
#
#   1. Reads the version from the root Cargo.toml (same field
#      release.sh stamps onto artifact filenames). Tag is v<version>.
#   2. Creates a signed git tag on HEAD (operator's GPG identity).
#   3. Pushes the current branch and the tag to `origin`.
#
# Pushing the tag is what triggers .github/workflows/release.yml,
# which then builds the artifacts in the canonical Debian 11 builder
# container, runs the per-distro smoke matrix, signs the .deb / .rpm,
# and publishes the GitHub Release. Until smoke is green, no release
# exists at origin — recover from a failure by fixing the issue,
# deleting the tag (`git push origin :v<version>` + `git tag -d
# v<version>`), and re-tagging.
#
# Prerequisites: an `origin` remote, a clean working tree (the tag
# must map to a reproducible commit), and a git signing key
# (`git config user.signingkey`).

set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    cat <<'EOF'
tag-release.sh — sign and push the release tag.

Usage:
  release/tag-release.sh                tag message = v<version> (stub)
  release/tag-release.sh -m "summary"   inline tag message
  release/tag-release.sh -F notes.md    tag message from a file
  release/tag-release.sh -e             open $EDITOR for the tag message
  release/tag-release.sh -y             skip the confirmation prompt

The tag is v<version>, read from the root Cargo.toml. Pushing the
tag triggers .github/workflows/release.yml — that workflow builds,
smoke-tests across distros, signs, and publishes.
EOF
}

# ---- flags ----
TAG_MSG=""
TAG_MSG_FILE=""
EDIT_MSG=0
ASSUME_YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        -m)              [ $# -ge 2 ] || { echo "error: -m needs a value" >&2; exit 1; }
                         TAG_MSG="$2"; shift 2 ;;
        -F|--notes-file) [ $# -ge 2 ] || { echo "error: -F needs a value" >&2; exit 1; }
                         TAG_MSG_FILE="$2"; shift 2 ;;
        -e|--edit)       EDIT_MSG=1; shift ;;
        -y|--yes)        ASSUME_YES=1; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               echo "error: unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done
SOURCES=0
[ -n "$TAG_MSG" ]       && SOURCES=$((SOURCES + 1))
[ -n "$TAG_MSG_FILE" ]  && SOURCES=$((SOURCES + 1))
[ "$EDIT_MSG" -eq 1 ]   && SOURCES=$((SOURCES + 1))
if [ "$SOURCES" -gt 1 ]; then
    echo "error: pass at most one of -m / -F / -e." >&2
    exit 1
fi
if [ -n "$TAG_MSG_FILE" ] && [ ! -f "$TAG_MSG_FILE" ]; then
    echo "error: notes file not found: $TAG_MSG_FILE" >&2
    exit 1
fi

# ---- preconditions ----
git rev-parse --git-dir >/dev/null 2>&1 || {
    echo "error: not inside a git checkout." >&2; exit 1; }
git remote get-url origin >/dev/null 2>&1 || {
    echo "error: no 'origin' remote configured." >&2; exit 1; }

BRANCH=$(git branch --show-current)
[ -n "$BRANCH" ] || {
    echo "error: detached HEAD — check out the release branch first." >&2; exit 1; }

# Refuse a dirty tree: the tag must map to a reproducible commit, and
# the binaries CI builds embed the git SHA — anything uncommitted
# would diverge from what `cargo build` produces from the tagged
# commit on a fresh checkout.
if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty — commit or stash before tagging." >&2
    git status --short >&2
    exit 1
fi

# ---- version + tag name ----
VERSION=$(awk -F\" '/^version = / { print $2; exit }' Cargo.toml)
[ -n "$VERSION" ] || {
    echo "error: could not read version from Cargo.toml." >&2; exit 1; }
TAG="v${VERSION}"

# SemVer pre-release (any '-' suffix) -> GitHub pre-release; the
# release workflow turns this into --prerelease on the GH release.
case "$VERSION" in
    *-*) PRERELEASE_NOTE="yes" ;;
    *)   PRERELEASE_NOTE="no"  ;;
esac

HEAD_SHA=$(git rev-parse HEAD)

# ---- refuse to clobber an existing tag at a different commit ----
TAG_EXISTS=0
if EXISTING=$(git rev-parse -q --verify "refs/tags/${TAG}^{commit}" 2>/dev/null); then
    if [ "$EXISTING" != "$HEAD_SHA" ]; then
        echo "error: tag $TAG already exists at ${EXISTING:0:9}, not HEAD (${HEAD_SHA:0:9})." >&2
        echo "       delete it (git tag -d $TAG) or check out the right commit." >&2
        exit 1
    fi
    TAG_EXISTS=1
fi

# Refuse to retag once a remote tag already exists (CI probably
# published a release for it).
if git ls-remote --exit-code --tags origin "refs/tags/${TAG}" >/dev/null 2>&1; then
    echo "error: tag $TAG already exists at origin. Bump the version and cut a new release —" >&2
    echo "       never reuse a tag (operators pin against immutable refs)." >&2
    exit 1
fi

# ---- create the signed tag ----
if [ "$TAG_EXISTS" -eq 1 ]; then
    echo "==> reusing existing local tag $TAG (already at HEAD)"
elif [ -n "$TAG_MSG" ]; then
    git tag -s "$TAG" -m "$TAG_MSG"
elif [ -n "$TAG_MSG_FILE" ]; then
    git tag -s "$TAG" -F "$TAG_MSG_FILE"
elif [ "$EDIT_MSG" -eq 1 ]; then
    git tag -s "$TAG"            # opens $EDITOR
else
    git tag -s "$TAG" -m "$TAG"  # stub message; the tag name already says v<version>
fi

# ---- summary + confirmation ----
echo
echo "  Repository  : $(git remote get-url origin)"
echo "  Branch      : $BRANCH (HEAD ${HEAD_SHA:0:9})"
echo "  Commit      : ${HEAD_SHA:0:9}  $(git log -1 --format='%s')"
echo "  Tag         : $TAG  (signed)"
echo "  Pre-release : $PRERELEASE_NOTE"
echo
echo "  Tag message:"
git tag -l --format='%(contents)' "$TAG" | sed 's/^/      | /'
echo
echo "This pushes $BRANCH + $TAG to origin and triggers"
echo ".github/workflows/release.yml (build -> smoke -> sign -> publish)."

if [ "$ASSUME_YES" -ne 1 ]; then
    printf "Proceed? [y/N] "
    read -r REPLY || REPLY=""
    case "$REPLY" in
        y|Y|yes|YES) ;;
        *) echo "aborted."
           # If we minted the tag in this run, drop it — the operator
           # rejected the cut, so the local state should match.
           if [ "$TAG_EXISTS" -eq 0 ]; then
               git tag -d "$TAG" >/dev/null
               echo "       removed local tag $TAG."
           fi
           exit 1 ;;
    esac
fi

# ---- push ----
echo "==> git push origin $BRANCH"
git push origin "$BRANCH"

echo "==> git push origin $TAG"
git push origin "$TAG"

echo
echo "==> tag $TAG pushed. CI: $(git remote get-url origin | sed 's,\.git$,,; s,git@github.com:,https://github.com/,')/actions"
