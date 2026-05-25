# Releasing thurvtl and thurvsa

Each release cut produces two `.deb`s and two `.rpm`s — one per product
(`thurvtl` tape VTL, `thurvsa` block target) — all built from a single
pinned-glibc container so the resulting binaries install on every
mainstream Linux distribution. Operators install whichever halves they
need. The two packages co-exist cleanly on the same host because they use
disjoint system users, data directories, unit names, and iSCSI ports.

Releases are cut **manually** — there is no release CI workflow. The
maintainer runs `release/release.sh` on a developer host.

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
`shared/cloud` and `shared/keystore` — so the binary carries no runtime
`libssl` dependency. This sidesteps the `libssl1.1` vs `libssl3` split
that would otherwise require separate binaries for older and newer distro
generations.

The Rust toolchain is pinned via `rust-toolchain.toml`
(`channel = "1.92.0"`) and the `RUST_VERSION` ARG in
`release/Containerfile.builder` — both must match; bump them in lockstep
whenever a transitive dependency raises its `rust-version` floor. Pinning
achieves close-to-byte-stable rebuilds; full reproducible builds are a
separate effort tracked in `ROADMAP.md`.

## Layout

```
release/                            # .deb / .rpm artifact sources
├── Containerfile.builder           # Debian 11 base + cargo-deb + cargo-generate-rpm
├── release.sh                      # Builds the image and runs both product cuts inside it
├── thurvtld.service          # systemd unit (ExecStart=/usr/bin/thurvtld)
├── thurvtl.yaml                    # thurvtl minimal starter conffile
├── thurvtl.env                     # thurvtl daemon env file (cloud creds + ${ENV_VAR} secrets + feature flags)
├── thurvtl/
│   └── postinst / prerm / postrm   # thurvtl .deb maintainer scripts
├── thurvsad.service          # systemd unit (ExecStart=/usr/bin/thurvsad)
├── thurvsa.yaml                    # thurvsa minimal starter conffile
├── thurvsa.env                     # thurvsa daemon env file (cloud creds + ${ENV_VAR} secrets + feature flags)
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

Single command, requires `podman`:

```bash
release/release.sh
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
   `cargo generate-rpm` once per product (`vtl-cli`, then `vsa-cli`),
   and — with `--sign` — detach-signs every artifact. Clippy / test
   failures abort the cut before any artifact is produced.

The first run takes roughly 10 minutes because rustup, cargo-deb, and
cargo-generate-rpm all install from scratch; subsequent runs add only a
few seconds of overhead.

The container's `target/` directory is an anonymous podman volume mounted
over the bind-mounted repo's `target/`. This isolation is necessary for
two reasons. First, cargo's fingerprint does not include the libc version,
so reusing a host `target/release/build/...` binary inside the Debian 11
builder would fail with `GLIBC_2.32 not found`. Second, the volume is
destroyed when the container exits (`--rm`), which guarantees that every
release is a **cold build** — the next run always starts from an empty
target directory. The cost is roughly 4 minutes of cargo compilation per
release.

Install podman if it is not already present: `sudo apt install podman` on
Debian/Ubuntu, and it is available by default on RHEL, Fedora, and SUSE.
Rootless mode is the default, so files written to `release-artifacts/`
will be owned by your user.

The script produces `release-artifacts/`:

```
thurvtl_<ver>-1_amd64.deb            # binary, Apache-2.0 (tape VTL)
thurvtl-<ver>-1.x86_64.rpm           # binary, Apache-2.0
thurvsa_<ver>-1_amd64.deb            # binary, Apache-2.0 (block target)
thurvsa-<ver>-1.x86_64.rpm           # binary, Apache-2.0
```

Plus `*.asc` detached signatures alongside each artifact, if signing is
enabled. The full source is published in the public repository; anyone who
wants to audit or rebuild simply clones it at the released tag.

Both binaries embed the build's git short-SHA in `--version`
(`thurvtl 0.1.0 (a42f57b)`); a `-dirty` suffix means the working
tree had uncommitted changes. **Don't release `-dirty` artifacts** —
start from a clean checkout, then build.

## Signing

Signing is opt-in via `--sign`, which requires `THUR_GPG_KEY_ID` set to
the fingerprint of an imported gpg key:

```bash
THUR_GPG_KEY_ID=<fingerprint> release/release.sh --sign
# adds thurvtl_<ver>-1_amd64.deb.asc and the matching .rpm.asc
```

`release.sh` aborts if `--sign` is passed without `THUR_GPG_KEY_ID`.
Only with `--sign` does it bind-mount the host's `~/.gnupg` into the
builder — an unsigned build has no path to the host's secret keys.

Omitting `--sign` produces unsigned artifacts and the script logs
`--sign not passed — skipping signatures`. Unsigned artifacts are
permitted **only** for `-dev` / `-alpha` / `-beta` versions. For a
release-candidate or final version, `release.sh` refuses to build without
signing. **Don't ship unsigned artifacts** beyond local QA.

The container launches with `podman run -it` so that gpg-agent's pinentry
has a TTY for the passphrase prompt. Run `release/release.sh` directly
from your terminal — don't pipe its stdout during a signed build, or the
prompt may be buffered out of view.

The current package signing key fingerprint:

```
E1FF A6E4 4D8A F56E BD17  997C 9B4E 436A E137 3A4B
```

Operators verify artifacts against this fingerprint; publish it
prominently in release notes, the README, and the project website. The
key signs the package artifacts and lives only on the maintainer's
release host.

When rotating the signing key: generate a new key, document the new
fingerprint in the next release notes, this file, and the README, and
leave the old public key on the keyserver so that historical signatures
continue to verify.

## Cutting a release

Order: **bump → commit → build → smoke → tag → push.** Building from a
clean, committed HEAD means `--version` reports the release commit's SHA
without `-dirty`.

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

# 3. Commit the bump. Tagging happens after smoke-test, so a build
#    or smoke failure can still be amended without retracting a tag.
git commit -am "release: v0.2.0"

# 4. Build + sign the artifacts. Clean HEAD, no -dirty in --version.
THUR_GPG_KEY_ID=<fingerprint> release/release.sh --sign

# 5. Smoke-test on a fresh VM (or at least `dpkg -i` / `rpm -i` on
#    a non-dev host). On failure, fix and amend the commit (no
#    published tag yet) before re-running release.sh.

# 6. Tag, push, and publish to GitHub Releases. ghrelease.sh creates
#    the signed tag v0.2.0 on the commit the artifacts were built
#    from — it extracts each .deb / .rpm, reads the short SHA every
#    binary embeds via `--version`, and tags that commit (not HEAD).
#    A follow-up commit between release.sh and ghrelease.sh (a docs
#    tweak, a CHANGELOG add) is fine: the tag stays pinned to the
#    build. All artifacts must agree on the SHA, none may be
#    `-dirty`, and the SHA must be an ancestor of HEAD so the branch
#    push publishes it. Tag message is a bare stub by default;
#    pushes branch + tag to origin, and uploads release-artifacts/*
#    (.deb / .rpm + .asc) as the release assets. The GitHub Release
#    body is left empty by default; pass -m "summary" / -F notes.md
#    to write real notes (also becomes the tag message), or -e to
#    compose in $EDITOR.
release/ghrelease.sh
```

To retract a release: delete the GitHub Release and its remote tag
(`gh release delete v0.2.0 --cleanup-tag --yes`), drop the local tag
(`git tag -d v0.2.0`), then bump the version forward and cut a new one.
Don't reuse a version number.

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

The target audience is expected to pin versions, validate in staging, and
deploy in change windows — not to `apt upgrade` storage software blindly.
Hosting a repository simply makes the install-and-verify ceremony
frictionless; it doesn't make the upgrade path automatic.

### How artifacts get there

After `release/ghrelease.sh` uploads the signed `release-artifacts/*` to
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

CVE notifications go on a public mailing list (TBD); see `ROADMAP.md`.
