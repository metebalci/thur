# Storage Layout

On-disk layout for both applications: the shared content-addressed chunk
pool and the per-unit directory each application keeps for a cartridge or
volume. Byte-level record layouts for the VTL index files
(`chunks.idx`, `blocks-p<N>.idx`, `lru.idx`, `manifest.json`) are in
[`SPEC.md`](SPEC.md) § On-Disk Layout — this document is the
interaction picture.

- **[VTL](#vtl)** — cartridge directory, tape index files,
  index-page backup, LTFS partitioning, integrity layers.
- **[VSA](#vsa)** — volume directory and page-table index.

## The shared chunk pool

Both applications store sealed chunks in a per-backend content-addressed
pool under `<data_dir>/chunks/`:

- **Global scope** — `<data_dir>/chunks/<backend>/<aa>/<bb>/<hash>.dat`
- **Local scope** — `<data_dir>/chunks/<backend>/<namespace>/<aa>/<bb>/<hash>.dat`

`<hash>` is the full BLAKE3 hash of the chunk's on-disk bytes; `<aa>`
and `<bb>` are its first two and next two hex chars (65 536-way
fanout). `<namespace>` is the cartridge barcode (VTL) or the volume
UUID hex (VSA). The **object key** is
`chunks/[<namespace>/]<aa>/<bb>/<hash>.dat` — the per-backend bucket
and optional prefix already isolate keys across backends, so the
storage-backend key carries no `<backend>` segment. Pool layout, the `local` /
`global` scope choice, refcount-aware eviction, and GC are the dedup
mechanism — see [`DEDUP.md`](DEDUP.md).

Each application splits per-unit state into a creation-frozen
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
  fetched from the storage backend, so the gap between a host counter
  and its backend counterpart shows the dedup + compression saving
  (write side) and the cache hit rate (read side). Rewritten via
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
  (LocalOnly|StorageOnly|Both) / backend-side `compression`.

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

`backup_manifest_to_storage` ships only the dirty pages to:

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
`Cartridge::open_with_storage_async` in Open mode allows the cartridge
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

## Read path — cache miss and read-ahead

The SCSI READ handler runs synchronously against the loaded cartridge's
on-disk pool, so the storage round-trips that hide read latency live in two
out-of-band async hooks the daemon fires *around* each tape READ (the
sync read itself can't `await`):

1. **On-demand refetch.** Before the read, the daemon peeks the chunk
   backing the next read LBA; if its pool file is missing (cold cache, or
   eviction pruned it mid-session) it downloads exactly that chunk from
   the cartridge's bound backend and warms it into the pool — blocking,
   because the read needs it now. A peek that finds the chunk already
   local is a prefetch *hit*; a download is a *miss*
   (`prefetch_hits_total` / `prefetch_misses_total`).

2. **Background read-ahead.** Sequential restores would still stall one
   storage round-trip per chunk if every read waited on its own refetch, so
   after the on-demand step the daemon fans the next
   `memory_buffers.read_prefetch_chunks_ahead` chunks (default 2) out to a
   per-backend `PrefetchManager`. It downloads them in the background —
   deduping against already-in-flight fetches and never blocking the host
   — so by the time the head reaches them they are already pool-resident.
   0 disables read-ahead; the on-demand refetch always runs. In-flight
   tasks are reported as `prefetch_queue_depth`, and the look-ahead bytes
   already warmed ahead of the head as `tape_read_buffer_used`.

Both hooks route through `ChunkPool::insert_verified_bytes`, so every
storage-fetched chunk is BLAKE3-verified against its content address before
it enters the pool (see *Integrity layers* below) and accounted against
the per-backend disk-cache budget.

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
the response. BLAKE3 runs once per backend download via
`ChunkPool::insert_verified_bytes`; the pool refuses bytes that don't
match the expected content address — so plaintext-uncompressed
cartridges (which otherwise lack any at-rest integrity check) and the
VSA block path both get the same backend-corruption guard.

GCM and codec checks fire on every block read regardless of the storage
path. A chunk corrupted by anything other than this daemon (manual
edits, filesystem bit-rot) on an encrypted-or-compressed cartridge
surfaces as a GCM auth failure or codec error. On a
plaintext-uncompressed cartridge the on-disk pool is trusted — chunks
are immutable after seal and the filesystem is the operator's
responsibility. Storage round-trip is the only path where untrusted
bytes can arrive, and it is guarded.

Mismatch surfaces at the iSCSI layer as CHECK CONDITION + MEDIUM ERROR
(0x03) + UNRECOVERED READ ERROR (0x11/0x00) — backup software (Veeam /
NetBackup / tar / Bacula) treats this as a per-block read failure,
logs and skips. The cartridge stays loaded and writable; only the
specific chunk fails.

An index-sidecar record that fails to decode (reserved tag bits in
`blocks-p<N>.idx` / `chunks.idx`, i.e. on-disk corruption of the
index file) surfaces the same way — MEDIUM ERROR + UNRECOVERED READ
ERROR (0x11/0x00) — on the READ / VERIFY data path, and for the block
index also on the SPACE walks (SPACE never decodes chunk records)
(issue #105). Unreadable medium metadata is a medium fault, not an
illegal request.

So does a codec-detected fault on a sealed chunk: a compressed
payload whose lz4/zstd frame fails to decode — on the warm read of a
cached chunk, or while decoding a refetched storage object one layer
before the BLAKE3 verify — is the same physical fault the hash check
catches, a rotted chunk payload, and maps to the same MEDIUM ERROR +
UNRECOVERED READ ERROR (issue #108). Codec failures on the
write/compress side and on staging chunks (the drive's internal
buffer, not yet on the medium) keep HARDWARE ERROR + INTERNAL TARGET
FAILURE (0x44/0x00): there the device failed, not the medium.

---

# VSA

## Volume directory

A volume is a directory under `<data_dir>/volumes/<name>/`:

- **`manifest.json`** — creation-frozen identity (schema v6): `name`,
  `uuid`, `size_bytes`, `sector_bytes` (default 4096),
  `page_size_bytes` (default 65536), `backend`, `lun`, `dedup_scope`,
  `worm`, `created_at`, optional `encryption` metadata (wrapped
  DEK for non-local keystore backends), optional `dedup_namespace`
  (schema v5; the chunk-pool *family* namespace — present only on
  snapshots and clones, see § Snapshots + clones), and optional
  `crypto_uuid` (schema v6; the *crypto identity* — present only on a
  clone of an encrypted volume, see § Snapshots + clones). Persisted
  atomically — tmp + fsync + rename. Pre-v6 manifests load with
  `crypto_uuid` (and pre-v5 with `dedup_namespace`) absent, each meaning
  "derive from my own `uuid`" — byte-for-byte the historical behaviour,
  no migration.
- **`runtime.json`** — daemon-mutated sidecar carrying the four
  per-volume byte counters plus `modified_at` and `sync_after`.
  `host_bytes_written` and `host_bytes_read` are logical,
  pre-dedup totals of what the
  initiator wrote and read; `backend_bytes_written` and
  `backend_bytes_read` are the post-dedup, post-compression bytes
  actually PUT to / fetched from the storage backend, so the gap between
  a host counter and its backend counterpart shows the dedup + compression
  saving (write side) and the cache hit rate (read side).
  `sync_after` is the SYNCHRONIZE CACHE durability tier (`storage` /
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
32-byte BLAKE3 hash of the page's chunk, an `allocated` flag bit, and
an 8-byte per-page `iv_salt` (format v2, issue #87). A host READ /
WRITE resolves `page_id = LBA / page_size`, then record → hash → pool
chunk.

For **encrypted** volumes the `iv_salt` is fed to AES-GCM nonce
derivation as `derive_iv(crypto_uuid, page_id, iv_salt)`: every seal
draws a fresh random salt, so each distinct ciphertext for a page gets
a unique nonce — eliminating the GCM nonce reuse that a deterministic
per-`(crypto_uuid, page_id)` IV caused on in-place rewrites and on
copy-on-write clone divergence. The salt lives in the record's
formerly-reserved tail, so it rides the same atomic 64-byte write as
the hash and copies for free with a wholesale `pages.idx` clone (so an
un-diverged shared chunk keeps decrypting under the salt it was sealed
with). A pre-salt **v1** record's zero tail reads `iv_salt = 0` — the
original IV — so existing encrypted volumes keep decrypting; `open`
migrates a v1 header to v2 in place and new seals start salting. The
salt is unused for unencrypted volumes (written as 0, never read).

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

## Snapshots + clones

A **snapshot** is a frozen point-in-time copy of a volume's page table.
It lives under the volume directory at
`<data_dir>/volumes/<parent>/snapshots/<snap>/`:

- **`snap.json`** — `SnapshotManifest` (schema v2): `name`, `uuid`
  (= the parent's uuid, so the copied index header validates with no
  rewrite), `parent_volume`, `parent_uuid`, `created_at`, `backend`,
  `dedup_scope`, `dedup_namespace` (the family namespace), `page_size_bytes`,
  `sector_bytes`, `size_bytes` (the parent's live size at snapshot time),
  the parent's optional `encryption` metadata, and optional `crypto_uuid`
  (schema v2; copied from the parent so a clone made from a snapshot of an
  encrypted clone inherits the right crypto identity).
- **`pages.idx`** — a byte-for-byte copy of the parent's page table at
  snapshot time.

Snapshots cost no chunk data: the frozen index references the same pool
chunks as the parent. Because chunks carry no on-disk refcount and a
chunk is alive iff some `pages.idx` references it, the existing
manifest-walking GC keeps a snapshot's chunks alive automatically. When
the parent later overwrites a page, its live `pages.idx` record moves to
the new chunk while the snapshot's frozen copy still references the old
one — copy-on-write, with no change to the host write path. Snapshots are
nested two levels deep so the daemon's discovery walk (which lists
`volumes/*`) never registers them as host LUNs, while GC and eviction
descend into `snapshots/`.

Snapshot create quiesces first — it flushes the volume's `PageCache` and
awaits its pending storage uploads — so the frozen index references only
storage-durable chunks (eviction may drop a snapshot-only chunk's local
copy and refetch it from the backend on read). The copy runs under the
cache lock, briefly pausing the volume's host I/O; `pages.idx` is sparse,
so the pause scales with allocated pages, and `std::fs::copy` reflinks on
btrfs/xfs/zfs. The snapshot is crash-consistent; the host fsyncs /
fs-freezes for application consistency.

A **clone** is a new writable top-level volume seeded with a copy of a
snapshot's (or the live volume's) page table — a first-class volume with
its own `uuid`, LUN, and page table. It inherits the source's *family*
`dedup_namespace` so its `Local`-dedup chunks resolve in the shared
family pool, and diverges from the source through ordinary copy-on-write
on write. A clone is a new volume name, so no host sees it until the
operator grants iSCSI / NVMe-TCP admission — it does **not** inherit the
source's grants.

Cloning an **encrypted** volume works too (issue #86). The shared chunks
are ciphertext sealed with the source's DEK under an IV derived from the
source's identity, so the clone inherits the source's *crypto identity*
in its `crypto_uuid` field — the single value that seeds both AES-GCM IV
derivation and the keystore wrap-context. The clone therefore derives the
right IV for the shared chunks and unwraps the *same* DEK (the source's
`encryption` metadata is copied verbatim — no re-wrap, no re-encrypt,
which would defeat COW). Its own `uuid` stays distinct for identity and
namespace. Divergent writes seal new ciphertext chunks under the same
crypto identity. The DEK's lifecycle is refcounted by a manifest-walk
scan (`crypto_identity_referenced`): `volume destroy` only forgets the
DEK once no other family member (source, sibling clone, or snapshot) still
keys its crypto identity on it, so destroying the source while a clone
exists never strands the clone. `volume key migrate` refuses a crypto
identity that is still shared, since re-wrapping one member's manifest
would desync the family.

The clone shares the source's IV identity, so an un-diverged shared page
decrypts against the same ciphertext as the source. A source page P and a
*diverged* clone page P sit at the same `page_id` under the same crypto
identity, but each seal carries its own per-page `iv_salt` in `pages.idx`
(issue #87): the wholesale page-table copy hands the clone the source's
salt for the shared chunk, while a divergent write draws a fresh salt, so
the two simultaneously-live ciphertexts never share an AES-GCM nonce. The
same salt removes the nonce reuse on single-volume page rewrites. (Even
before the salt this was never a correctness bug — distinct plaintexts
produce distinct ciphertext hashes, so no chunk ever aliases — but it now
holds GCM's confidentiality + integrity guarantees too.)

The family-namespace GC arithmetic is in [`DEDUP.md`](DEDUP.md) §
Snapshots + clones.

### Restore (in-place rollback)

A **restore** (issue #85) is the inverse of clone: rather than reading a
snapshot into a new volume, it rolls the *existing* volume back to the
snapshot in place. The volume keeps its identity — same UUID, LUN, name,
and DEK — and only its page table is rewound. Because a snapshot's frozen
`pages.idx` is bound to the parent's UUID (`snapshot.uuid ==
parent.uuid`), it is already valid for the live volume; restore rewrites
the live `pages.idx` byte-for-byte from the frozen copy through the *same*
file descriptor (no reopen, no UUID rebind — unlike clone, which mints a
fresh identity) and matches its length exactly, so any pages allocated
after the snapshot are dropped. It then resets the `upload.idx` / `lru.idx`
sidecars to empty: the snapshot references only storage-durable chunks
(snapshot-create's quiesce contract), so every page is honestly
`Uploaded` again.

The daemon quiesces first — `flush_all` drains dirty pages and awaits
pending uploads, then the cache's inner lock is held across the rewrite
(fencing the flush worker) while the whole `PageCache` is invalidated so
no stale cached page survives the swap. Chunks the volume referenced only
after the snapshot are now unreferenced and become orphans the next
`system gc` reclaims — the same leave-for-GC contract as `volume
destroy`. A snapshot still referencing a chunk keeps it alive, so a
restored volume that shares chunks with a sibling clone or another
snapshot loses nothing.

Restore is page-table-only by default: it refuses if the volume has been
resized since the snapshot, and refuses while a persistent reservation is
held. Passing `--resize` (issue #90) rolls the **logical size** back to
the snapshot's captured size too: after the page-table rewrite the daemon
calls the same `VolumeWriter::set_size` path `volume resize` uses — flip
the live size shadow, rewrite `manifest.json`, then fan a capacity-change
notice (NVMe Namespace Attribute Changed AER / iSCSI CAPACITY DATA HAS
CHANGED unit attention) to connected hosts. The size step runs *after* the
index swap on purpose: the restored index's high-water mark is already the
snapshot's, so a shrink-back sits within the shrink guard rails by
construction — nothing is allocated past the snapshot-era size, so
`set_size`'s would-discard-data check can never trip. The one case it
still refuses is a WORM volume, whose size is grow-only; since a WORM
volume can only have been *grown* after the snapshot, the rollback would
be a shrink, and that is rejected up front (before the index is touched)
so the volume is left wholly intact. Restore does **not** check for active
host sessions — the target cannot observe host mount state, and forcing a
logout (which would also drop other LUNs sharing the session) is too heavy
— so quiescing the host is the operator's responsibility, consistent with
the concurrent-same-LBA host-side-UB stance elsewhere in the cache. The
rewrite is not crash-atomic: a daemon crash mid-rewrite leaves a partial
index, but the snapshot copy is immutable, so the recovery is simply to
re-run the restore.

## Integrity

The BLAKE3 content-address check applies to VSA exactly as to VTL: a
backend-fetched chunk is admitted to the pool only through
`ChunkPool::insert_verified_bytes`, which recomputes the hash and
refuses non-matching bytes — the backend bit-rot / wrong-bytes guard. On
an encrypted volume the per-page AES-256-GCM tag additionally catches
at-rest tampering of the ciphertext on every page read. A verification
failure surfaces to the host as a per-page MEDIUM ERROR — the same way
the tape side fails a single chunk without faulting the whole device.

Only those integrity faults are medium faults. A local pread/pwrite
EIO on `pages.idx` or a cached chunk file, a storage-backend op
failure, daemon shutdown mid-command, or a keystore fault surfaces as
HARDWARE ERROR + INTERNAL TARGET FAILURE (0x44/0x00) over iSCSI and
`Internal Error` over NVMe/TCP — the device is at fault, not the
medium, the same split the tape side reports (issue #109). Both
transports classify through one shared classifier
(`core_block::FaultClass`), so an iSCSI host and an NVMe host see the
same fault class for the same failure.
