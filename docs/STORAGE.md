# Storage Layout

On-disk layout for both products: the shared content-addressed chunk
pool and the per-unit directory each product keeps for a cartridge or
volume. Byte-level record layouts for the VTL index files
(`chunks.idx`, `blocks-p<N>.idx`, `lru.idx`, `manifest.json`) are in
[`SPEC.md`](SPEC.md) § On-Disk Layout — this document is the
interaction picture.

- **[VTL](#vtl)** — cartridge directory, tape index files,
  index-page backup, LTFS partitioning, integrity layers.
- **[VSA](#vsa)** — volume directory and page-table index.

## The shared chunk pool

Both products store sealed chunks in a per-backend content-addressed
pool under `<data_dir>/chunks/`:

- **Global scope** — `<data_dir>/chunks/<backend>/<aa>/<bb>/<hash>.dat`
- **Local scope** — `<data_dir>/chunks/<backend>/<namespace>/<aa>/<bb>/<hash>.dat`

`<hash>` is the full BLAKE3 hash of the chunk's on-disk bytes; `<aa>`
and `<bb>` are its first two and next two hex chars (65 536-way
fanout). `<namespace>` is the cartridge barcode (VTL) or the volume
UUID hex (VSA). The **object key** is
`chunks/[<namespace>/]<aa>/<bb>/<hash>.dat` — the per-backend bucket
and optional prefix already isolate keys across backends, so the cloud
key carries no `<backend>` segment. Pool layout, the `local` /
`global` scope choice, refcount-aware eviction, and GC are the dedup
mechanism — see [`DEDUP.md`](DEDUP.md).

Each product splits per-unit state into a creation-frozen
**`manifest.json`** (identity: UUID, capacity, backend binding, dedup
scope, WORM flag — written once, never rewritten on the hot path) and
a daemon-mutated **`runtime.json`** sidecar (counters and runtime
knobs). Keeping `manifest.json` byte-stable lets it ride a
retention-locked backend untouched and lets out-of-band identity edits
(migration, restore) avoid racing the daemon's hot-path persist.

---

# VTL

## Cartridge directory

A cartridge is a directory under `<data_dir>/tapes/<barcode>/`:

- **`manifest.json`** — creation-frozen identity: chunking mode, UUID,
  capacity, sticky `backend` (the bound `storage.backends` entry),
  sticky `worm: bool`, sticky `dedup` scope. Written once at
  `cartridge create`; the hot path never rewrites it. Only
  `cartridge migrate` (rewrites `backend`) and archive provenance
  stamping touch it post-create.
- **`runtime.json`** — daemon-mutated sidecar: `partitions`,
  `active_partition`, `pending_partition_layout`,
  `set_capacity_proportion`, `index_epoch`, and four lifetime byte
  counters. `host_bytes_written` and `host_bytes_read` are logical,
  pre-dedup totals of what the host
  wrote and read; `backend_bytes_written` and `backend_bytes_read`
  are the post-dedup, post-compression bytes actually PUT to /
  fetched from cloud, so the gap between a host counter and its
  backend counterpart shows the dedup + compression saving (write
  side) and the cache hit rate (read side). Rewritten via
  `Cartridge::persist_runtime` at every runtime-mutating boundary
  (LOCATE-cross-partition, MODE SELECT 0x11, FORMAT MEDIUM, ERASE,
  SET CAPACITY, manifest backup) and once more when the cartridge is
  unloaded, so the read counters survive a pure sequential restore
  that triggers none of those boundaries.
- **`chunks.idx`** — per-cartridge fixed-record chunk index.
- **`lru.idx`** — local-only LRU sidecar.
- **`blocks-p<N>.idx`** — per-partition fixed-record block index.
- **`<file>.dirty`** sidecars — 1 MiB-page bitmap + monotonic epoch
  driving delta backend uploads (`dirty_pages.rs`). One per `chunks.idx`
  and per `blocks-p<N>.idx`.
- **`.staging/`** — the active unsealed chunk only.

A restore that crashes between writing manifest and runtime refuses
cleanly via `Runtime::load`'s missing-sidecar error.

Both index files use a 32-byte header (magic + version + record-size
as u32 LE) plus a positional records area.

## Drive state — `<data_dir>/library/drive_state.json`

Library-wide local-only sidecar holding emulated drive NVRAM. One
[`DriveState`] entry per drive id, keyed in a JSON object. Loaded by
`DriveManager` at startup; rewritten atomically (tmp + rename) on every
host MODE SELECT with SP=1.

Carries the SCSI MODE SELECT round-trip bodies that MODE SENSE under
PC=Current / PC=Saved replays. Future drive-side knobs extend the
`DriveState` envelope rather than adding another file.

- **Drive-scoped, not cartridge-scoped.** Real LTO drives store
  saved-mode-page state in NVRAM, so values persist across cartridge
  swaps. VTL mirrors that — keyed by drive id, not barcode.
- **Not in `manifest.json`.** The manifest may live on a
  retention-locked backend; drive-side configuration must stay freely
  re-writable, so it gets its own local-only sidecar — same treatment
  as `lru.idx`. Never uploaded; cold-bucket DR rebuilds it empty.

## Chunk index — `chunks.idx`

One fixed 64-byte record per chunk, indexed by `chunk_id`
(per-cartridge monotonic 0, 1, 2, …, derived from file offset — never
stored explicitly). Magic `NVCI`. Carries:

- `size` (u32 LE) — bounded by `BlockRec.offset`'s u32 width, so a
  chunk can never exceed 4 GiB.
- raw 32-byte BLAKE3 `hash`.
- flags byte packing `hash_present` / `uploaded` / `location`
  (LocalOnly|CloudOnly|Both) / storage-side `compression`.

`hash` is `Some(hex)` once sealed into the pool, `None` while in
`.staging/`. Every per-chunk mutation (mark uploaded, transition
`location`) is an O(1) `pwrite_at(id * 64)`.

Active chunks seal into the pool on roll or on cartridge drop
(`Cartridge::Drop` → `flush_and_seal`). Empty trailing staging chunks
are cleaned up on `flush_and_seal` to avoid `create_new` collision on
the next open. Exception: a brand-new cartridge whose only chunk is
empty staging is preserved — `chunks.idx` needs at least one record.

## LRU sidecar — `lru.idx`

One fixed 8-byte record per chunk_id (u64 LE epoch seconds),
positional and mirrored 1:1 with `chunks.idx`. Magic `TVLI`. Holds
last-accessed timestamps for disk-cache LRU eviction — split out of
`chunks.idx` so the read path's `touch` doesn't dirty
storage-replicated metadata pages.

**Local-only**: never has a `.dirty` sidecar, never enumerated by
`index_backup`, never restored on cold-bucket DR. A fresh host
rebuilds it as zeros sized to `chunks.idx.next_id()`; the first
eviction cycle picks oldest uniformly, later cycles converge as
touches arrive. Reset / corrupt header → rebuilt empty (cache hint,
not authoritative). fsync cadence matches `chunks.idx` (chunk-roll,
drop, truncate); read/write `touch` does not fsync, so transient LRU
loss on crash is tolerated by design.

## Index page backup

`dirty_pages.rs` + `index_backup.rs`. Each `chunks.idx` and
`blocks-p<N>.idx` owns a sidecar `<file>.dirty` (magic `NVDP`, 1
MiB-page bitmap, monotonic epoch). Every `pwrite_at` / `truncate_to`
mark-before-writes the affected pages; `fsync` persists the sidecar.

`backup_manifest_to_cloud` ships only the dirty pages to:

- `manifests/<barcode>/chunks/page-<NNNNNN>.dat`
- `manifests/<barcode>/blocks-p<N>/page-<NNNNNN>.dat`

…clears the bits as each PUT lands, bumps the epoch, writes the
versioned manifest, then writes the `manifest-latest.json` sentinel
last (so a torn upload leaves the sentinel pointing at the previous
consistent epoch).

The manifest's `index_epoch: BTreeMap<String, IndexEpoch>` records,
per file label (`chunks`, `blocks-p0`, …), `pages` / `page_size` /
`epoch` / `file_size` so cold-bucket restore knows what to fetch and
how big to grow each file.

Without this layer a cold-bucket restore can fetch chunks but can't
map LBA → chunk_id → hash. With it,
`Cartridge::open_with_cloud_async` in Open mode allows the cartridge
directory to be entirely missing locally as long as a storage backend is
configured, and rebuilds `chunks.idx` + `blocks-p<N>.idx` from the
backend copy before opening them.

## LTFS partitioning

Up to 2 partitions (P0 = Index, P1 = Data) per LTO-7+. Default is
single-partition (P0). Implementation in `cartridge.rs` (`Partition`,
`PendingPartitionLayout`, `apply_format_medium`, `locate_partition`,
`set_allow_overwrite`). `overwrite_barrier` (ALLOW OVERWRITE 0x82) is
volatile drive state, never persisted.

## Tape semantics

0-based LBA, filemarks (zero-length blocks), BOT/EOD, SPACE
(records/filemarks), LOCATE (random access).

## Integrity layers

Four independent layers, each firing on its own trigger for a distinct
failure mode — none redundant with another.

| Layer       | Scope               | When fires                          | Catches                                          | Stored where                   |
| ----------- | ------------------- | ----------------------------------- | ------------------------------------------------ | ------------------------------ |
| LBP CRC32C  | per host LBA, wire  | every READ if `LBP_R=1`             | host ↔ target in-flight corruption               | computed fresh, not stored     |
| AES-GCM tag | per block, at-rest  | every block read if encrypted       | at-rest tampering of ciphertext                  | inline in chunk file           |
| Codec CRC   | per chunk, at-rest  | every block read if compressed      | lz4 / zstd structural corruption                 | inline in codec frame          |
| BLAKE3      | per chunk, at-rest  | **at backend-download time only**     | backend bit-rot / wrong-bytes-for-hash             | filename is the hash (implicit) |

LBP is wire-only (host ↔ target), fresh-computed per READ, gone after
the response. BLAKE3 runs once per cloud download via
`ChunkPool::insert_verified_bytes`; the pool refuses bytes that don't
match the expected content address — so plaintext-uncompressed
cartridges (which otherwise lack any at-rest integrity check) and the
VSA block product both get the same backend-corruption guard.

GCM and codec checks fire on every block read regardless of the cloud
path. A chunk corrupted by anything other than this daemon (manual
edits, filesystem bit-rot) on an encrypted-or-compressed cartridge
surfaces as a GCM auth failure or codec error. On a
plaintext-uncompressed cartridge the on-disk pool is trusted — chunks
are immutable after seal and the filesystem is the operator's
responsibility. Cloud round-trip is the only path where untrusted
bytes can arrive, and it is guarded.

Mismatch surfaces at the iSCSI layer as CHECK CONDITION + MEDIUM ERROR
(0x03) + UNRECOVERED READ ERROR (0x11/0x00) — backup software (Veeam /
NetBackup / tar / Bacula) treats this as a per-block read failure,
logs and skips. The cartridge stays loaded and writable; only the
specific chunk fails.

---

# VSA

## Volume directory

A volume is a directory under `<data_dir>/volumes/<name>/`:

- **`manifest.json`** — creation-frozen identity (schema v4): `name`,
  `uuid`, `size_bytes`, `sector_bytes` (default 4096),
  `page_size_bytes` (default 65536), `backend`, `lun`, `dedup_scope`,
  `worm`, `created_at`, and optional `encryption` metadata (wrapped
  DEK for non-local keystore backends). Persisted atomically — tmp +
  fsync + rename.
- **`runtime.json`** — daemon-mutated sidecar carrying the four
  per-volume byte counters plus `modified_at` and `sync_after`.
  `host_bytes_written` and `host_bytes_read` are logical,
  pre-dedup totals of what the
  initiator wrote and read; `backend_bytes_written` and
  `backend_bytes_read` are the post-dedup, post-compression bytes
  actually PUT to / fetched from cloud, so the gap between a host
  counter and its backend counterpart shows the dedup + compression
  saving (write side) and the cache hit rate (read side).
  `sync_after` is the SYNCHRONIZE CACHE durability tier (`cloud` /
  `disk` / `memory`, see [`BACKPRESSURE.md`](BACKPRESSURE.md) § VSA).
  The counters live as in-memory atomics and reach this file at
  flush boundaries plus a 60-second timer; `thurvsa volume info`
  surfaces all four.
- **`pages.idx`** — the page table (next section).
- **`lru.idx`** — per-page last-accessed timestamps; local-only.
- **`upload.idx`** — per-page async-upload state (`Uploaded` /
  `LocalOnly`); local-only.

`lru.idx` and `upload.idx` — byte format (`CSLI` / `CSUI` magic,
sparse positional records) and their role in eviction and
backpressure — are documented in [`BACKPRESSURE.md`](BACKPRESSURE.md)
§ VSA.

## Page table — `pages.idx`

VSA presents a thin-provisioned block device: 4 KiB sectors, a fixed
page (default 64 KiB) as the unit of storage. `pages.idx` maps each
page slot to the content-addressed pool chunk that holds it.

A sparse positional index — magic `CRPI`, a 64-byte header (magic,
version, record size, volume UUID, `page_size_bytes`), then one fixed
64-byte record per `page_id` (a `u32`). Each record carries the
32-byte BLAKE3 hash of the page's chunk plus an `allocated` flag bit.
A host READ / WRITE resolves `page_id = LBA / page_size`, then record
→ hash → pool chunk.

The index is **sparse**: a `page_id` the host never wrote is a file
hole consuming zero disk — the thin-provisioning mechanism. A 100 TiB
volume serving a 4 GiB working set occupies ~4 GiB of pages plus its
chunk-pool footprint, not 100 TiB. Rewriting a page updates its
`pages.idx` record to the new chunk's hash and leaves the superseded
chunk for GC (see [`DEDUP.md`](DEDUP.md)).

A page becomes a pool chunk through `VolumeWriter::write_page_unsynced`.
Sub-page host writes (the 4 KiB sector vs 64 KiB page mismatch) are
read-modify-merged in the per-volume RAM `PageCache` first, so the
pool only ever sees whole pages. That cache tier, the write-back path,
and the SYNCHRONIZE CACHE durability fence are in
[`BACKPRESSURE.md`](BACKPRESSURE.md) § VSA.

## Integrity

The BLAKE3 content-address check applies to VSA exactly as to VTL: a
backend-fetched chunk is admitted to the pool only through
`ChunkPool::insert_verified_bytes`, which recomputes the hash and
refuses non-matching bytes — the backend bit-rot / wrong-bytes guard. On
an encrypted volume the per-page AES-256-GCM tag additionally catches
at-rest tampering of the ciphertext on every page read. A verification
failure surfaces to the host as a per-page MEDIUM ERROR — the same way
the tape side fails a single chunk without faulting the whole device.
