# Installation

The complete install matrix. For the fastest single-machine loop, the
[`../QUICKSTART.md`](../QUICKSTART.md) is enough; this page covers every
path, signature verification, containers, and the post-install steps.

## Prerequisites

- **Linux.** Tested on Ubuntu 26.04; `.deb` / `.rpm` packages also cover
  Debian 12/13, Ubuntu 24.04, RHEL / Rocky / Alma 9/10, SLES 15 SP6+/16,
  and openSUSE Leap 15.6+.
- **Storage:** a cloud account (AWS S3, GCS, Azure Blob), an on-prem
  S3-compatible object store (MinIO, Ceph RGW, AIStore, …), or the
  `local` filesystem backend.

## Package repository (recommended)

One line on any supported distro wires up the apt / yum repository and
installs the signing key:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo bash
```

This uses the `stable` channel (tagged releases without a pre-release
suffix; includes pre-1.0 builds). For alpha / beta / rc tags, set the
channel:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo CHANNEL=unstable bash
```

Then install one or both applications:

```bash
sudo apt install thurvtl thurvsa        # Debian/Ubuntu
sudo dnf install thurvtl thurvsa        # RHEL/Rocky/Alma
sudo zypper install thurvtl thurvsa     # SLES/openSUSE
```

Verify the imported signing key against the current fingerprint:

```
E1FF A6E4 4D8A F56E BD17  997C 9B4E 436A E137 3A4B
```

## Direct package download

For air-gapped or offline staging, download the `.deb` / `.rpm` from
[GitHub Releases](https://github.com/metebalci/thur/releases). Each
release ships separate packages per application. One `.deb` covers Debian
12/13 + Ubuntu 24.04/26.04; one `.rpm` covers RHEL 9/10 + Rocky/Alma +
SLES 15 SP6+/16 + openSUSE Leap. Only the `.deb` on Ubuntu 26.04 is
regularly tested; other targets are best-effort.

Every release artifact ships a detached `.asc` GPG signature (except dev
and alpha builds) — verify it before installing. Fingerprint and the
build / signing process are in [`../dev/RELEASING.md`](../dev/RELEASING.md).

## What the packages do

- Install binaries to `/usr/bin/`.
- Drop a systemd unit (`thurvtld` / `thurvsad`).
- Lay down a minimal starter conffile
  (`/etc/thurvtl/thurvtl.yaml` / `/etc/thurvsa/thurvsa.yaml`).
- **Do not** auto-start the daemon — it needs configuration first.
- Never touch `/var/lib/{thurvtl,thurvsa}/` (operator data) on uninstall.

Both applications co-exist on one host with disjoint users, data dirs, conffiles,
unit names, and admin sockets — iSCSI/HTTP ports default to the same
number, so override one in YAML for co-residency.

## Post-install: group membership

Most CLI commands are **daemon-routed** — they reach the daemon through
its admin socket (`/run/<application>/admin.sock`, mode 0660, owned by the
application's system user). To run them as an ordinary user, join that
application's group (log out and back in to apply):

```bash
sudo usermod -aG thurvtl $USER
sudo usermod -aG thurvsa $USER     # on a co-resident host
```

## Run

```bash
sudo systemctl enable --now thurvtld       # or thurvsad
sudo systemctl status thurvtld
sudo journalctl -u thurvtld -f
```

Each daemon runs a storage target plus an HTTP metrics server (port 9090)
in one process. Persist systemd customizations through
`sudo systemctl edit <unit>` so package upgrades don't clobber them.
Configuration is in [`CONFIGURATION.md`](CONFIGURATION.md).

## Containers

Tagged releases publish multi-arch (amd64 + arm64) images per application,
each carrying both the daemon and the CLI:

```
ghcr.io/metebalci/thurvtl
ghcr.io/metebalci/thurvsa
```

Because the CLI is daemon-routed, you operate a running container with
`exec`:

```bash
cat > thurvsa.yaml <<'EOF'
data_dir: /var/lib/thurvsa
storage:
  backends:
    local:
      type: local
      root_dir: /var/lib/thurvsa/storage-local
EOF

podman run -d --name thurvsa --network host \
    -v ./thurvsa.yaml:/etc/thurvsa/thurvsa.yaml:ro \
    -v thurvsa-data:/var/lib/thurvsa \
    ghcr.io/metebalci/thurvsa:latest

podman exec thurvsa thurvsa volume create myvol --size 100G
```

Ready-to-edit Compose files are at
[`../../release/compose-vsa.yaml`](../../release/compose-vsa.yaml) and
[`../../release/compose-vtl.yaml`](../../release/compose-vtl.yaml) (each
carries a matching minimal config; VTL additionally needs the required
`library:` block and serves iSCSI only). Print the annotated config
without a daemon via `podman run --rm ghcr.io/metebalci/thurvsa config
defaults`.

Container notes:

- **Networking.** With `--network host` (or macvlan) the daemon binds
  3260 (iSCSI), 9090 (HTTP), and — for VSA with `nvmetcp` — 4420 + 8009
  directly on the host and discovery just works. On a Docker **bridge**
  with `-p` published ports, set the advertised address so initiators get
  a reachable IP (see [`CONNECTING.md`](CONNECTING.md) § Advertised
  address).
- **UID/GID 9000.** The daemon runs as `thurvtl` / `thurvsa` (uid/gid
  9000) inside the image. A *named* volume inherits that ownership; a
  host **bind**-mounted data dir must be `chown 9000:9000`, and the
  mounted config readable by uid 9000.
- **Apple Silicon / Mac:** run the arm64 image in a Linux VM
  (OrbStack / Lima / UTM / Docker Desktop) — there is no native macOS
  port. See [issue #83](https://github.com/metebalci/thur/issues/83).
