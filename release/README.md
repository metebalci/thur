# release/

Packaging and release tooling. `release.sh` builds the `.deb` /
`.rpm` artifacts inside the pinned Debian builder image
(`Containerfile.builder`) — always unsigned, for local iteration
and as the build step of CI. `tag-release.sh` signs and pushes
the release tag; pushing the tag triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml),
which rebuilds in the same container, runs `smoke-install.sh`
across the supported distro matrix, signs the artifacts, and
publishes the GitHub Release. Also holds the systemd units,
conffile / env starters, and the `.deb` maintainer scripts under
`thurvtl/` and `thurvsa/`.

Full release flow: [`docs/RELEASING.md`](../docs/RELEASING.md).
