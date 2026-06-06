# Releasing thurvtl and thurvsa

Each release cut produces two `.deb`s and two `.rpm`s — one per product
(`thurvtl` virtual tape library, `thurvsa` virtual storage appliance) — all built from a single
pinned-glibc container so the resulting binaries install on every
mainstream Linux distribution. Operators install whichever halves they
need. The two packages co-exist cleanly on the same host because they use
disjoint system users, data directories, unit names, and iSCSI ports.

Releases are cut by a **tag-triggered CI workflow**
([`.github/workflows/release.yml`](../.github/workflows/release.yml)).
The maintainer signs and pushes a `v<version>` tag with
`release/tag-release.sh`; CI then builds the artifacts in the
canonical Debian 11 builder container, runs the per-distro smoke
matrix, signs the `.deb` / `.rpm`, and publishes the GitHub Release.
The local `release/release.sh` produces unsigned artifacts for dev
iteration and is what the build step in CI invokes under the hood.

## Licensing

Source and binaries ship under the **Apache License, Version 2.0**
(`LICENSE` at the repo root):

- `.deb` — `[package.metadata.deb] license-file` points at `../LICENSE`;
  cargo-deb writes it to `/usr/share/doc/<package>/copyright`.
- `.rpm` — `[package.metadata.generate-rpm] license = "Apache-2.0"`;
  LICENSE shipped at `/usr/share/licenses/<package>/LICENSE`.

## Supported distros

| Distro | glibc | Package |
| --- | --- | --- |
| Debian 12 (Bookworm) | 2.36 | `.deb` |
| Debian 13 (Trixie) | 2.41 | `.deb` |
| Ubuntu 24.04 LTS | 2.39 | `.deb` |
| Ubuntu 26.04 LTS | 2.41 | `.deb` |
| RHEL 9 / Rocky 9 / Alma 9 | 2.34 | `.rpm` |
| RHEL 10 / Rocky 10 / Alma 10 | 2.39 | `.rpm` |
| SLES 15 SP6 / openSUSE Leap 15.6 | 2.38 | `.rpm` |
| SLES 16 / openSUSE Leap 16 | 2.39+ | `.rpm` |

All packages build inside `debian:11` (glibc 2.31), which sits below the
glibc floor of every distro in the table above. Forward glibc compatibility
guarantees that the single resulting binary will run on all of them without
per-distro builds.

OpenSSL is vendored — statically linked via `features = ["vendored"]` in
`shared/object-store` and `shared/keystore` — so the binary carries no runtime
`libssl` dependency. This sidesteps the `libssl1.1` vs `libssl3` split
that would otherwise require separate binaries for older and newer distro
generations.

The Rust toolchain is pinned via `rust-toolchain.toml`
(`channel = "1.92.0"`) and the `RUST_VERSION` ARG in
`release/Containerfile.builder` — both must match; bump them in lockstep
whenever a transitive dependency raises its `rust-version` floor. Pinning
achieves close-to-byte-stable rebuilds; full reproducible builds are a
separate effort tracked in the issue tracker.

## Layout

```
release/                            # .deb / .rpm artifact sources
├── Containerfile.builder           # Debian 11 base + cargo-deb + cargo-generate-rpm
├── release.sh                      # Builds .deb/.rpm inside the container (unsigned; local + CI build step)
├── smoke-install.sh                # Per-distro install + start + one-verb smoke harness (invoked by release.yml)
├── tag-release.sh                  # Operator step: signs + pushes the release tag (triggers release.yml)
├── thurvtld.service          # systemd unit (ExecStart=/usr/bin/thurvtld)
├── thurvtl.yaml                    # thurvtl minimal starter conffile
├── thurvtl.env                     # thurvtl daemon env file (storage credentials + ${ENV_VAR} secrets + feature flags)
├── thurvtl/
│   └── postinst / prerm / postrm   # thurvtl .deb maintainer scripts
├── thurvsad.service          # systemd unit (ExecStart=/usr/bin/thurvsad)
├── thurvsa.yaml                    # thurvsa minimal starter conffile
├── thurvsa.env                     # thurvsa daemon env file (storage credentials + ${ENV_VAR} secrets + feature flags)
└── thurvsa/
    └── postinst / prerm / postrm   # thurvsa .deb maintainer scripts (separate dir keeps cargo-deb's auto-discovery from confusing the two products)
(RPM scriptlets are inlined in vtl/cli/Cargo.toml and vsa/cli/Cargo.toml respectively)
```

`Containerfile.builder` is build-time only — it produces the artifacts.
The product ships exclusively as host packages managed by systemd; there
are no runtime container images or compose recipes.

The `thurvtl` `.deb` and `.rpm` install:

```
/usr/bin/thurvtld
/usr/bin/thurvtl
/usr/lib/systemd/system/thurvtld.service
/etc/thurvtl/thurvtl.yaml                        # conffile — never overwritten
/etc/thurvtl/thurvtl.env                        # conffile — never overwritten
/usr/share/doc/thurvtl/thurvtl.defaults.yaml     # reference, refreshed on upgrade
/usr/share/doc/thurvtl/README.md
/usr/share/bash-completion/completions/thurvtl
/usr/share/zsh/site-functions/_thurvtl
/var/lib/thurvtl/                               # owned by thurvtl:thurvtl
```

The `thurvsa` `.deb` and `.rpm` install:

```
/usr/bin/thurvsad
/usr/bin/thurvsa
/usr/lib/systemd/system/thurvsad.service
/etc/thurvsa/thurvsa.yaml                        # conffile — never overwritten
/etc/thurvsa/thurvsa.env                        # conffile — never overwritten
/usr/share/doc/thurvsa/thurvsa.defaults.yaml     # reference, refreshed on upgrade
/usr/share/doc/thurvsa/README.md
/usr/share/bash-completion/completions/thurvsa
/usr/share/zsh/site-functions/_thurvsa
/var/lib/thurvsa/                               # owned by thurvsa:thurvsa
```

The `.deb` places the license at `/usr/share/doc/<package>/copyright` per
Debian convention; the `.rpm` places it at
`/usr/share/licenses/<package>/LICENSE` per RPM convention.

## Daemon is not auto-enabled

The postinst deliberately does NOT enable or start the daemon. Both
products require an edited conffile before the daemon can start usefully.
Auto-enabling on install would simply produce failed-unit logs on every
fresh installation.

- `thurvtl`: edit `/etc/thurvtl/thurvtl.yaml` — `data_dir` plus the
  required `library:` block (`num_slots` / `num_drives` /
  `lto_generation`) — then `systemctl enable --now thurvtld`. The
  daemon materializes `library.json` / `inventory.json` on first
  start from the YAML.
- `thurvsa`: edit `/etc/thurvsa/thurvsa.yaml`, then
  `systemctl enable --now thurvsad`. Volumes are created at
  runtime via `thurvsa volume create` (admin-socket-routed).

## Building a release

The canonical release build is `.github/workflows/release.yml`,
triggered by a `v<version>` tag push. It runs three jobs:

1. **build** — checks out the tag, runs `release/release.sh --no-cache`
   inside the pinned Debian 11 builder container (cold build),
   uploads `release-artifacts/` as a workflow artifact.
2. **smoke** — fans out across the supported distros (matrix:
   `debian:12`, `debian:13`, `ubuntu:24.04`, `ubuntu:26.04`,
   `rockylinux:9`, `rockylinux:10`, `fedora:latest`,
   `opensuse/leap:15.6` × `vtl` / `vsa`). Each cell installs the
   package as root in a stock distro container, writes a minimal
   conffile (`release/smoke-install.sh` is the harness), execs the
   daemon directly (no systemctl in containers), waits for the
   admin socket, and runs one verb (`cartridge create` /
   `volume create`). Any cell failing fails the release.
3. **publish** — imports the GPG signing key from the
   `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` repo secrets, detach-signs
   every `.deb` / `.rpm`, creates the GitHub Release, uploads
   artifacts + `.asc` signatures. A SemVer pre-release tag
   (`-alpha` / `-beta` / `-rc`) publishes as a GitHub pre-release.

The local `release/release.sh` produces the same unsigned artifacts
for dev iteration. Single command, requires `podman`:

```bash
release/release.sh                 # default: cache ON, clean tree required
release/release.sh --no-cache      # cold build (what CI does)
release/release.sh --allow-dirty   # cut from a dirty working tree (local only)
```

The script runs in two halves, divided by the `THUR_IN_BUILDER`
sentinel:

1. **On the host:** builds (or rebuilds, with layer cache) the
   `thur-builder:latest` image from `release/Containerfile.builder`,
   then re-execs itself inside the container with `THUR_IN_BUILDER=1`.
2. **Inside the container:** runs `cargo clippy --workspace --release
   --all-targets -- -D warnings` and `cargo test --workspace --release`
   as quality gates, then `cargo build --release --workspace` (both
   daemons + both CLIs in one pass). Then `cargo deb` /
   `cargo generate-rpm` once per product (`vtl-cli`, then `vsa-cli`).
   Clippy / test failures abort the cut before any artifact is produced.

The first run takes roughly 10 minutes because rustup, cargo-deb, and
cargo-generate-rpm all install from scratch; subsequent runs add only a
few seconds of overhead.

The container's `target/` directory is a podman volume mounted over the
bind-mounted repo's `target/`. This isolation is necessary because
cargo's fingerprint does not include the libc version, so reusing a host
`target/release/build/...` binary inside the Debian 11 builder would fail
with `GLIBC_2.32 not found`. With `--no-cache` the volume is anonymous
and destroyed on container exit, guaranteeing a cold build that matches
a fresh checkout; without it the volume is the named `thur-builder-target`
and the cargo target dir persists across runs (the dev default — cuts
iterative builds from minutes to seconds).

Install podman if it is not already present: `sudo apt install podman` on
Debian/Ubuntu, and it is available by default on RHEL, Fedora, and SUSE.
Rootless mode is the default, so files written to `release-artifacts/`
will be owned by your user.

The script produces `release-artifacts/`:

```
thurvtl_<ver>-1_amd64.deb            # binary, Apache-2.0 (virtual tape library)
thurvtl-<ver>-1.x86_64.rpm           # binary, Apache-2.0
thurvsa_<ver>-1_amd64.deb            # binary, Apache-2.0 (virtual storage appliance)
thurvsa-<ver>-1.x86_64.rpm           # binary, Apache-2.0
```

`release.sh` itself produces unsigned artifacts only; `.asc`
signatures are added by the publish step in CI. The full source is
published in the public repository; anyone who wants to audit or
rebuild simply clones it at the released tag.

Both binaries embed the build's git short-SHA in `--version`
(`thurvtl 0.1.0 (a42f57b)`); a `-dirty` suffix means the working
tree had uncommitted changes. The clean-tree check in `release.sh`
prevents `-dirty` cuts unless `--allow-dirty` is passed explicitly,
and `tag-release.sh` refuses to tag a dirty tree.

## Signing

Signing happens in CI's publish job, using the GPG key stored in two
repository secrets:

- `GPG_PRIVATE_KEY` — the ASCII-armored private key (`gpg --armor
  --export-secret-key <fingerprint>` output).
- `GPG_PASSPHRASE` — the passphrase that unlocks it.

The fingerprint is public (it's printed below and on the package
repository site) and not stored as a secret — the workflow derives it
from whatever key is imported. `release.sh` no longer takes a `--sign`
flag and no longer mounts the host's `~/.gnupg` — local builds are
unsigned by construction. To sign artifacts ad-hoc outside CI, run
`gpg --detach-sign --armor` against the files in `release-artifacts/`
directly.

The current package signing key fingerprint:

```
E1FF A6E4 4D8A F56E BD17  997C 9B4E 436A E137 3A4B
```

Operators verify artifacts against this fingerprint; publish it
prominently in release notes, the README, and the project website.

When rotating the signing key: generate a new key, replace the
`GPG_PRIVATE_KEY` + `GPG_PASSPHRASE` repo secrets, document the new
fingerprint in the next release notes, this file, and the README, and
leave the old public key on the keyserver so historical signatures
continue to verify.

## Cutting a release

Order: **bump → commit → tag-push → CI does the rest.** CI is the
only path that produces signed, smoke-tested, published artifacts.

```bash
# 1. Bump the version. It lives in ONE place — [workspace.package]
#    version in the root Cargo.toml; every crate inherits it via
#    `version.workspace = true`.
cargo install cargo-edit                            # one-time
cargo set-version --workspace 0.2.0
# Or hand-edit that one line. Run `cargo build` afterwards to
# refresh Cargo.lock.

# 2. Sanity-check the bump — release.sh reads this to stamp filenames.
awk -F\" '/^version = / { print $2; exit }' Cargo.toml

# 3. Commit the bump.
git commit -am "release: v0.2.0"

# 4. (Optional) Build locally to catch obvious breakage before CI does.
#    Same container, same toolchain — just unsigned and cached:
release/release.sh

# 5. Tag, sign, push. Pushing the tag triggers release.yml, which
#    builds in the canonical container, runs the per-distro smoke
#    matrix, signs the .deb / .rpm, and publishes the GitHub Release.
#    Default tag message is a bare stub; pass -m "summary" / -F
#    notes.md to write real notes, or -e to compose in $EDITOR.
release/tag-release.sh
```

The same tag push also triggers `container.yml`, which builds the
per-product multi-arch (amd64 + arm64) images and publishes them to
`ghcr.io/<owner>/thurvtl` and `.../thurvsa` (a `v*-*` pre-release tag
publishes by version only — no `latest` / `major.minor` float). It runs
independently of the `.deb` / `.rpm` pipeline: a container-build failure
doesn't block the GitHub Release, and vice versa.

If smoke fails in CI no release is published — the tag exists at
origin but there is no GitHub Release for it. Recover by deleting
the remote and local tag (`git push origin :v0.2.0 && git tag -d
v0.2.0`), fixing the issue, and re-running `tag-release.sh` from a
new HEAD. Don't re-use the tag from the broken commit.

To retract a published release: delete the GitHub Release and its
remote tag (`gh release delete v0.2.0 --cleanup-tag --yes`), drop
the local tag (`git tag -d v0.2.0`), then bump the version forward
and cut a new one. Don't reuse a version number. Deleting the
Release also fires the `notify-unpublish.yml` bridge (see §
Package repository) and auto-removes the matching `.deb` / `.rpm`
from the apt + yum channel — provided the bridge workflow file
was already present on the release's tag commit. For releases cut
before the bridge existed, run `unpublish.yml` over in
`thur.metebalci.com` manually to clean the channel.

## CSI driver

The Kubernetes CSI driver (`csi/`, issue #15) releases on its **own tag
namespace, `csi-v*`** — separate from the daemon's `v*` SemVer above. The
two are versioned and shipped independently: a daemon release does not
imply a driver release, and vice-versa. The repo therefore carries two
tag families, and the workflows are path/trigger-scoped so they never
collide:

| Tag | Triggers | Publishes |
| --- | --- | --- |
| `v<version>` | `release.yml`, `container.yml` | `.deb`/`.rpm` + `ghcr.io/<owner>/{thurvtl,thurvsa}` images |
| `csi-v<version>` | `csi-release.yml` | `ghcr.io/<owner>/thurvsa-csi` image + `ghcr.io/<owner>/charts/thurvsa-csi` OCI Helm chart |

A `csi-v*-*` tag (e.g. `csi-v0.1.0-rc.0`) is a pre-release: it publishes
the exact version only, without floating `latest`. The version is the tag
minus the `csi-v` prefix and is stamped into the binary (`-ldflags -X
main.version`), the image tag, and the Helm chart + app version. The
per-PR gate is `csi.yml` (path-scoped to `csi/**`). Driver design and the
deploy chart are in [CSI.md](CSI.md).

## Package repository

Operators install Thur via the **https://thur.metebalci.com** apt and yum
repositories — one install line, signed by the key whose fingerprint
appears in § Signing above:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo bash
```

The site source and the publishing workflow live in the separate
`metebalci/thur.metebalci.com` repository; that repo's `README.md` is the
operational source of truth for URL layout, Cloudflare wiring, secrets
inventory, and the rotation procedure for the signing key. Two channels
are published in parallel:

- **stable** — tagged releases without a pre-release suffix (`vN.M.P`).
  Includes pre-1.0 (0.x) releases; the channel guarantees build and
  signing hygiene, not API stability. Operators on 0.x should pin to
  specific minor versions if they can't tolerate the breaks SemVer
  reserves the right to introduce before 1.0.0.
- **unstable** — pre-release tagged versions
  (`vN.M.P-alpha.X` / `-beta.X` / `-rc.X`) for testing forthcoming
  releases, plus any future per-commit dev builds.

Both channels are auto-published. A small `notify-publish.yml` bridge
in this repo listens for the `release.published` event, inspects the
tag string, and fires `repository_dispatch` at `thur.metebalci.com`
with the routed channel — `vN.M.P` lands in `stable`,
`vN.M.P-anything` lands in `unstable`. Both channels accumulate, so
operators can pin against a specific tag rather than always taking the
latest.

Recalls run the same bridge in reverse. A companion
`notify-unpublish.yml` workflow listens for `release.deleted` events
here and fires `repository_dispatch` at `thur.metebalci.com`'s
`unpublish.yml`, which scrubs the matching `.deb` / `.rpm` from the
channel and re-signs the indices. The bridge intentionally ignores
bare tag deletions — a stray `git push --delete` shouldn't be able to
yank a published version. GitHub runs the unpublish workflow against
the release tag's commit (not `main` HEAD), so releases cut before
this bridge existed need a manual `workflow_dispatch` over in
`thur.metebalci.com` to clean the channel.

The target audience is expected to pin versions, validate in staging, and
deploy in change windows — not to `apt upgrade` storage software blindly.
Hosting a repository simply makes the install-and-verify ceremony
frictionless; it doesn't make the upgrade path automatic.

### How artifacts get there

After `release.yml` publishes the signed `release-artifacts/*` to
the GitHub Release, a `publish.yml` workflow in
`metebalci/thur.metebalci.com` downloads them, regenerates the apt suite
indices and the rpm `repomd.xml`, signs both with the same package
signing key documented above, and syncs the resulting tree to the R2
bucket backing `pkg.thur.metebalci.com`. Cross-repo publishing is
deliberate — it keeps the source repo free of CDN credentials and the
publish-side CI free of release-cutting concerns.

The supply-chain trust model rests on the signing key, not on TLS or
the CDN: an attacker who compromises the R2 bucket can swap binaries
but cannot forge a new signed `Release` / `repomd.xml` without the key,
and apt / yum refuse to install artifacts that don't match the
operator's trusted fingerprint. Publish the fingerprint prominently —
release notes, this file, and the project website — so operators have
an authoritative reference to verify against.

CVE notifications go on a public mailing list (TBD).
