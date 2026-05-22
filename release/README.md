# release/

Packaging and release tooling. `release.sh` builds the `.deb` /
`.rpm` / `.tar.gz` artifacts inside the pinned Debian builder image
(`Containerfile.builder`); `ghrelease.sh` tags and publishes them.
Also holds the systemd units, conffile / env starters, and the
`.deb` maintainer scripts under `thurvtl/` and `thurvsa/`.

Full release flow: [`docs/RELEASING.md`](../docs/RELEASING.md).
