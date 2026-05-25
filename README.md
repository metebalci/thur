# Thur

> **Status: alpha — under active development.** Thur has not had a
> stable release. On-disk formats, configuration keys, and the
> SCSI / NVMe surface may change without notice or migration path.
> Not recommended for production use.

**Thur VTL** and **Thur VSA** are two sibling products presenting
spec-conformant storage devices to the host — a virtual tape library
and a virtual storage appliance. Both are **thin-provisioned**
(addressable capacity is declared, not reserved; only written data
consumes any storage), and both run a **small local cache** in front
of a durable backend: an object store (AWS S3, GCS, Azure Blob,
AIStore, MinIO, Ceph RGW, Wasabi, …) or a local filesystem holds the
source of truth, while a refcount-evicted on-host disk cache —
typically far smaller than the addressable capacity — fronts the
working set.

Stored data is **deduplicated** in a shared content-addressed chunk
pool. Every chunk is keyed by its BLAKE3 hash, with no central index
or database, so identical bytes from any source — across every volume
and cartridge on a backend — are stored exactly once.

- **Thur VTL** — a Virtual Tape Library that presents a spec-conformant
  SMC-3 medium changer and SSC-4 LTO-8 drive surface over iSCSI. From
  the backup software's perspective it is an ordinary tape library — no
  proprietary agent required.
- **Thur VSA** — a Virtual Storage Appliance that presents any number
  of spec-conformant SBC-3 direct-access LUNs over iSCSI or NVMe/TCP.
  From the host's perspective each LUN is an ordinary block device —
  VMware, Hyper-V, Linux, no proprietary agent required.

Both products can live co-resident on a single host; they use disjoint
system users, data directories, conffiles, systemd unit names, and admin
sockets. The configured storage backend (S3, GCS, Azure, or another
S3-compatible object store) is, in all cases, the durable source of
truth.

The two share a common codebase — chunk pool, storage backends, iSCSI
transport, audit log, telemetry, alerting are all the same code — and
diverge only in the SCSI command surfaces they expose (SMC-3 + SSC-4 vs
SBC-3) and the on-host data shapes layered above (cartridges vs
volumes). Hence the single repository, **Thur**.

# Disclaimer

**Thur VTL and Thur VSA are experimental backup and storage software.** It is provided
**"as is"**, **without any warranty** of any kind. There is **no
guarantee of data integrity, reliability, or fitness for any
purpose**.

You are solely responsible for verifying your backups and maintaining
independent copies of critical data. The authors and contributors
accept **no responsibility or liability** for data loss, corruption,
or any damages resulting from the use of this software. Use at your
own risk.

# Features

The following capabilities are shared across both products:

- **Content-addressed dedup** — BLAKE3-hashed chunks stored once per
  backend pool; cross-volume / cross-cartridge.
- **Object-store-backed storage** — S3-compatible (AWS S3, AIStore,
  MinIO, Ceph RGW, Wasabi, …), Google Cloud Storage, Azure Blob, or
  a local filesystem. Disk is a warm cache with a per-backend budget
  and write backpressure when the budget is hit.
- **Compression on backend uploads** (zstd / lz4, post-dedup); parallel
  backend up/downloads.
- **CHAP authentication** for iSCSI; **TLS-PSK** for NVMe/TCP (VSA).
- **Append-only, BLAKE3-chained audit log**; Prometheus metrics + OTLP.
- **Optional at-rest encryption** under a pluggable DEK keystore.

Thur VTL additionally provides:

- Spec-conformant **SMC-3 medium changer + SSC-4 LTO-8 drives** over
  iSCSI; configurable topology (caps 65535 slots / 65535 mail slots /
  255 drives).
- Virtual cartridges with full sequential-access semantics, WORM,
  backend-native legal hold, the LTFS two-partition layout, and LTO
  Application-Managed Encryption.
- Cross-region disaster recovery; cartridge migration and archive
  between storage backends.
- LTO-style per-block drive compression (lz4 / zstd) ahead of the
  storage-tier compression — off by default, matching real-drive convention.

Thur VSA additionally provides:

- **SBC-3 over iSCSI**, or the **NVM Command Set over NVMe/TCP**.
- Thin-provisioned per-volume LUNs, 4 KiB sectors, sparse page table,
  write-back page cache.
- VAAI / NVMe data-path primitives and persistent reservations.

# Getting Started

## Prerequisites

- Linux. Tested on Ubuntu 26.04; `.deb` / `.rpm` packages also cover
  Debian 12/13, Ubuntu 24.04, RHEL / Rocky / Alma 9/10, SLES 15 SP6+/16,
  and openSUSE Leap 15.6+.
- Building from source: Rust 1.92+ (2024 edition), plus a C toolchain
  (gcc or clang, pkg-config, OpenSSL headers) — OpenSSL is vendored and
  compiled from source so the release binary carries no runtime
  `libssl` dependency.
- Integration tests: `open-iscsi`, `sg3-utils`, `mtx`, `mt-st`,
  `libiscsi-bin`, `lsscsi`.
- Storage: a cloud account (AWS S3, GCS, Azure Blob), an on-prem
  S3-compatible object store (MinIO, Ceph RGW, AIStore, …), or the
  `local` filesystem backend.

## Install

The shortest path is the **https://thur.metebalci.com** apt / yum
repository — one line on any supported distro:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo bash
```

This wires up the `stable` channel (tagged releases without a
pre-release suffix; includes pre-1.0 builds). Use the pre-release
channel that holds alpha / beta / rc tags by setting `CHANNEL` in the
same line:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo CHANNEL=unstable bash
```

The script writes the right `sources.list.d` or `yum.repos.d` entry
and installs the signing public key alongside it. Verify the imported
key against the current package signing fingerprint:

```
E1FF A6E4 4D8A F56E BD17  997C 9B4E 436A E137 3A4B
```

For air-gapped installs, offline staging, or anyone who'd rather not
delegate the install ceremony to a piped shell script, the packages
can also be downloaded directly from
[GitHub Releases](https://github.com/metebalci/thur/releases). Each
tagged release ships separate `.deb` / `.rpm` packages per product —
`thurvtl` and `thurvsa`. One `.deb` covers Debian 12/13 + Ubuntu
24.04/26.04; one `.rpm` covers RHEL 9/10 + Rocky/Alma + SLES 15 SP6+/16
+ openSUSE Leap. Only the `.deb` on Ubuntu 26.04 is regularly tested;
other targets are best-effort. Products co-exist on one host with
disjoint users, data dirs, conffiles, unit names, and admin sockets.

Packages install to `/usr/bin/`, drop a systemd unit, and lay down a
minimal starter conffile (`/etc/thurvtl/thurvtl.yaml` /
`/etc/thurvsa/thurvsa.yaml`). They **do not** auto-start the daemon —
it needs configuration first. `/var/lib/{thurvtl,thurvsa}/` (operator
data) is never touched on uninstall.

Most CLI commands are **daemon-routed** — they reach the running
daemon through its admin socket (`/run/thurvtl/admin.sock` /
`/run/thurvsa/admin.sock`, mode `0660`, owned by the product's system
user). To run those as an ordinary user, add yourself to that
product's group — each product has its own:

```bash
sudo usermod -aG thurvtl $USER     # log out and back in to apply
sudo usermod -aG thurvsa $USER     # ...and thurvsa, on a co-resident host
```

Every release artifact ships a detached `.asc` GPG signature (except
dev and alpha builds) — verify it before installing. Key fingerprint
and the build / signing process are in
[`docs/RELEASING.md`](docs/RELEASING.md).

Other install paths:

- **From source** — `cargo build --release`; binaries land in
  `target/release/`. Not recommended for production.

## Configure

The absolute minimum each conffile needs is three things: `data_dir`
(the packaged starter pre-fills `/var/lib/thurvtl` and
`/var/lib/thurvsa` — change only if you want the data elsewhere), at
least one entry under `storage.backends:`, and — for Thur VTL only — a
`library:` chassis declaration (see below). The `config defaults`
command prints the full annotated reference — every key documented
with its default value and a description — which you can redirect
straight to a starter file:

```bash
thurvtl config defaults > thurvtl.yaml
thurvsa config defaults > thurvsa.yaml
```

Every config file and YAML key is catalogued in
[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md). Both the daemon and
CLI resolve `--config PATH` first, otherwise
`/etc/<product>/<product>.yaml`.

### Storage backends

Both products store data as content-addressed chunks in a per-backend
pool. The backend type determines where those chunks live; configure
backends under `storage.backends:` in the conffile, giving each entry a
name, a `type` (`s3` / `gcs` / `azure` / `local`), and its
per-provider knobs:

```yaml
storage:
  backends:
    primary:
      type: s3
      bucket: thur-data
      prefix: "data/"
      region: us-east-1
    archive:
      type: gcs
      bucket: thur-cold
      prefix: "data/"
      project_id: my-project
```

The `local` backend is filesystem-only — no credentials, no network —
ideal for testing:

```yaml
storage:
  backends:
    primary:
      type: local
      root_dir: "./.thur/local-backend"
```

On startup the daemon validates each storage backend's credentials,
bucket existence, and read/write/delete permissions, and refuses to
start on failure. Validate ahead of time, without starting the
daemon, with `thurvtl system storage check`.

- **Credentials** — `auth:` blocks, default chains, the daemon env
  file, multi-provider layouts: [`docs/AUTH.md`](docs/AUTH.md).
- **S3-compatible provider matrix** — Backblaze B2, Wasabi, Hetzner,
  OVHcloud, …: [`docs/S3_BACKENDS.md`](docs/S3_BACKENDS.md).
- **WORM, legal hold, at-rest encryption** (incl. provider bucket
  setup): [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md).
- **Cross-region DR, cartridge migration / archive** —
  [`docs/SPEC.md`](docs/SPEC.md).

### Thur VTL library

Thur VTL also needs a chassis declaration. Add a `library:` block to
your `thurvtl.yaml` — every field is required:

```yaml
library:
  num_slots: 40       # storage slots; raise/lower by editing this and restarting
  num_drives: 3       # tape drives
  lto_generation: 8   # 7 or 8 (LTO-8 only is supported today)
```

The daemon materializes `<data_dir>/library/library.json` from this
block on first start (minting a stable chassis serial + SMC element
bases). On subsequent starts it diffs the YAML against the persisted
declaration and reconciles: grow operations always succeed; shrink
operations refuse if any cartridge or loaded drive would be orphaned.
`thurvtl library bounds` (against a running daemon) shows the
safe-shrink envelope for the current inventory. No imperative chassis
mutation — edit the YAML and restart the daemon.

Add cartridges via `thurvtl cartridge create` once the daemon is up.

## Run

```bash
sudo systemctl enable --now thurvtld       # or thurvsad
sudo systemctl status thurvtld
sudo journalctl -u thurvtld -f
```

Each daemon runs a storage target and an HTTP metrics server
(port 9090) in a single process. Thur VTL serves iSCSI on port 3260;
Thur VSA serves iSCSI on port 3260 by default, or NVMe/TCP on port
4420 if `transport: nvmetcp` is set in `thurvsa.yaml` (one listener
binds, not both). For co-resident installs, override the shared ports
in YAML so the two daemons don't clash. Persist systemd
customizations through `sudo systemctl edit <unit>` so package
upgrades don't clobber them.

# Using Thur VTL

## Connect (iSCSI)

Once the daemon is running, connect from any iSCSI initiator in the
usual way. The library and drive LUNs appear as standard kernel devices:

```bash
sudo iscsiadm -m discovery -t sendtargets -p <target_ip>:3260
sudo iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvtl \
     -p <target_ip>:3260 --login
lsscsi -g
# [7:0:0:0]  mediumx MB      THUR VTL       NVL8  /dev/sch0
# [7:0:0:1]  tape    MB      Ultrium 8-SCSI NVL8  /dev/st0
```

On Windows, use the built-in iSCSI Initiator (Control Panel) — add
the portal, connect, and the devices appear under Tape drives and
Medium Changers.

## Manage cartridges

`thurvtl` manages the library without an iSCSI initiator. Cartridges
have to exist before any backup software can write to them, so this
is the next step after connecting:

```bash
thurvtl cartridge create TAPE001         # new cartridge, first free slot
thurvtl cartridge list                   # --json for automation
thurvtl changer inventory
thurvtl changer load 1 0
thurvtl system stats                     # dedup analytics
```

Cartridge lifecycle — creation flags, WORM, legal hold, at-rest
encryption — is in [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md).

## Examples

With the iSCSI session active, drive the changer with `mtx` and the
drive with `mt`, then back up with `tar`:

```bash
sudo mt  -f /dev/st0  status                 # drive status
sudo mtx -f /dev/sch0 status                 # library + slot inventory
sudo mtx -f /dev/sch0 load 1 0               # load slot 1 -> drive 0
sudo tar -cvf /dev/nst0 /path/to/backup      # write
sudo tar -xvf /dev/nst0                      # restore
```

# Using Thur VSA

## Connect (iSCSI or NVMe/TCP)

VSA serves each volume as a block LUN. The default transport is
iSCSI; set `transport: nvmetcp` in `thurvsa.yaml` to serve NVMe/TCP
instead (one listener binds, not both).

```bash
# iSCSI (port 3260)
sudo iscsiadm -m discovery -t sendtargets -p <target_ip>:3260
sudo iscsiadm -m node -T iqn.2025-10.com.metebalci:thurvsa \
     -p <target_ip>:3260 --login

# NVMe/TCP (port 4420)
sudo nvme connect -t tcp -a <target_ip> -s 4420 \
     -n nqn.2025-10.com.metebalci:thurvsa
```

The NVMe/TCP transport design (handshake, R2T flow, auth) is in
[`docs/NVMETCP.md`](docs/NVMETCP.md).

## Create a volume

`thurvsa` talks to the running daemon over its admin socket
(the daemon must be up):

```bash
thurvsa volume create myvol --size 100G
thurvsa volume list
```

After creating a new volume, rescan the initiator so the host sees
the new LUN:

```bash
sudo iscsiadm -m session --rescan        # iSCSI
sudo nvme ns-rescan /dev/nvme0           # NVMe/TCP
```

## Examples

The host now sees a thin-provisioned block device — partition,
format, and mount it like any disk:

```bash
lsblk                                    # find the new device (e.g. sdb, nvme0n1)
sudo parted -s /dev/sdb mklabel gpt mkpart primary ext4 0% 100%
sudo mkfs.ext4 /dev/sdb1
sudo mkdir -p /mnt/myvol
sudo mount /dev/sdb1 /mnt/myvol
```

If you destroy and recreate a volume, the kernel keeps the old
partition table cached and the new contents won't show up in
`lsblk` / `/proc/partitions`. Force a re-read on the affected block
device:

```bash
sudo blockdev --rereadpt /dev/sdb        # or /dev/nvme0n1
```

# Documentation

**Operations:** [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md),
[`docs/CLI.md`](docs/CLI.md),
[`docs/AUTH.md`](docs/AUTH.md),
[`docs/AUDIT.md`](docs/AUDIT.md),
[`docs/TELEMETRY.md`](docs/TELEMETRY.md),
[`docs/ALERTING.md`](docs/ALERTING.md),
[`docs/RELEASING.md`](docs/RELEASING.md).

[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) catalogues every
configuration file and YAML key; the same per-key reference is what
`<product> config defaults` prints, checked in under
[`dist/`](dist/).

**Conformance** — per-spec coverage tables plus the behavioral model:

- [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md) — SPC-4 /
  SAM-5 / iSCSI / CHAP (shared baseline), the SSC-4 / SMC-3 tape
  surface with deliberate divergences from typical LTO hardware, and
  the SBC-3 block surface.
- [`docs/CONFORMANCE_NVME.md`](docs/CONFORMANCE_NVME.md) —
  NVMe Base / NVM Command Set / NVMe-oF / NVMe-TCP.

**Wire-level & storage reference:**

- [`docs/SPEC.md`](docs/SPEC.md) — VTL wire surface, schemas,
  on-disk + storage-backend layout, DR / migration / archive.
- [`docs/STORAGE.md`](docs/STORAGE.md),
  [`docs/DEDUP.md`](docs/DEDUP.md),
  [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md),
  [`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md),
  [`docs/NVMETCP.md`](docs/NVMETCP.md).

**Development:**

- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — building from source,
  the test suite, running a daemon from the build tree.
- [`CLAUDE.md`](CLAUDE.md) — architecture orientation and repo map.
- [`docs/TESTCOVERAGE.md`](docs/TESTCOVERAGE.md) — per-crate line
  coverage and the end-to-end suite catalogue.
- Roadmap and open work — tracked as
  [GitHub issues](https://github.com/metebalci/thur/issues).

# Contributing

The project does not accept pull requests. Bug reports, feature
ideas, and questions are welcome as
[GitHub issues](https://github.com/metebalci/thur/issues), but no
commitment is made about whether or when any given request will be
implemented.

# Commercial Support

For commercial support or paid development work, get in touch:
[info@metebalci.com](mailto:info@metebalci.com).

# License

Copyright (c) 2026 Mete Balci

Thur VTL and Thur VSA are licensed under the **Apache License,
Version 2.0** (`Apache-2.0`). See the top-level [LICENSE](LICENSE)
file for the full text.

SPDX-License-Identifier: Apache-2.0
