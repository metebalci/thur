# Concepts

The mental model an operator needs before configuring and running either
product. It is deliberately light on internals — the deep mechanics live
in the reference set ([`../reference/STORAGE.md`](../reference/STORAGE.md),
[`../reference/DEDUP.md`](../reference/DEDUP.md),
[`../reference/BACKPRESSURE.md`](../reference/BACKPRESSURE.md)).

## Two products, one engine

Thur VTL and Thur VSA are siblings on a shared backend. They diverge only
in the device surface they present and the on-host data shape above it:

- **Thur VTL** presents an SMC-3 medium changer + SSC-4 LTO-8 tape drives
  over iSCSI. Its on-host unit is the **cartridge** (sequential access).
  Backup software sees an ordinary tape library.
- **Thur VSA** presents SBC-3 block LUNs over iSCSI or NVMe/TCP. Its
  on-host unit is the **volume** (random access). A host sees an ordinary
  block device.

Everything below the device surface — the chunk pool, storage backends,
iSCSI transport, audit log, telemetry, alerting, encryption — is the same
code. Both daemons co-exist on one host with disjoint users, data dirs,
conffiles, unit names, and admin sockets.

## The backend is the source of truth; disk is a cache

Stored data does not live "on the appliance." It lives in a **storage
backend** — an object store (S3, GCS, Azure Blob, or any S3-compatible
store like MinIO / Ceph RGW / Wasabi) or a `local` filesystem. The
backend is the durable source of truth.

The on-host disk is a **warm cache** in front of it, typically far
smaller than the addressable capacity. Each backend gets its own
`disk_cache` budget; when the working set exceeds it, the cache evicts
data that is already safely uploaded (refcount-aware, so nothing
referenced is dropped). A read that misses the cache refetches from the
backend transparently. This is what lets a small box front a very large
library or volume set.

## Content-addressed dedup

Both products reduce cartridges/volumes to **chunks** — variable-sized
(VTL) or fixed-page (VSA) byte ranges — keyed by their BLAKE3 hash and
stored once per backend pool. Identical bytes from any source — across
every cartridge and volume on a backend — are stored a single time. There
is no central index or database; the hash *is* the address. Compression
(zstd/lz4) is applied on upload, after dedup.

## Thin provisioning

Capacity is **declared, not reserved**. A 100 TB volume or a 40-slot
library consumes only the storage that written data actually occupies
(after dedup and compression). You can over-subscribe addressable
capacity well beyond physical backend size — monitor real usage via
`system stats` and the telemetry metrics.

## Backpressure

Host writes land in a cache and drain to the backend asynchronously. If
the host writes faster than the backend can absorb for long enough, the
cache budget fills and the daemon applies **backpressure** — it parks the
write and, on timeout, returns SCSI NOT READY rather than losing data or
growing the cache unbounded. The lever is backend bandwidth: it must keep
up with the sustained host write rate. See
[`../reference/BACKPRESSURE.md`](../reference/BACKPRESSURE.md).

## Always-on safety rails

- **Audit log** — every operator and host-driven state change is recorded
  in an append-only, BLAKE3-chained JSONL log per daemon. Always on.
  [`AUDIT.md`](AUDIT.md).
- **At-rest encryption** — optional AES-256-GCM per cartridge/volume under
  a pluggable DEK keystore (local sidecar, AWS KMS, Vault, Azure Key
  Vault, GCP KMS, KMIP HSM). [`ENCRYPTION.md`](ENCRYPTION.md).
- **WORM + legal hold** (VTL) — write-once retention and backend-native
  legal holds. [`CARTRIDGE.md`](CARTRIDGE.md).
- **Telemetry + alerting** — Prometheus/OTLP metrics and opt-in
  email/webhook alerts. [`TELEMETRY.md`](TELEMETRY.md),
  [`ALERTING.md`](ALERTING.md).

## Glossary

| Term | Meaning |
|---|---|
| **Backend** | A named storage destination (`storage.backends:` entry) — S3/GCS/Azure/local. The durable source of truth. |
| **Chunk pool** | The per-backend, content-addressed store of deduplicated chunks. |
| **Chunk** | A hash-addressed byte range — the unit of dedup and upload. |
| **Disk cache** | The on-host warm copy of recently used chunks, with a per-backend size budget and refcount-aware eviction. |
| **Cartridge** (VTL) | A virtual tape — sequential-access, lives in a slot, loaded into a drive. |
| **Volume** (VSA) | A thin-provisioned block LUN with a sparse page table. |
| **Slot / drive / changer** (VTL) | Storage slot holds a cartridge; drive reads/writes; changer (medium changer, LUN 0) moves cartridges. |
| **Snapshot / clone** (VSA) | Frozen point-in-time page table; a clone is a new writable LUN seeded from one. [`VSA_OPERATIONS.md`](VSA_OPERATIONS.md). |
| **Backpressure** | Flow control that parks host writes when the cache budget fills, surfacing SCSI NOT READY on timeout. |
| **DEK / keystore** | Data Encryption Key and the backend that wraps it, for at-rest encryption. |
| **Admin socket** | The peer-cred-authed Unix socket the daemon-routed CLI talks to (`/run/<product>/admin.sock`). |
| **WORM / legal hold** (VTL) | Write-once-read-many retention and backend-native immutability holds. |
