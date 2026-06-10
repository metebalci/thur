# Deduplication

Both applications store data as chunks in a shared content-addressed pool
(`shared-pool`), so the deduplication mechanism is common across VTL
and VSA. What differs is how each application cuts its byte stream into
chunks and what scope the deduplication spans. This document covers the
shared mechanism first, then the per-application specifics.

- **[VTL](#vtl)** — sealed tape chunks, FastCDC / fixed chunking, the
  `--dedup` scope per cartridge, encryption / compression interactions.
- **[VSA](#vsa)** — fixed 64 KiB volume pages, the `--dedup` scope per
  volume, the volume-encryption interaction.

This is the single explanation of how deduplication works. Other docs
(`README.md`, `docs/reference/SPEC.md`, `CLAUDE.md`,
`dist/thurvtl.defaults.yaml`) link here rather than repeating it.

## The shared mechanism

### Content-addressed pool

Every sealed chunk is addressed by the **BLAKE3 hash of its on-disk
bytes**. The chunk store (`shared_pool::ChunkPool`) is content-addressed,
which means identical bytes from any source always land at the same
filesystem path and the same object key. Duplicate data is therefore
stored exactly once, wherever it first appears. The unit of deduplication
is the **whole sealed chunk** — nothing smaller than a chunk participates
in dedup on its own.

There is **no central index or database**. The hash is the address at
both the local and backend layers; checking whether a chunk already exists
is a filesystem `stat` call locally and a `HEAD` request against the
storage backend. The pool path includes a two-level `<aa>/<bb>` shard
prefix — the first two and next two hex characters of the hash — which
creates a 65,536-way fanout. That fanout keeps individual leaf directories
small enough that `readdir` remains fast during garbage collection and
manual inspection, even on a heavily loaded pool.

### Two stages — local pool, then storage backend

Deduplication fires at two independent stages, one synchronous at chunk
seal time and one asynchronous in the upload worker.

**Stage 1 — local pool (synchronous, at chunk seal).** When a chunk
seals, the daemon finalizes its BLAKE3 hash, derives the pool path
`<data_dir>/chunks/<backend>/[<namespace>/]<aa>/<bb>/<hash>.dat`, and
checks whether that file already exists:

- **Hit** → the staged copy is dropped, and the existing pool file is
  referenced instead. (`ChunkPool::insert_bytes` returns `was_new = false`;
  `insert_from_path` deletes the source.)
- **Miss** → the staged chunk is atomically moved into the pool via
  `rename`.

The filesystem itself serves as the lookup table. Two writers racing on
stage 1 is benign — both produced byte-identical chunks, so whichever
`rename` wins, the final file is correct.

**Stage 2 — storage backend (asynchronous, in the upload worker).** When a
sealed chunk reaches the upload worker, the daemon issues a `HEAD` on the
object key before attempting a `PUT`:

- **HEAD 200** → some other cartridge or volume on this backend already
  uploaded this chunk. The `PUT` is skipped, and the chunk is recorded
  as uploaded.
- **HEAD 404** → the chunk is new to the bucket and is uploaded normally.

The bucket key is the hash, so checking existence is simply asking whether
that key responds to `HEAD`. No separate metadata service is involved.

**The two stages are independent.** A chunk can be a stage-1 local hit
and still require a `PUT` if no peer has uploaded it to the bucket yet.
Conversely, the local copy may have been evicted while the backend copy
survives; a new producer of identical bytes will then find the chunk via
the stage-2 HEAD probe. Under the `local` dedup scope, the object key
includes a per-unit namespace, so a stage-2 HEAD is guaranteed to miss
against another unit's chunks — the upload worker therefore skips the
probe entirely for locally-scoped chunks.

### Dedup scope — `local` vs `global`

The deduplication mechanism is always active — chunks are
content-addressed, so collisions always collapse within whatever scope is
configured. The **scope** is a sticky, per-unit choice
(`shared_object_store::DedupScope`, set at create time, recorded in the manifest,
and never changed thereafter):

| Scope | Pool path | Object key | Cross-unit sharing |
| --- | --- | --- | --- |
| `global` | `chunks/<backend>/<aa>/<bb>/<hash>.dat` | `chunks/<aa>/<bb>/<hash>.dat` | Yes — with other `global` units on the same backend |
| `local` | `chunks/<backend>/<namespace>/<aa>/<bb>/<hash>.dat` | `chunks/<namespace>/<aa>/<bb>/<hash>.dat` | None — chunks isolated per unit |

`<namespace>` is the cartridge barcode (VTL) or the volume UUID hex
(VSA). Cross-unit deduplication fires only when both units are `global`
**and share a backend** — each backend's bucket holds an authoritative
copy, so refetch-on-eviction is unambiguous. Two `local` units, or units
on different backends, never share pool files even when their content is
identical.

The `local` scope trades storage savings for clean isolation. A shared
chunk couples its referrers: it cannot be evicted while any unit still
pins it, garbage collection must take the union of every manifest, a
corrupt block fans out to every referrer simultaneously, and a
per-object provider primitive such as a legal hold or retention lock
affects every unit that references it. Because these trade-offs play out
differently for tape versus block workloads, **the two applications default
to opposite choices** — see the per-application sections below.

### Refcount-aware eviction

The local pool is a warm cache; the backend copy is the source of truth.
When a per-backend disk-cache budget is exceeded, the eviction worker
removes least-recently-used chunks — but only chunks whose backend copy is
already durable, and only chunks that no other unit still pins locally.

Eviction is **refcount-aware and per-namespace**. Before unlinking a
pool file, the cache manager scans every same-backend manifest and
page-index for a still-local reference to that hash:

- `global` candidates check pins across every `global` unit of the
  backend (the shared pool).
- `local` candidates check only the single unit that owns the namespace.

A chunk evicted from one unit's view may still be pinned by another; the
union of all referrers determines whether the file can be removed.
`PoolBudget`'s boot-time refresh counts both pool layouts so that a
daemon restart never silently re-grants bytes to the local-scope pool.
The eviction worker itself — budget recompute, the LRU walk, releasing
the backpressure gate — is described in [`BACKPRESSURE.md`](BACKPRESSURE.md).

### Garbage collection

Eviction reclaims disk space for chunks that are still live but whose
backend copy makes them safe to drop locally. **Garbage collection** does
something different: it reclaims chunks that are no longer referenced by
anything — orphaned when a page is rewritten to new content, or when a
unit is deleted. GC walks every manifest and page index to build the
live set of `(backend, namespace) → {hash}`, then sweeps both pool
layouts (and, on request, the storage backends), removing anything not in
that live set. The command surface differs per application — see below.

---

# VTL

The deduplication unit is a **sealed tape chunk**. A cartridge streams
host writes into a `.staging/` chunk file that seals into the
content-addressed pool when it rolls (hitting a size threshold) or when
the cartridge unloads (`Cartridge::Drop` → `flush_and_seal`).

## The `--dedup` scope (per cartridge)

```
thurvtl cartridge create BARCODE [--dedup local|global]
```

`--dedup` is sticky for the cartridge's lifetime — the manifest carries a
`dedup: "local"|"global"` field, set at create time and never changed. The
CLI default is **`global`** (`default_cartridge_dedup`); the `local`
namespace is the cartridge barcode.

Cross-cartridge dedup fires only when both cartridges are `global` and
share a backend. `local` cartridges each get a per-barcode subtree and
never share pool files with any other unit, even when the content is
identical. This makes `local` useful for compliance and tenant separation,
or for stress-testing the write path in isolation without any
dedup-related coupling.

## Chunking modes

The chunking mode is picked at cartridge create time and is sticky for
the cartridge's lifetime:

```
thurvtl cartridge create BARCODE \
  --chunking fixed|fastcdc \
  --chunk-size-mb N
```

### Fixed (`--chunking fixed`)

Every chunk is exactly `--chunk-size-mb` megabytes. Simple and
predictable, but a single-byte shift anywhere in the input changes the
hash of every downstream chunk, collapsing cross-backup dedup entirely.
Fixed chunking is useful only when input streams are byte-for-byte
identical between backups.

### FastCDC (`--chunking fastcdc`, default)

Content-defined chunking via a Gear-hash rolling window with normalized
strict and loose masks. The `--chunk-size-mb` value is the target average;
`min ≈ avg/8` and `max ≈ avg×4` are derived automatically. The default
average of 1 MiB yields a `1 MiB / 8 MiB / 32 MiB` min/avg/max range.

FastCDC means dedup ratios survive **whole-block shifts** — for example, a
new file added to a tar stream that pads to the tar block size. The
rolling Gear hash re-converges to the same state roughly one chunk after
the shift point, so downstream chunks match the corresponding chunks in
the previous backup and still dedup.

### Block-aligned approximation (current limitation)

VTL's content-defined cut points fire only at SCSI block boundaries. A
single `BlockIndex` record always lives within one chunk; the manifest
schema does not support segment lists. The practical consequence is:

- **Whole-block shifts** (tar padding to block size, backup tools that
  preserve block alignment) survive — this is the common case.
- **Sub-block shifts** (a single-byte insertion mid-block) still break
  dedup: every block past the shift contains different bytes, so
  block-aligned chunks rooted at those blocks hash differently.

Lifting the sub-block limitation would require `BlockIndex` to become a
`Vec<Segment>` so that a logical block can span multiple chunks. This has
been evaluated and **declined** for these reasons (encrypted chunks
dedup near-zero anyway, zstd
captures most savings, cold-backend read seams double, AME ordering becomes
trickier, manifest grows O(segments), and the correctness surface
expands). Revisit only for a workload that is plaintext, highly redundant
under sub-block shifts, and not already covered by compression.

### Picking parameters

8 MiB average is a reasonable starting point — small enough for
cross-backup dedup to fire reliably, large enough to keep S3 PUT counts
manageable on a 12 TB tape (roughly 1.5 million chunks in the worst case).
Smaller average = more dedup opportunities but more backend objects. Larger
average = fewer objects but less dedup sensitivity to boundary shifts.

## Interactions

### Drive-side compression × dedup

Drive compression (LTO Mode Page 0x0F DCE bit) runs **before**
encryption, operating per block. It significantly reduces cross-cartridge
dedup ratios because compressed bytes are highly sensitive to surrounding
context — a tiny shift in the source produces wildly different compressed
bytes, preventing the content addresses from matching. The recommended
setting is to leave drive compression off (it is off by default; real LTO
drives also ship with DCE off) and let the post-dedup backend-compression
layer do the work instead.

### Encryption (AME) × dedup

Drive-level encryption (AES-256-GCM) uses a **per-block IV**. Identical
plaintext blocks produce different ciphertext under different IVs, so
chunks containing encrypted blocks essentially never deduplicate.
Plaintext and encrypted cartridges can coexist in the same library; dedup
fires only for plaintext chunks. This is the documented trade-off of
host-managed encryption — most deployments under HIPAA, PCI, SOX, or GDPR
get effectively zero dedup benefit from AME and rely on compression for
storage savings instead.

### Appliance-side at-rest encryption × dedup

Per-cartridge AES-256-GCM encryption at the chunk-seal boundary is
opt-in (`cartridge create --encrypt --keystore NAME`). The entire sealed
chunk becomes a single ciphertext envelope under a per-cartridge DEK and
a per-chunk IV (`derive_iv(uuid, chunk_id, 0)`), which means the pool hash
is computed over ciphertext rather than plaintext.

- **Cross-cartridge dedup is defeated** — two cartridges with different
  DEKs encrypting identical plaintext produce different ciphertext and
  therefore different pool hashes, so no collision occurs.
- **Within-cartridge dedup is also defeated** — chunk IDs are monotonic
  (every chunk gets a fresh IV under the same DEK), so even a chunk
  containing the same bytes as an earlier chunk produces different
  ciphertext.

This is the same trade-off VSA pays for volume encryption. Plaintext
cartridges continue to dedup normally; mixed libraries work fine.
Operators who need both at-rest custody and effective deduplication should
consider **bucket-level SSE** (S3 SSE-KMS, GCS CMEK, Azure CMK): the
provider encrypts opaquely, the daemon's dedup hashes are computed over
plaintext and survive across cartridges, and the key management chain
lives in the provider's KMS. The appliance-side layer is for shops where
the bucket-key model is not sufficient — zero-trust against the storage backend
provider, or HSM-backed KMIP custody chains.

Server-side copy offload (VAAI XCOPY / Hyper-V ODX) upholds this boundary
rather than punching through it. Those primitives normally copy by
*hash-rebind* — pointing the destination's page-index slot at the source's
existing chunk — which for encrypted data would bind the destination to
ciphertext it cannot decrypt (the IV is keyed on the source's `dek_uuid` +
`page_id`). VSA therefore rebinds only when the destination can reconstruct
the chunk's (key, IV): both volumes unencrypted, or both encrypted under the
same crypto identity (`dek_uuid`) with the page at the same offset
(`core_block::rebind_is_sound`). Otherwise it *recrypts* — decrypt under the
source identity, re-encrypt under the destination — so the offloaded copy
still lands under the destination's own DEK and never shares a chunk across
crypto identities (issue #88). A snapshot/clone, which inherits the source
DEK + `dek_uuid`, keeps the zero-copy rebind and so still shares chunks with
its source family.

### Backend-side compression × dedup

Backend-tier compression (`storage.compression.algorithm`,
`storage.compression.level`) runs **post-dedup**: by the time the upload
worker pulls bytes from a chunk and compresses them for the bucket, the
chunk has already sealed and its hash already exists. Dedup decisions are
therefore made on the on-disk plaintext bytes, and storage-side compression
does not interfere with them. The compression format used is stored in
object metadata (S3 object metadata, GCS custom metadata, Azure blob
metadata) so the download path knows how to decompress. The default is
zstd level 3; switch to lz4 if throughput matters more than compression
ratio.

The marker records only the **algorithm** (`zstd` / `lz4` / `none`), not
the level. The level governs how hard the encoder works, never how the
decoder reads the result — a zstd or lz4 frame is self-describing, so
decompression takes the algorithm alone. The `local` backend never
compresses, so it stores no marker at all (a plain filesystem has no
object-metadata facility, and there is nothing to record).

That marker is a fast-path *hint*, not the ultimate source of truth —
because the pool is content-addressed, the chunk's BLAKE3 hash is. If a
chunk's compression metadata were ever lost (say an out-of-band tool
rewrote the object without carrying its metadata across), the chunk stays
both safe and recoverable. Safe, because the read path verifies the
decompressed bytes against the expected hash before admitting them to the
pool: a missing marker can never silently yield wrong data — at worst it
surfaces as an integrity error, never corruption. Recoverable, because the
algorithm can be read back from the bytes themselves: zstd and lz4 frames
each begin with a distinct 4-byte magic (`0x28B52FFD` and `0x184D2204`),
so a recovery tool can spot a compressed chunk on sight, and the one
genuinely ambiguous case — uncompressed data carries no magic and could,
at scale, coincidentally begin with one — is settled by trying each of the
three candidates and keeping whichever decompresses to the expected hash.
There are only three, and collision resistance guarantees exactly one
matches.

This stays a documented recovery procedure, not an automatic fallback on
the hot read path: the live decode trusts the single explicit marker and
stays O(1). Magic-sniffing alone cannot replace the marker — it cannot
distinguish an uncompressed chunk from a compressed one without that hash
round-trip. The same reasoning rules out encoding the algorithm in the
object key's extension (e.g. `<hash>.zst`): a hash-derived key has to stay
independent of compression, or the dedup HEAD-probe would have to glob
across extensions and a mid-life config change would silently store
duplicates.

## Garbage collection

`thurvtl system gc [--dry-run] [--storage]` walks every
`manifest.json` under `<data_dir>/tapes/` and groups each `chunks[].hash`
by `(manifest.backend, namespace)` — `None` for `global`-scope cartridges
(the shared pool) and `Some(barcode)` for `local`-scope cartridges. For
each configured backend it sweeps both layouts: the shared pool against
the backend's `(_, None)` live set, and every `local` namespace against
that namespace's `(_, Some(barcode))` live set. Orphan namespace
directories left by deleted cartridges have an empty live set, so every
chunk in them is reclaimed and the empty directory removed. `--storage` does
the same for each storage backend. `--dry-run` previews what would be
removed without deleting anything.

## Analytics

Both the dedup mechanism and its analytics are always on. The dedup
ratio, per-cartridge contribution, and pool-vs-backend upload-skip rates
surface as Prometheus metrics and through `thurvtl system stats`.

---

# VSA

The deduplication unit is a **fixed volume page** — default 64 KiB,
configurable at `volume create` via `page_size_bytes` (must be a power of
two and a multiple of the 4 KiB sector size). There is **no
content-defined chunking** on the block side: a page is hashed and sealed
as a whole unit, one page to one pool chunk.

## Fixed-page chunking — why no CDC

VSA serves random-access block volumes. A host write lands at an arbitrary
LBA; the page that contains it is a fixed, position-indexed slot
(`page_id = LBA / page_size`). Content-defined chunk boundaries would have
nothing to anchor to — there is no append-only stream to re-converge over
— and a page must stay addressable by position for sub-page
read-modify-write to work. VSA therefore uses fixed pages exclusively; the
FastCDC machinery is VTL-only.

Fixed pages mean VSA dedup catches only **page-aligned identical content**
— two volumes (or two different LBAs within the same volume) whose page
slot is byte-for-byte the same. It does not survive a sub-page shift. For
the random-access block workload that is the right trade-off: such volumes
rarely contain large runs of shifted-but-identical data, and fixed pages
are a prerequisite for RMW anyway.

## The `--dedup` scope (per volume)

```
thurvsa volume create NAME [--dedup local|global]
```

VSA uses the same `shared_object_store::DedupScope` as VTL, recorded in the
manifest's `dedup_scope` field and sticky for the volume's lifetime. The
CLI default is **`global`** — matching VTL — and the `local`
namespace is the volume UUID hex. Using the UUID as the namespace is
deliberate: a `volume destroy` followed by a recreate under the same name
mints a fresh UUID, so the new volume never inherits the destroyed one's
namespace or its orphan chunks.

VSA defaults to `global` because the dominant block workload — many VMs
or containers cloned from a shared golden image — has large runs of
identical page-aligned content (base OS, shared libraries, package
caches) that cross-volume dedup collapses into one pool copy per chunk.
Operators who want a volume isolated from the shared pool (so a single
chunk is never pinned by another volume, and orphan reclamation is
strictly per-volume) can pass `--dedup local` at create time; the
fresh-install case where boot sectors, partition tables, and filesystem
superblocks differ per volume is the workload where that isolation
costs little.

## Volume encryption × dedup

VSA volume encryption is **opt-in per volume** (`volume create --encrypt
--keystore NAME`), using AES-256-GCM. When a volume is encrypted, the page
is encrypted **before** it is hashed — the pool stores ciphertext, and the
content address is computed over ciphertext.

The per-page IV is derived from `(crypto_uuid, page_id, iv_salt)`, where
`crypto_uuid` is the volume's crypto identity and `iv_salt` is a fresh
random per-seal value stored in the page's `pages.idx` record (issue #87,
to eliminate AES-GCM nonce reuse). The consequences for dedup are:

- **Cross-volume dedup is defeated** — two encrypted volumes with
  different DEKs (distinct `crypto_uuid`) produce different ciphertext for
  the same plaintext page, so no pool collision occurs.
- **Cross-page dedup within a volume is defeated** — the IV is bound to
  `page_id`, so the same plaintext at two different page positions encrypts
  to different ciphertext.
- **Even an idempotent rewrite is defeated** — a fresh `iv_salt` per seal
  means rewriting a page with identical content at the *same* `page_id`
  produces a new IV → new ciphertext → new pool chunk (the superseded one
  becomes a GC orphan). An encrypted volume therefore sees **no** dedup at
  all; this is the deliberate cost of guaranteeing nonce uniqueness.

This is the same content-vs.-key tension as VTL's AME, taken to its limit.
Plaintext volumes dedup normally within their scope; for at-rest custody
combined with meaningful dedup, the right approach is bucket-level SSE.

## Eviction & GC

VSA's refcount-aware eviction and the per-volume `lru.idx` and
`upload.idx` sidecars that drive it are covered in
[`BACKPRESSURE.md`](BACKPRESSURE.md) § VSA. The dedup-relevant point is
that a chunk is evictable only once its backend copy is durable and no
volume still pins it locally — the same union rule as VTL, with each
volume's `pages.idx` standing in for the cartridge manifest.

Orphan chunks accumulate when a page is rewritten to new content (the
superseded chunk lingers until nothing references it) or when a volume is
destroyed — `volume destroy` removes the manifest and page index but
leaves pool chunks behind. `thurvsa system gc` reclaims them: it walks
every volume's `pages.idx` **and every snapshot's frozen `pages.idx`**
into a live `(backend, namespace) → {hash}` set and removes pool chunks
not in that set, including every chunk under a destroyed volume's
now-orphan namespace directory. `--dry-run` reports what would be deleted
without touching anything; `--storage` extends the sweep to the storage
backend's `chunks/` objects. The verb mirrors VTL's `thurvtl system gc` —
daemon-routed, runs alongside live traffic, audited as `gc.run`.

## Snapshots + clones

Snapshots and clones (issue #13) share chunks rather than copying them,
and lean entirely on the GC arithmetic above. A snapshot is a frozen copy
of a volume's `pages.idx`; a clone is a new writable volume seeded with a
copy of one. Both reference the source's existing pool chunks, so the
shared bytes are stored once.

For `Global`-dedup volumes nothing special is needed — the pool is shared
per-backend, so a snapshot/clone resolves the same chunks as the source.
For `Local` dedup, where each volume's chunks live under a namespace keyed
on its UUID, a snapshot/clone would otherwise derive a *different*
namespace from its own UUID and fail to find the shared chunks. So a
snapshot/clone inherits the source's **family namespace** via the
manifest's `dedup_namespace` field (the snapshot records it directly; a
clone carries it forward): the whole snapshot/clone family — origin,
snapshots, and clones — resolves to one `Local` pool namespace.

GC keys each member's hashes on that family namespace, so they union into
one `(backend, namespace) → {hash}` bucket. A chunk shared across family
members is therefore counted once and reclaimed only when **no** member
references it. Concretely: overwrite a page on the origin and its old
chunk is retained as long as a snapshot or clone still maps to it; destroy
that snapshot (and any clone that diverged from the page) and the next GC
reclaims the now-unreferenced chunk. The single on-disk namespace
directory is torn down only when the entire family is gone.
