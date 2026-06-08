# Thur VTL and Thur VSA

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
  iSCSI; configurable topology (caps 65535 storage slots / 255 drives;
  one Import/Export element).
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

# Documentation

All documentation lives under [`docs/`](docs/), organized into four sets
by audience — see [`docs/README.md`](docs/README.md) for the full map:

- **[Quick Start](docs/QUICKSTART.md)** — install to a working device,
  fast.
- **[Admin Guide](docs/admin/)** — installation, configuration, storage
  backends, connecting hosts, VTL/VSA operations, security, monitoring,
  disaster recovery, troubleshooting, production-readiness.
- **[Reference](docs/reference/)** — how it works: the wire spec, SCSI /
  NVMe conformance, and the storage / dedup / backpressure / transport
  internals.
- **[Developer](docs/dev/)** — building from source, the test suite,
  releasing.

Roadmap and open work are tracked as
[GitHub issues](https://github.com/metebalci/thur/issues).

# License

Copyright (c) 2026 Mete Balci

Thur VTL and Thur VSA are licensed under the **Apache License,
Version 2.0** (`Apache-2.0`). See the top-level [LICENSE](LICENSE)
file for the full text.

SPDX-License-Identifier: Apache-2.0
