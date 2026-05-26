# Upload Backpressure

Both daemons use the same mechanism to keep the local chunk pool bounded under
sustained write pressure, rather than letting the disk fill until the SCSI
write path is forced to abort the host's backup. This document explains how
that mechanism works and why it is built this way.

VTL and VSA share the upload pipeline end-to-end, so they share the core
backpressure design. Two sections cover the product-specific details:

- **[VTL](#vtl)** — the tape side: chunk-seal gating against the
  SSC-4 sequential write stream.
- **[VSA](#vsa)** — the block side: per-page gating against the
  SBC-3 / NVMe write path, plus the SYNCHRONIZE CACHE durability tier.

## The shared problem

A host workload can burst faster than backend upload can drain — a `tar` of a
large dataset on VTL, or a journal flush, large copy, or snapshot cohort flush
on VSA. Storage-backend upload throughput varies with network conditions, provider
throttling, and retry backoff. The local chunk pool sits between the host write
and the backend PUT and absorbs that mismatch.

If writes persistently outpace uploads, the pool grows. The difference between
having backpressure and not having it is the difference between a graceful
pause and a failed backup:

- **Without backpressure**: the pool overflows its per-backend cap, fills the
  filesystem, the next staging write hits `ENOSPC` → SCSI MEDIUM ERROR →
  backup software marks the medium failed and aborts.
- **With backpressure**: the seal or page write blocks at the cap; if uploads
  cannot free space within the timeout, the SCSI surface returns NOT READY →
  backup software retries → the workload pauses until uploads catch up, then
  resumes normally.

This is also why `disk_cache.size_gb` is impossible to tune correctly without
this gate: no static size can absorb every burst against every backend condition.

## What's shared

Since the shared-upload-worker lift, the async upload pipeline is shared
end-to-end between both products:

- **`shared_pool::PoolBudget`** + per-backend cap map — the same hard cap
  and disk-free floor apply to both products. One `PoolBudget` is
  constructed per `storage.backends` entry at startup. The YAML
  `disk_cache.size_gb` is the per-backend default; individual
  `storage.backends:` entries may override it with their own
  `disk_cache_size_gb`. Both take the `auto | <gb>` shape: an explicit
  GB integer pins the cap, or `auto` (default) derives
  `min(50% of free, max_size_gb)` floored at `min_size_gb`, recomputed
  every eviction tick. Multi-backend installs with several `auto` entries
  split the 50%-of-free share evenly.

  ```json
  {
    "version": 1,
    "backends": {
      "hot-s3":   { "type": "s3",    "bucket": "...",    "disk_cache_size_gb": 64 },
      "cold-az":  { "type": "azure", "container": "...", "disk_cache_size_gb": 8 }
    }
  }
  ```

  At startup the budget is seeded from the actual on-disk pool state so that
  a restart does not silently re-grant bytes that survived on disk (VTL's
  `refresh_from_disk`; VSA's `core_block::refresh_pool_budget_from_volumes`).
  The seed is a per-namespace breakdown — global-dedup bytes under the
  `None` bucket, each local-dedup volume / cartridge under
  `Some(uuid_hex)` / `Some(label)` — so a restart that picks up two
  namespaces' on-disk bytes can attribute them correctly in the
  monitor view.

- **Backend-wide cap, per-namespace tracking.** The hard cap and the
  backpressure semaphore are backend-wide — every reserver (no matter
  which namespace it carries) blocks on the same gate, and a release
  against one namespace wakes a waiter parked against another. The
  per-namespace counter exists for reporting only, surfaced via the
  `system monitor` job stream's `PoolEntry` payload as one row per
  (backend, namespace). Prometheus instruments stay backend-only so
  existing operator dashboards don't break (see `docs/SPEC.md` §
  Telemetry).

- **Two trigger conditions.** A reservation blocks if **either** is true:

  1. **Pool cap.** `current_bytes + payload > cap` — host writes
     outrunning backend uploads.
  2. **Disk-free floor.** `statvfs(data_dir).free < disk_free_min_bytes`
     — catches disk-fill from outside the pool (audit retention,
     manifest growth, external writers on the same partition).

  Both gates release when an eviction unlinks an uploaded chunk file and
  signals the waiter.

- **`shared/upload-worker/` crate** — `PendingUpload` /
  `UploadOutcome` payload types, `upload_chunk_inert` (stateless
  PUT + HEAD probe), `run_upload_pipeline` (bounded-concurrency
  scaffold). VTL drives the pipeline from
  `vtl/daemon/src/upload_worker.rs`; VSA drives a Semaphore-capped
  per-task fan-out from `vsa/daemon/src/upload_worker.rs`. Both
  products' per-completion hooks (legal-hold reapply on VTL, sidecar
  flip on VSA) plug into the same scaffold.

- **`shared_pool::ChunkPool`** — same content-addressed layout on both products.

- **`core_stream` / `core_block` `DiskCacheManager`** — eviction
  filter pinned on per-item "Uploaded" state (`chunks.idx`
  `LocationTag::Both` on tape; `upload.idx` `Uploaded` byte on
  block). Both products honor `disk_cache.recent_seal_pin_seconds`
  (default `0`) to optionally pin chunks whose most recent `lru.idx`
  touch — seal or read — is inside the configured window.

- **Per-cartridge / per-volume `lru.idx` sidecar** — same byte format
  modulo magic and positional key (sparse `page_id` vs monotonic
  `chunk_id`).

- **`BackpressureError` → SCSI NOT READY mapping.** On timeout the
  gate surfaces NOT READY with ASC/ASCQ `0x04 / 0x07` "LOGICAL UNIT
  NOT READY, OPERATION IN PROGRESS". Real LTO drives surface this when
  their internal buffer is overcommitted; Linux `mt-st`, `tar`, and
  every major backup product (NetBackup, Veeam, Bacula, Commvault)
  treat 0x04/0x07 as transient and retry.

The remaining divergence between the two products is confined to the cache
tier and is workload-driven — see [VSA § Why this shape
(vs VTL)](#why-this-shape-vs-vtl).

---

# VTL

On the tape side the gate sits at chunk-seal: when the active staging chunk
rolls, the seal reservation runs through the `PoolBudget` before the
staging-rename boundary. The trigger conditions and per-backend cap are
exactly as described in [§ What's shared](#whats-shared).

## Sync gate, no async ripple

The gate is a `std::sync::{Mutex, Condvar}` so that the sync chunk-seal path
(`Cartridge::seal_current_chunk`) can stay synchronous. The eviction worker
runs on tokio and calls the same sync `PoolBudget::release` after each unlink
— release simply signals the condvar — so sync and async cohabit cleanly
without the seal path needing to be aware of the async runtime.

## Drop-time `force=true` carve-out

`Cartridge::Drop` (and `flush_and_seal`) cannot return `Err` and must not lose
data. Drop-time seals therefore call `seal_current_chunk(force=true)`, which
uses `PoolBudget::force_reserve` to bypass both the cap and the disk-free
check.

This creates a bounded overshoot of at most one `chunk_max` bytes per
concurrent unload. With FastCDC defaults (32 MiB max chunk) and 3 drives,
that is at most 96 MiB of transient overshoot. The next eviction tick or
upload completion brings usage back under the cap.

## Operator knobs

```yaml
disk_cache:
  # Per-backend pool budget default (hard cap, not advisory). Two
  # shapes: an explicit GB integer pins it, or `auto` (default)
  # derives `min(50% of free, max_size_gb)` floored at min_size_gb,
  # recomputed every eviction tick. Multi-backend installs with
  # several `auto` entries split the 50%-of-free share evenly.
  # Each storage.backends entry may override via `disk_cache_size_gb`
  # (same shape).
  size_gb: auto
  min_size_gb: 4
  max_size_gb: 500
  # Warn at this fraction of the cap — early signal.
  localonly_soft_watermark_pct: 80
  # Reserve free filesystem space below which we also backpressure.
  disk_free_min_gb: 5

storage:
  upload:
    # Per-seal max wait before surfacing SCSI NOT READY.
    backpressure_max_wait_seconds: 60
```

### Tuning

- **High write rate, slow backend.** Raise `disk_cache.max_size_gb` so the
  `auto` cap can grow further on a roomy filesystem; the eviction worker
  re-derives the cap every tick. If only one backend is hot, pin it with
  `disk_cache_size_gb: 64` under its `storage.backends:` entry and leave the
  rest on `auto`.
- **Tight disk budget.** Drop `disk_cache.max_size_gb` or pin an explicit
  `disk_cache.size_gb: <n>`. Lower `disk_free_min_gb` in lockstep so
  backpressure fires earlier.
- **Short host retry window.** Lower `backpressure_max_wait_seconds` so seals
  surface NOT READY faster — more round trips, each shorter.
- **No host retry budget at all** (rare). Raise `backpressure_max_wait_seconds`.
  The seal blocks longer per call but never surfaces NOT READY.

## What backpressure does *not* do

- **Throttle individual block writes.** Only chunk-seal gates. The active
  staging chunk (≤ 32 MiB under FastCDC max) fills normally; the budget gate
  applies only when the chunk *rolls*. Per-block latency spikes would tank
  throughput.
- **Auto-scale upload concurrency.** `upload.max_concurrent` stays static. If
  upload rate is the bottleneck, raise the knob or fix the storage-side
  throttling. Backpressure makes the failure mode graceful; it does not make
  uploads faster.
- **Help if all chunks are `LocalOnly`.** Eviction can only free `Both`-state
  chunks. If nothing has uploaded yet because the backend is unreachable,
  eviction does nothing, the cap fills, and every seal eventually hits NOT
  READY — which is the correct behavior.
- **Replace operator monitoring.** The `pool_backpressure_waits_total` /
  `pool_backpressure_wait_seconds` instruments and the warn-level "Upload
  backpressure waiting" log line are the signals to watch; if either is
  sustained, add upload concurrency or fix storage throughput.

## Failure modes

### "Backup paused for hours" — uploads are stuck

If `upload.max_concurrent: 8` is configured but uploads are not completing,
check:

- Storage credentials still valid (validated at startup; mid-run rotation can
  break it).
- Storage bucket / container reachable (network, firewall, DNS).
- Provider-side throttling (S3 503 SlowDown, Azure 500 ServerBusy — the retry
  loop handles transient cases, but a prolonged throttling event keeps the
  seal blocked).
- Per-object failures (Object Lock + retention misconfiguration — PUT succeeds
  but DELETE fails on eviction, filling the pool with un-evictable chunks).

### Drop-time overshoot accumulates

If the daemon is restarted with cartridges still loaded (rare — typically you
would `cartridge unload` first), Drop-time `force_reserve` can transiently
push `current_bytes` over the cap. On restart `refresh_from_disk` reads the
actual pool state, which may already be over-budget. The first chunk-seal
blocks immediately and eviction trims back over the next few cycles. There is
no data loss, just a brief pause.

### `disk_free_min_gb` triggers but pool is fine

When this gate fires unexpectedly, look at what else is on the same
filesystem:

- `<data_dir>/audit/` if `audit.compress_rotated: false` under high traffic.
- `<data_dir>/tapes/<barcode>/manifest.json` on a 12 TB+ tape under FastCDC
  at min-chunk-size.
- External processes writing to the same partition.

`disk_free_min_gb: 0` disables this gate entirely (not recommended).

---

# VSA

On the block side the gate sits per-page in
`VolumeWriter::write_page_unsynced` (`core/block/src/uploader.rs`), between the
SBC-3 / NVMe write and the backend PUT. Without it, the pool fills the
filesystem, the next `pool.insert_bytes` hits `ENOSPC`, and the SCSI write
returns MEDIUM ERROR. With the gate, `write_page` blocks at the per-backend
cap; if uploads cannot free space within `backpressure_max_wait_seconds`, the
SBC-3 dispatcher returns NOT READY 0x04/0x07 — host filesystems retry, the
workload pauses, and then resumes.

## Gate placement

For each page, `write_page_unsynced` follows these steps:

1. `try_reserve(payload.len(), deadline)` — blocks if reserving would exceed
   the cap or take filesystem free below `disk_free_min_gb`.
2. `pool.insert_bytes(payload)` — atomic write of the chunk file.
3. On local-dedup hit (`!was_new`), `release(payload.len())` — the chunk was
   already on disk, so the reservation never consumed new bytes.
4. On `insert_bytes` error, `release(payload.len())` — backs out the
   reservation.
5. After backend upload and page-index update, the reservation stays held until
   the eviction worker releases it.

Storage upload errors do **not** release the reservation: the chunk is
content-addressed and still on disk, so budget accounting stays consistent
with reality. Orphan GC reclaims the chunk later if no page references it.

## Backpressure deadline

`disk_cache.backpressure_max_wait_seconds` (default 30). Surfaces through
`UploaderError::Backpressured` → SBC-3 NOT READY 0x04/0x07. Tune upward only
if the eviction worker's recovery latency outruns the backend PUT-then-HEAD
cadence.

## Eviction worker

`run_disk_cache_eviction_worker` ticks every
`disk_cache.eviction_interval_seconds` (default 300). For each backend it
works through the following steps:

0. **Recomputes the cap** for `auto`-mode entries: calls statvfs on
   `data_dir`, derives `min(50% of free / N_auto, max_size_gb)`, floors at
   `min_size_gb`, and pushes the result through `PoolBudget::set_cap_bytes` so
   the next `try_reserve` sees the updated ceiling. Explicit `size_gb: <n>`
   entries skip this step. The 50%-of-free share is divided evenly across all
   `auto` backends.
1. Walks every volume's `pages.idx` and `lru.idx` to build a map of
   `namespace → hash → max(last_accessed)`. `lru.idx` is touched on every
   `write_page` / `read_page`, so the timestamp reflects genuine recency.
2. Lists every pool chunk for the backend, joins to the touch map, and sorts
   ascending by `last_accessed`.
3. Removes oldest chunks via `pool.remove` until usage is under cap. Each
   remove calls `PoolBudget::release(size)`, immediately waking any
   `write_page` parked on backpressure.

Eviction skips any chunk whose pages still have a pending backend upload. The
per-volume `upload.idx` sidecar records `LocalOnly` vs `Uploaded`, and
`collect_lru_touches_and_upload_state` ANDs across every referencing page, so
a chunk shared by an uploaded page and a pending-upload page stays pinned.
This mirrors the tape side's `collect_pinned_hashes`. When a chunk is later
evicted and a read arrives for it, the refetch happens transparently in
`read_page` via `insert_verified_bytes`.

The optional `disk_cache.recent_seal_pin_seconds` knob (default `0`,
disabled) layers a second filter: chunks whose most recent `lru.idx` touch —
write OR read — is within the configured window stay pinned regardless of LRU
position. This targets verify-after-write workloads (Veeam / NetBackup re-read
freshly written pages within seconds; pure LRU would let an unrelated read
burst evict them first). The trade-off is that effective cache capacity shrinks
by the volume of write+read activity inside the window, so a tight budget under
sustained load can see "all candidates pinned" warnings. The 0 default is
validated before RC/GA against a workload trace (see `ROADMAP.md`).

### Ghost-list telemetry — `cache_miss_after_eviction_seconds`

Sizing the cache and choosing a value for `recent_seal_pin_seconds`
both come down to the same question: *of the cache misses operators
see, how recently were those chunks evicted?* If most are evicted
within the last few seconds, the cache is undersized by that window
and either a bigger `size_gb` or a non-zero `recent_seal_pin_seconds`
would have caught them. If most are evicted minutes or hours ago,
the misses are organic — a bigger cache wouldn't help cost-effectively.

A per-backend **ghost list** answers this question without storing any
per-chunk state. The eviction worker maintains a bounded ring of
recently-evicted `(chunk_hash, evicted_at_unix)` tuples. On every
cache miss that triggers a backend GET, the read path consults the
ring; if the hash is found, `now - evicted_at` is recorded into the
`thurv{tl,sa}_cache_miss_after_eviction_seconds` histogram, labelled by
backend. The ring is measurement-only — it never participates in
cache replacement decisions (no ARC), so its only failure mode is
losing telemetry resolution.

Ring capacity is `disk_cache.ghost_ring_size` (default 100,000 per
backend, ~10 MB at ~100 B/entry). Under sustained heavy eviction the
ring's effective time window narrows to "the last N evictions" rather
than "the last 256 s"; that case piles overwhelming signal into the
low buckets anyway, so the lost tail is not actionable.

The histogram's buckets are explicit and log-uniform: `1, 2, 4, 8,
16, 32, 64, 128, 256, +Inf` seconds. Reading it:

- **Mass below 60 s** → cache undersized by roughly that window.
  Either bump `disk_cache.size_gb` to give LRU headroom, or set
  `disk_cache.recent_seal_pin_seconds` to that value (the temporal
  guarantee equivalent, at the cost of backpressuring writes when
  the pin can't free).
- **Mass past 256 s** → organic misses; a bigger cache won't help.
- **`+Inf` bucket dominates** → the ring is too small for the
  current eviction rate, or the workload genuinely re-reads
  long-cold data. Check eviction throughput before bumping
  `ghost_ring_size`.

## `lru.idx` sidecar

Per-volume `<volume_dir>/lru.idx` — sparse 8-byte-per-`page_id` file
(`CSLI` magic header). Touched on every `write_page` / `read_page`.
Local-only (never uploaded), rebuilds as zeros on corrupt header.
Format at `core/block/src/lru_index.rs`. Mirrors the tape side's
`core/stream/src/lru_index.rs` byte-for-byte modulo magic / positional
key (sparse `page_id` vs monotonic `chunk_id`).

## `upload.idx` sidecar

Per-volume `<volume_dir>/upload.idx` — sparse 1-byte-per-`page_id`
file (`CSUI` magic header). Records the async-upload state:

- `0x00 = Uploaded` (also: unallocated, legacy default for volumes
  created before async upload landed).
- `0x01 = LocalOnly` (pool has the chunk, backend PUT pending).

`VolumeWriter::write_page_unsynced` flags `LocalOnly` before enqueuing the
[`UploadTask`]; the daemon's upload worker
(`vsa/daemon/src/upload_worker.rs`) calls
`VolumeWriter::apply_page_upload_outcome` on completion, which flips back to
`Uploaded` and wakes any `PageCache::synchronize_bytes` waiter parked on the
page range. The eviction filter consults the sidecar to skip pinned chunks.

Local-only; corrupt header rebuilds empty — every page reads as `Uploaded`,
the safe pre-async invariant. Format at `core/block/src/upload_index.rs`.

## Why this shape (vs VTL)

VSA and VTL share the **upload pipeline** end-to-end — both seal to a local
pool, hand off to an `mpsc`-driven async upload worker, and track per-item
upload state so eviction cannot drop an un-uploaded chunk (see
[§ What's shared](#whats-shared)). The remaining divergence sits one layer
up, at the cache tier, and is entirely workload-driven.

### Cache tier — divergence

VSA's application-level RAM cache (`core_block::PageCache`, default 64 MiB /
1024 pages per volume at the 64 KiB page size) carries three SBC-3
responsibilities that have no tape analogue:

1. **Sub-page RMW.** SBC-3 sector grain is 4 KiB; page grain is 64 KiB. A
   4 KiB host WRITE must merge into a 64 KiB page; without a RAM page the
   merge costs a pool/backend read plus write on every sub-page operation —
   fatal for random DB or filesystem workloads.
2. **COMPARE AND WRITE atomicity.** SBC-3 CAW is per-page atomic. Serializing
   it on an in-RAM page under a `tokio::sync::Mutex` is straightforward; doing
   it via pool files would need an atomic-rename and file-lock protocol.
3. **WRITE SAME / UNMAP at page grain.** Page-shaped operations want a
   page-shaped abstraction in front of the chunk pool.

VTL faces none of these. Tape WRITEs are unit-sized sequential appends — no
RMW, no CAW, no atomic ops. The staging chunk file is the right buffer and the
kernel page cache provides RAM-side aggregation for free. The RAM-cache budget
is a fixed tunable, not a function of volume size: a 100 TiB volume serving a
4 GiB hot working set wants approximately 4 GiB of cache.

### SYNCHRONIZE CACHE drain — per-volume durability tier

`PageCache::synchronize_bytes` honours a per-volume [`SyncAfter`] tier — set
at `volume create --sync-after <MODE>`, mutable at runtime via
`volume modify --sync-after <MODE>`, persisted in `runtime.json`, and
hot-cached as an `AtomicU8` on the [`VolumeWriter`] so the SYNC fast path is
lock-free.

| Tier | SYNC blocks until | Survives | Lost on |
|---|---|---|---|
| `storage` (default) | bytes are in backend object store | host-disk loss / daemon-process crash / power loss | provider outage during the upload window |
| `disk` | bytes are in the local pool file | daemon-process crash / power loss (if pool is on stable storage) | host-disk loss |
| `memory` | (no-op — bytes are in RAM cache only) | nothing | any crash; only the periodic flush worker tick eventually drains |

Implementation in `PageCache::synchronize_bytes`:

- `Storage` (default) — `flush_pages_in_range` drives every dirty RAM page
  through `VolumeWriter::write_page_unsynced` (pool insert + enqueue to the
  upload worker), then `VolumeWriter::pending_uploads().wait_for_range(...)`
  blocks until every page in the range has had its upload acked. A host
  `fsync(2)` settles to "bytes are in the storage backend."
- `Disk` — `flush_pages_in_range` only; skip phase 2. Host `fsync(2)` settles
  to "bytes are in the local pool." Faster; loses on daemon-host disk failure
  before the worker drains.
- `Memory` — no-op. Dirty pages stay in the RAM cache until the periodic flush
  worker tick or eviction-induced flush drains them. Host `fsync(2)` returns
  immediately. This is the ZFS `sync=disabled` equivalent.

**Live-mutable, silent to the SCSI initiator.** A `volume modify --sync-after
<MODE>` flip updates the writer's atomic and rewrites `runtime.json` so the
next boot picks the new tier. The flip takes effect on the next SYNC;
in-flight SYNCs finish under the mode active when they started. The contract
change is **not signalled to the host** — an fsync-heavy workload silently
gains or loses durability; pair flips with workload-level awareness.

VTL has no chunk-grain SYNCHRONIZE CACHE equivalent; tape backup software's
durability boundary is cartridge unload, handled via
`MemoryBufferManager::on_cartridge_unloaded` draining the same upload queue.

### Principle

Align where the workloads are similar (storage, dedup, storage tiering, async
upload, eviction); diverge where the SCSI semantics differ (sequential
streaming vs random RMW with atomic ops). What remains in VSA-specific code is
the RAM cache, the SYNCHRONIZE CACHE drain hook, and the per-page sidecar
format.

## What can go wrong

- **Pool budget too small for the burst.** Periodic backpressure waits will
  appear in the daemon log and in the `thurvsa_pool_backpressure_wait_seconds`
  histogram. On `auto`, raise `disk_cache.max_size_gb`; on explicit, raise
  `disk_cache.size_gb`. Either knob can also be set per-backend via
  `disk_cache_size_gb` on each `storage.backends:` entry.
- **Eviction-interval too long.** The steady-state cache stays at the ceiling
  longer than necessary; new pages park on backpressure even though older pages
  could have been evicted. Lower `disk_cache.eviction_interval_seconds`.
- **Filesystem fills from outside the pool.** Audit retention or external
  writers on the same partition will trigger the disk-free gate. Raise
  `disk_cache.disk_free_min_gb` so the gate fires before the partition is
  full.

## Telemetry

Same per-backend instruments as VTL, prefixed `thurvsa_*` (sourced from
`shared_naming::DISK.metric_prefix`):

- `thurvsa_pool_used_bytes{backend}` — gauge of current pool bytes.
- `thurvsa_pool_cap_bytes{backend}` — gauge of the configured cap.
- `thurvsa_pool_backpressure_waits_total{backend}` — counter of parking
  events.
- `thurvsa_pool_backpressure_wait_seconds{backend}` — histogram of wait
  durations (seconds).
- `thurvsa_cache_miss_after_eviction_seconds{backend}` — histogram of
  `now - evicted_at` for cache misses whose chunk had been recently
  evicted from this backend's pool. Drives `disk_cache.size_gb` and
  `disk_cache.recent_seal_pin_seconds` sizing. Explicit log-uniform
  buckets `1, 2, 4, 8, 16, 32, 64, 128, 256, +Inf`. See § *Ghost-list
  telemetry* above.
