# Thur

> **Status: alpha — under active development.** Thur has not had a
> stable release. On-disk formats, configuration keys, and the
> SCSI / NVMe surface may change without notice or migration path.
> Not recommended for production use.

Thur is two sibling cloud-backed storage targets built on a shared Rust
codebase. Each presents an ordinary storage device to the host, but the
data lives in cloud object storage — local disk holds only a warm,
refcount-evicted cache in front of it. The capacity a host can address
is set by the cloud bucket, not the local disk: a modest cache machine
can front a dataset many times its own size.

Stored data is deduplicated in a shared content-addressed chunk pool.
Every chunk is keyed by its BLAKE3 hash, with no central index or
database, so identical bytes from any source — across every volume and
cartridge on a backend — are stored exactly once.

The two products share that chunk pool, the dedup layer, and the
cloud-tiering machinery, but they present different device types over
different host protocols, and are packaged and deployed independently:

- **Thur VTL** — a Virtual Tape Library that presents a spec-conformant
  SMC-3 medium changer and SSC-4 LTO-8 drive surface over iSCSI. From
  the backup software's perspective it is an ordinary tape library — no
  proprietary agent required.
- **Thur VSA** — a Virtual Storage Appliance: a block-storage target
  that serves thin-provisioned SBC-3 LUNs over iSCSI or NVMe/TCP. It
  behaves as a cloud-backed virtual disk for VMware, Hyper-V, and Linux
  hosts.

Both products can live co-resident on a single host; they use disjoint
system users, data directories, conffiles, systemd unit names, and admin
sockets. Cloud storage (S3, GCS, or Azure) is, in all cases, the durable
source of truth.

# Disclaimer

**Thur is experimental backup and storage software.** It is provided
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
- **Cloud tiering** — S3-compatible (AWS S3, MinIO, Wasabi, …), Google
  Cloud Storage, Azure Blob. Disk is a warm cache with a per-backend
  budget and write backpressure when the budget is hit.
- **Two-layer compression** (zstd / lz4); parallel cloud up/downloads.
- **CHAP authentication** for iSCSI; **TLS-PSK** for NVMe/TCP (VSA).
- **Append-only, BLAKE3-chained audit log**; Prometheus metrics + OTLP.
- **Optional at-rest encryption** under a pluggable DEK keystore.

Thur VTL additionally provides:

- Spec-conformant **SMC-3 medium changer + SSC-4 LTO-8 drives** over
  iSCSI; configurable topology (caps 65535 slots / 65535 mail slots /
  255 drives).
- Virtual cartridges with full sequential-access semantics, WORM,
  cloud-native legal hold, the LTFS two-partition layout, and LTO
  Application-Managed Encryption.
- Cross-region disaster recovery; cartridge migration and archive
  between cloud backends.

Thur VSA additionally provides:

- **SBC-3 block target** over iSCSI, or the **NVM Command Set over
  NVMe/TCP**.
- Thin-provisioned per-volume LUNs, 4 KiB sectors, sparse page table,
  write-back page cache.
- VAAI / NVMe data-path primitives and persistent reservations.

# Getting Started

## Prerequisites

- Linux (tested on Ubuntu 26.04).
- Building from source: a C compiler, OpenSSL headers, pkg-config,
  and Rust 1.92+ (2024 edition).
- Integration tests: `open-iscsi`, `sg3-utils`, `mtx`, `mt-st`,
  `libiscsi-bin`, `lsscsi`.
- A cloud storage account (S3 / GCS / Azure) for production; the
  `local` backend needs none.

## Install

The shortest path is the **https://thur.metebalci.com** apt / yum
repository — one line on any supported distro:

```bash
curl -fsSL https://thur.metebalci.com/install.sh | sudo bash
```

This wires up the `stable` channel (GA only, 1.0.0+); set
`CHANNEL=unstable` for the pre-GA channel that holds every 0.x and
release-candidate tag. The script writes the right `sources.list.d` or
`yum.repos.d` entry and installs the signing public key alongside it.
The fingerprint is documented in
[`docs/RELEASING.md`](docs/RELEASING.md) — verify it against the key
the script imports.

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

Alternatively, invoke the CLI as the daemon user per command —
`sudo -u thurvtl thurvtl ...`. Pure-local commands (`config defaults`)
use no socket and need neither.

Every release artifact ships a detached `.asc` GPG signature — verify
it before installing. Key fingerprint and the build / signing process
are in [`docs/RELEASING.md`](docs/RELEASING.md).

Other install paths:

- **Static binary tarball** — for distributions outside the
  `.deb` / `.rpm` matrix; carries the binaries, a reference yaml, the
  systemd unit, shell completions, man pages, and an install-focused
  README.
- **From source** — `cargo build --release`; binaries land in
  `target/release/`. Not recommended for production.

## Configure

The one required key in each conffile is `data_dir`. Add at least one
cloud backend under `cloud.backends:`. The `config defaults` command
prints the full annotated reference — every key documented with its
default value and a description — which you can redirect straight to a
starter file:

```bash
thurvtl config defaults > thurvtl.yaml
thurvsa config defaults > thurvsa.yaml
```

Cloud credentials are wired per-backend (`auth:` blocks) or via the
default credential chain / daemon env file; the reference is
[`docs/AUTH.md`](docs/AUTH.md). Every config file and YAML key is
catalogued in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md). Both
the daemon and CLI resolve `--config PATH` first, otherwise
`/etc/<product>/<product>.yaml`.

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

Each daemon runs an iSCSI target (port 3260) and an HTTP metrics
server (port 9090) in a single process. For co-resident installs,
override one of each port in YAML. Persist systemd customizations
through `sudo systemctl edit <unit>` so package upgrades don't
clobber them.

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

The host kernel sees a standard tape library — back up with `tar`,
control the drive with `mt`, drive the changer with `mtx`:

```bash
sudo tar -cvf /dev/nst0 /path/to/backup      # write
sudo tar -xvf /dev/nst0                      # restore
sudo mt  -f /dev/st0  status                 # drive status
sudo mtx -f /dev/sch0 status                 # library status
sudo mtx -f /dev/sch0 load 1 0               # load slot 1 -> drive 0
```

## Manage cartridges

`thurvtl` manages the library without an iSCSI initiator. This is
useful for provisioning cartridges, inspecting inventory, and running
analytics before or after a backup session:

```bash
thurvtl cartridge create TAPE001         # new cartridge, first free slot
thurvtl cartridge list                   # --json for automation
thurvtl changer inventory
thurvtl changer load 1 0
thurvtl system stats                     # dedup analytics
```

Cartridge lifecycle — creation flags, WORM, legal hold, at-rest
encryption — is in [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md).

# Using Thur VSA

## Create a volume

`thurvsa` talks to the running daemon over its admin socket
(the daemon must be up):

```bash
thurvsa volume create myvol --size 100G
thurvsa volume list
```

## Connect

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

The host sees a thin-provisioned block device — partition, format,
and mount it like any disk. The NVMe/TCP transport design (handshake,
R2T flow, auth) is in [`docs/NVMETCP.md`](docs/NVMETCP.md).

# Cloud Backends

Both products store data as content-addressed chunks in a per-backend
pool. The backend type determines where those chunks live; configure
backends under `cloud.backends:` in the conffile, giving each entry a
name, a `type` (`s3` / `gcs` / `azure` / `local`), and its
per-cloud knobs:

```yaml
cloud:
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

The `local` backend is filesystem-only — no credentials, no cloud —
ideal for testing:

```yaml
cloud:
  backends:
    primary:
      type: local
      root_dir: "./.thur/local-backend"
```

On startup the daemon validates each cloud backend's credentials,
bucket existence, and read/write/delete permissions, and refuses to
start on failure. Validate ahead of time, without starting the
daemon, with `thurvtl system cloud check`.

- **Credentials** — `auth:` blocks, default chains, the daemon env
  file, multi-provider layouts: [`docs/AUTH.md`](docs/AUTH.md).
- **S3-compatible provider matrix** — Backblaze B2, Wasabi, Hetzner,
  OVHcloud, …: [`docs/S3_BACKENDS.md`](docs/S3_BACKENDS.md).
- **WORM, legal hold, at-rest encryption** (incl. provider bucket
  setup): [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md).
- **Cross-region DR, cartridge migration / archive** —
  [`docs/SPEC.md`](docs/SPEC.md).

# Monitoring & Audit

Each daemon exposes HTTP on port 9090 for health and observability
probes. The `/metrics` endpoint serves Prometheus-formatted metrics
and is always wired — there is no on/off switch separate from the HTTP
listener itself:

```bash
curl http://localhost:9090/health      # liveness probe
curl http://localhost:9090/metrics     # Prometheus
curl http://localhost:9090/sessions    # iSCSI sessions
curl http://localhost:9090/info        # topology (library / volume count)
```

Metrics are prefixed `thurvtl_*` / `thurvsa_*`. Both daemons keep an
always-on, append-only, BLAKE3-chained audit log under
`<data_dir>/audit/`, with optional offsite shipping to a cloud
backend. The chain means any after-the-fact modification to the log
is detectable:

```bash
thurvtl system audit tail -f
thurvtl system audit verify         # exit 0 valid, 1 break, 2 io error
```

Telemetry design — [`docs/TELEMETRY.md`](docs/TELEMETRY.md); audit
design — [`docs/AUDIT.md`](docs/AUDIT.md); opt-in email / webhook
alerting — [`docs/ALERTING.md`](docs/ALERTING.md).

# Development

```bash
cargo build [--release]       # binaries in target/{debug,release}/
cargo test
cargo fmt && cargo clippy

# Run a daemon in the foreground from the build tree
RUST_LOG=info ./target/release/thurvtld --config thurvtl.yaml
```

`cargo test` runs the workspace suite — 1,299 unit and integration
tests across the 38 crates. Measured with `cargo llvm-cov`, the storage
and protocol crates (storage engines, SCSI / NVMe command sets, dedup,
crypto, chunk pool) carry **75–95% line coverage**; the daemon and CLI
integration surface is covered separately by the end-to-end conformance
suites under `vtl/scripts/` and `vsa/scripts/` (`test-smoke.sh`,
`test-*-conformance.sh`, and backup / filesystem workflow tests) — each
script's header documents its prerequisites and what it covers.
[`docs/TESTCOVERAGE.md`](docs/TESTCOVERAGE.md) has the per-crate
coverage breakdown, the methodology, and the suite catalogue.

The release-cut process is in [`docs/RELEASING.md`](docs/RELEASING.md);
the workspace crate map is in [`docs/WORKSPACE.md`](docs/WORKSPACE.md).

# Documentation

- [`CLAUDE.md`](CLAUDE.md) — architecture orientation and repo map.
- Roadmap and open work — tracked as
  [GitHub issues](https://github.com/metebalci/thur/issues).
- [`docs/TESTCOVERAGE.md`](docs/TESTCOVERAGE.md) — per-crate line
  coverage and the end-to-end suite catalogue.

**Conformance** — per-spec coverage tables plus the behavioral model:

- [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md) — SPC-4 /
  SAM-5 / iSCSI / CHAP (shared baseline), the SSC-4 / SMC-3 tape
  surface with deliberate divergences from typical LTO hardware, and
  the SBC-3 block surface.
- [`docs/CONFORMANCE_NVME.md`](docs/CONFORMANCE_NVME.md) —
  NVMe Base / NVM Command Set / NVMe-oF / NVMe-TCP.

**Wire-level & storage reference:**

- [`docs/SPEC.md`](docs/SPEC.md) — VTL wire surface, schemas,
  on-disk + cloud layout, DR / migration / archive.
- [`docs/STORAGE.md`](docs/STORAGE.md),
  [`docs/DEDUP.md`](docs/DEDUP.md),
  [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md),
  [`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md),
  [`docs/NVMETCP.md`](docs/NVMETCP.md).

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

# License

Copyright (c) 2026 Mete Balci

Thur VTL and Thur VSA are licensed under the **Apache License,
Version 2.0** (`Apache-2.0`). See the top-level [LICENSE](LICENSE)
file for the full text.

SPDX-License-Identifier: Apache-2.0
