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
  release/ghrelease.sh                 tag message = v<version> (stub);
                                       GitHub release body is empty
  release/ghrelease.sh -m "summary"    tag message inline (also the body)
  release/ghrelease.sh -F notes.md     tag message from a file (also the body)
  release/ghrelease.sh -e              open $EDITOR for the tag message
                                       (also the body)
  release/ghrelease.sh -y              skip the confirmation prompt

Default: the tag message is a bare stub (the tag already says
v<version>) and the GitHub Release body is empty. With stable and
prerelease tags interleaved there is no single "previous tag" worth
auto-comparing against — operators who want a commit list pick the
pair themselves (.../compare/<old-tag>...<new-tag>). Pass -m / -F / -e
to write real notes; that text becomes both the tag message and the
release body (via --notes-from-tag).

The tag is v<version> (from the root Cargo.toml); a -alpha/-beta/-rc
version is a pre-release.
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
[ -n "$TAG_MSG" ] && SOURCES=$((SOURCES + 1))
[ -n "$TAG_MSG_FILE" ] && SOURCES=$((SOURCES + 1))
[ "$EDIT_MSG" -eq 1 ] && SOURCES=$((SOURCES + 1))
if [ "$SOURCES" -gt 1 ]; then
    echo "error: pass at most one of -m / -F / -e." >&2
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

# Filter $OUT_DIR/ down to artifacts that match THIS version.
# release.sh wipes $OUT_DIR/ on every run, so a mismatch here means
# the Cargo.toml VERSION was bumped between release.sh and this run.
# Stale files get skipped, not uploaded.
#
# Filename translations release.sh inherits from the packagers:
#   .deb  cargo-deb maps `-` -> `~` in the version field, then stamps
#         <package>_<deb-version>-1_amd64.deb
#   .rpm  release.sh splits a SemVer prerelease across the Fedora
#         Version / Release fields (0.1.0-alpha.1 -> 0.1.0 / 0.alpha.1),
#         cargo-generate-rpm stamps <package>-<ver>-<rel>.x86_64.rpm
# .asc signatures track their parent — match against the name with
# `.asc` stripped.
# Backslash on the `~` is load-bearing: bash performs tilde expansion
# on the replacement of `${var//pat/repl}`, so a bare `~` would expand
# to $HOME and `0.1.0-dev.4` would turn into `0.1.0/home/yousilently.4`.
DEB_VERSION="${VERSION//-/\~}"
if [[ "$VERSION" == *-* ]]; then
    RPM_VERREL="${VERSION%%-*}-0.${VERSION#*-}"
else
    RPM_VERREL="${VERSION}-1"
fi

shopt -s nullglob
ALL=("$OUT_DIR"/*)
shopt -u nullglob

[ ${#ALL[@]} -gt 0 ] || {
    echo "error: $OUT_DIR/ is empty — run release/release.sh first." >&2; exit 1; }

ASSETS=()
ASC=()
SKIPPED=()
for f in "${ALL[@]}"; do
    name="${f##*/}"
    base="${name%.asc}"
    case "$base" in
        *_${DEB_VERSION}-1_amd64.deb|*-${RPM_VERREL}.x86_64.rpm)
            ASSETS+=("$f")
            [ "$base" != "$name" ] && ASC+=("$f")
            ;;
        *)
            SKIPPED+=("$name")
            ;;
    esac
done

[ ${#ASSETS[@]} -gt 0 ] || {
    echo "error: no artifacts in $OUT_DIR/ match version $VERSION." >&2
    echo "       re-run release/release.sh for a clean $OUT_DIR/." >&2
    exit 1; }

if [ ${#SKIPPED[@]} -gt 0 ]; then
    echo "==> skipping ${#SKIPPED[@]} stale file(s) in $OUT_DIR/ (do not match $VERSION):"
    for s in "${SKIPPED[@]}"; do echo "       $s"; done
fi

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
# Track whether the operator wrote a real tag message. If they did, the
# GitHub Release body uses --notes-from-tag so their text surfaces; if
# not, the tag message is a bare stub and the body is built from
# --generate-notes (commits since the last tag).
CUSTOM_MSG=0
if [ "$TAG_EXISTS" -eq 1 ]; then
    echo "==> reusing existing tag $TAG (already at HEAD)"
    # An existing tag's message is whatever it was when first created;
    # honor it as authoritative.
    CUSTOM_MSG=1
elif [ -n "$TAG_MSG" ]; then
    git tag -s "$TAG" -m "$TAG_MSG"
    CUSTOM_MSG=1
elif [ -n "$TAG_MSG_FILE" ]; then
    git tag -s "$TAG" -F "$TAG_MSG_FILE"
    CUSTOM_MSG=1
elif [ "$EDIT_MSG" -eq 1 ]; then
    git tag -s "$TAG"            # opens $EDITOR for the release notes
    CUSTOM_MSG=1
else
    # No override: the tag name already says v<version>, so the message
    # is a stub. The release body comes from --generate-notes below.
    git tag -s "$TAG" -m "$TAG"
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
           # If we minted the tag in this run, drop it — the operator
           # rejected the cut, so the local state should match.
           # TAG_EXISTS=1 means the tag pre-existed at HEAD (recovered
           # from a previous run that failed past tagging); leave it.
           if [ "$TAG_EXISTS" -eq 0 ]; then
               git tag -d "$TAG" >/dev/null
               echo "       removed local tag $TAG."
           fi
           exit 1 ;;
    esac
fi

# ---- push + publish ----
echo "==> git push origin $BRANCH"
git push origin "$BRANCH"

echo "==> git push origin $TAG"
git push origin "$TAG"

echo "==> gh release create $TAG"
if [ "$CUSTOM_MSG" -eq 1 ]; then
    # Operator wrote real notes via -m / -F / -e — surface them.
    NOTES_ARG=(--notes-from-tag)
else
    # Empty body. With stable + prerelease tags interleaved, there is
    # no canonical "previous tag" to auto-compare against (GitHub's
    # --generate-notes picks the chronologically previous tag, which
    # for a stable cut right after an -rc.N gives the wrong delta).
    # Operators wanting a changelog pick the pair themselves via
    # GitHub's compare UI. `--notes ""` is mandatory — omitting all
    # notes flags makes gh open an editor.
    NOTES_ARG=(--notes "")
fi
gh release create "$TAG" "${ASSETS[@]}" \
    --title "$TAG" \
    "${NOTES_ARG[@]}" \
    --verify-tag \
    "${PRERELEASE_ARG[@]}"

echo "==> published $TAG to GitHub Releases"
