# Disaster Recovery, Migration & Archive

Operator procedures for moving cartridges between storage backends and
for recovering a library from a cold mirror. All four are Thur VTL verbs:

- **`library restore`** — cross-region DR: bring a fresh host up from a
  cold mirror bucket (daemon-down).
- **`cartridge migrate`** — move one cartridge from one backend to
  another (daemon-routed job).
- **`cartridge archive`** — snapshot a cartridge onto another backend as
  a frozen, self-contained blob (daemon-routed job).
- **`library restore-archive`** — pull a frozen archive back into a live
  cartridge (daemon-routed job).

The on-disk and storage-backend layouts these build on are in
[`SPEC.md`](../reference/SPEC.md) (§ Object Layout, § On-Disk Layout).

## Cross-region DR — `thurvtl library restore`

This verb brings a fresh host up from a cold mirror bucket. It can
only replicate what the bucket actually holds, which is the cartridge
state — manifests, index pages, and chunks. The chassis topology in
`library.json` is **not** storage-replicated, so the operator has to
declare it themselves in the new host's `thurvtl.yaml` `library:`
block (and start the daemon once to materialize it). Restored
cartridges are seated into storage slots sequentially in barcode-sort
order; if a specific layout matters, run `changer move` afterward.

The command is daemon-down — the daemon must not be running:

```
thurvtl library restore --backend NAME
                            [--barcodes B1,B2,...]
                            [--dry-run]
                            [--allow-existing]
```

- `--backend NAME` — required when `storage.backends:` declares more
  than one backend; inferred when exactly one is configured.
- `--barcodes` — optional comma-separated allowlist; default is every
  barcode whose `manifest-latest.json` sentinel is reachable under
  `manifests/`.
- `--dry-run` — lists what would be restored without writing anything
  under `<data_dir>/tapes/`. No audit entry; no inventory mutation.
- `--allow-existing` — skip a barcode whose local cartridge directory
  already exists. Without this flag, a pre-existing local directory is
  a fatal per-cartridge error.

### Phases

The restore runs in four phases:

1. **Discovery.** `ObjectStoreBackend::list_objects("manifests/")`
   enumerates every key under that prefix. A barcode that has a
   `manifest-latest.json` sentinel is kept; anything without one — an
   in-flight upload, a torn write — is surfaced as an orphan hint
   rather than restored.
2. **Per-cartridge restore.** Each selected barcode goes through the
   single-cartridge cold-bucket path
   (`Cartridge::open_with_storage_async` → the missing-locally branch of
   `load_manifest_async`), which fetches `manifest-latest.json`, then
   every index page enumerated in `index_epoch`, and writes both to
   `<data_dir>/tapes/<barcode>/`. A failure on one cartridge does
   **not** abort the batch — every cartridge is attempted and its
   outcome reported individually. Chunks are *not* downloaded in this
   phase; they lazy-load on the first host read, through
   `read_block_async`'s storage-refetch path.
3. **Inventory rebuild.** The cartridges that restored successfully,
   sorted by barcode, are seated into storage slots via
   `Library::add_or_create_tape` — which short-circuits its create
   path when the cartridge directory already exists, leaving only the
   slot assignment to do. If the cartridge count exceeds the free slot
   count the restore refuses, and the error names the exact remediation
   (`--slots >= N`).
4. **Audit footprint.** Each invocation writes one `library.restore`
   audit entry, queued under `<audit_dir>/pending/` and replayed into
   the chain on the next daemon start. It is suppressed on
   `--dry-run`. The payload carries `backend`, `discovered`,
   `selected`, `restored`, `failed`, `skipped_existing`,
   `filtered_out`, and `allow_existing`.

### Operator runbook (cold-bucket DR)

On a fresh host, a cold-bucket recovery is three steps:

```
# 1. Declare chassis topology in thurvtl.yaml (operator's call,
#    not storage-replicated):
#    library:
#      num_slots: N
#      num_drives: M
#      lto_generation: 7|8

# 2. Bring the daemon up briefly so it materializes library.json
#    from the YAML, then stop so the daemon-down restore can run.
systemctl start thurvtld
systemctl stop thurvtld

# 3. Restore cartridges from the mirror.
thurvtl library restore --backend mirror

# 4. Start serving.
systemctl start thurvtld
```

This assumes the storage provider has already replicated the source
bucket to the mirror region out-of-band — S3, GCS, and Azure all offer
bucket-level cross-region replication. Thur VTL itself does not drive
cross-bucket replication; that is a separate feature ("cartridge
replication", issue #14).

### Exit codes

- 0 — every selected cartridge restored, inventory rebuilt cleanly.
- 1 — at least one cartridge failed to restore, or the slot-overflow
  guard fired (cartridge count > library's free slot count). Audit
  entry is recorded with the failure detail.

### What this verb does NOT cover

- **Chassis topology.** The operator declares the chassis in the
  YAML `library:` block first; the topology fields (`num_slots`,
  `num_drives`, `lto_generation`) are never pulled from storage state.
- **Daemon-routed warm-host restore.** `library restore` refuses to
  run if a daemon is alive on the data dir. Refreshing a single
  cartridge's metadata against a live daemon is a different operation
  altogether — closer to `system verify --repair`, which is not
  currently shipped.
- **Eager chunk pre-fetch.** The restore is metadata-only by design;
  the first host read is what pulls chunks in.
- **App-driven cross-region mirroring** — synchronous writes to two
  buckets. That is the cartridge-replication feature (issue #14).

---

## Cartridge migration — `thurvtl cartridge migrate`

Migration moves a single cartridge from one storage backend to another.
The cartridge keeps its barcode and its logical identity — the only
field that actually changes is `manifest.backend`. It runs as a
daemon-routed admin job (kind `cartridge.migrate`), backed by
`core_stream::cartridge_migrate::run_migrate`.

There are two modes:

- **`move`** (the default) — copy the chunks and manifest backups from
  the source to the target, flip the manifest, then delete the source
  objects.
- **`rebind`** — a pointer rewrite only. It HEAD-verifies the target
  before the flip (unless `--no-verify`) and never touches the source.
  This mode is for operators who already run bucket-level replication
  out-of-band.

### CLI surface

```
thurvtl cartridge migrate <BARCODE> --target-backend <NAME>
    [--mode move|rebind]   (default: move)
    [--no-verify]          (rebind only; skip HEAD pass)
    [--dry-run]
```

### Move mode — phases

1. **Discover chunks.** Walk `chunks.idx`; every record that has a
   hash contributes one `(hash, object_key)` pair. The object key shape
   is the backend-independent one from § Object Layout.
2. **Copy chunks.** Issue a HEAD on the target first — this is what
   makes a retry idempotent — and if the chunk is missing,
   `source.download_chunk(key)`, then BLAKE3-verify, then
   `target.upload_chunk(key, bytes)`.
3. **Copy manifest backups.** Every key under `manifests/<barcode>/`
   on the source is copied across: JSON keys via `upload_manifest` /
   `download_manifest`, binary index pages via `upload_chunk` /
   `download_chunk`.
4. **Move local pool files.** Files at
   `<data_dir>/chunks/<source>/[<ns>/]<aa>/<bb>/<hash>.dat` are
   renamed under `<data_dir>/chunks/<target>/[<ns>/]<aa>/<bb>/<hash>.dat`.
5. **Commit.** An atomic temp+rename of `manifest.json` with `backend`
   set to the new name. This is *the* commit point — see crash
   semantics below.
6. **Delete source (best-effort).** Manifest backups are always
   deleted. Chunks are deleted only under `Local` dedup; under
   `Global` dedup a chunk may still be referenced by a sibling
   cartridge on the source backend, so the chunks are left for
   `system gc` to reclaim as orphans. A failure here becomes a
   warning, not a migration failure.

### Rebind mode — phases

1. **Discover chunks.** Same as move.
2. **Verify** (unless `--no-verify`). HEAD every chunk key on the
   target, plus `manifests/<barcode>/manifest-latest.json`. Any single
   miss aborts the rebind with `RebindTargetMissing { keys }` (the key
   list is capped at 16); because nothing has been mutated yet, the
   abort is clean. The source backend is never contacted in this mode.
3. **Move local pool files.** Same as move.
4. **Commit.** Same as move.

### Refuse-gates

- Daemon must be running (admin socket is the only entry point).
- Cartridge must not be loaded in any drive
  (`find_drive_for_loaded_cartridge` on `inventory.json`).
- Target backend must exist in `storage.backends:`.
- Source ≠ target.
- WORM cartridges require the target's `retention_mode` to be
  `governance` or `compliance`.

### Audit

One entry per invocation:

- `cartridge.migrated` — move mode. Params: `barcode`, `mode: "move"`,
  `from_backend`, `to_backend`, `chunks_total`, `chunks_copied`,
  `bytes_copied`, `manifest_objects_copied`, `source_objects_deleted`,
  `local_files_moved`, `source_delete_warnings`, `dry_run`.
- `cartridge.rebound` — rebind mode. Same params, plus
  `chunks_verified`.
- `cartridge.tiered` — one entry per cartridge moved by `system tiering
  run-now` (policy-driven migration). Params on success: `barcode`,
  `from`, `to`, `chunk_count`, `bytes`. On failure: `barcode`, `from`,
  `to` with `result: Error(reason)`. Each move reuses the move-mode
  primitive, so its data movement matches `cartridge.migrated`; the
  distinct op name records that the move was policy-initiated rather
  than operator-initiated.

Both Ok and Err paths audited (failures carry `result: Error(reason)`).

Migration of any kind refuses a cartridge under a cloud-native legal
hold (the hold has no cross-backend transfer path), so neither
`cartridge migrate` nor `system tiering run-now` can relocate a held
cartridge.

### Crash semantics

The manifest flip in phase 5 is the single commit point, and that
makes a crash recoverable from either side of it:

- A crash **before** the flip leaves orphan chunks on the target, the
  source intact, and no on-disk manifest change. To recover, either
  re-run migrate — the HEAD-then-copy idempotency makes the second
  pass a no-op for chunks already uploaded — or run `system gc` on the
  target to reclaim the orphans.
- A crash **after** the flip but **before** the source-delete leaves
  orphan chunks on the source, with the manifest correctly pointing at
  the target. To recover, run `system gc` on the source.

### Exit codes

- 0 — success (or dry-run plan generated).
- 1 — migration failed mid-run (chunk verify mismatch, target unreachable, …).
- 2 — refuse-gate triggered or bad params (loaded, WORM/retention mismatch, unknown backend, …).

---

## Cartridge archive — `thurvtl cartridge archive`

Archiving snapshots a cartridge onto a different storage backend as a
frozen, self-contained blob. The contrast with migrate is that archive
leaves the source cartridge entirely untouched — its manifest,
indexes, local pool, and bound backend all stay intact — so the same
cartridge can have several archives coexisting under distinct labels.

It runs as a daemon-routed admin job (kind `cartridge.archive`),
backed by `core_stream::cartridge_archive::run_archive`.

### CLI surface

```
thurvtl cartridge archive <BARCODE> --target-backend <NAME>
    [--label LABEL]   (defaults to `archive-<ISO-8601-UTC>`)
    [--dry-run]
```

### Object layout on the target backend

```
archives/<barcode>/<label>/manifest.json
archives/<barcode>/<label>/chunks.idx
archives/<barcode>/<label>/blocks-p<N>.idx
archives/<barcode>/<label>/chunks/<aa>/<bb>/<hash>.dat
```

The archive is self-contained: its chunks live under the archive
prefix, not in the target's regular `chunks/` pool, so an archive of
cartridge X cannot collide with a *live* cartridge X bound to the same
backend. The archive's own `manifest.json` is the source manifest plus
two provenance fields — `archived_from_backend`, the source's bound
backend, and `archived_at`, an ISO-8601 UTC timestamp.

### Phases

1. **Validate.** The label must be 1-64 characters, alphanumeric with
   `-` and `_` allowed; the target backend must be named in
   `storage.backends:`; source and target must differ; and
   `archives/<barcode>/<label>/manifest.json` must not already exist
   on the target.
2. **Walk `chunks.idx`** to collect every sealed chunk's hash.
3. **Copy chunks.** For each hash, prefer the local pool and fall back
   to the source backend's object key
   (`chunks/[ns/]<aa>/<bb>/<hash>.dat`). BLAKE3-verify the bytes, then
   `target.upload_chunk` them under the archive prefix.
4. **Snapshot index files.** Read `chunks.idx` and every
   `blocks-p<N>.idx` from disk and upload each as a single binary blob
   under the archive prefix.
5. **Stamp + upload manifest.** Insert the `archived_from_backend` and
   `archived_at` fields, then PUT `manifest.json` last — sentinel-last,
   because it is the manifest's presence that makes the archive
   discoverable at all.

### Refuse-gates

- Daemon running.
- Cartridge not loaded in any drive.
- Target backend named in `storage.backends:`.
- Source ≠ target.
- Label is non-empty + matches the allowed character set.
- Archive at the same `(barcode, label)` doesn't already exist.
- WORM cartridges require the target's `retention_mode` to be
  governance or compliance.

### Audit

`cartridge.archived` — `barcode`, `from_backend`, `to_backend`,
`label`, `archived_at`, `chunks_total`, `chunks_uploaded`,
`chunks_from_local_pool`, `chunks_from_source_storage`,
`bytes_uploaded`, `index_files_uploaded`, `dry_run`. Both Ok and Err
paths.

### Crash semantics

Because the archive sentinel (`manifest.json`) is uploaded last, a
crash mid-archive leaves orphan chunks under the archive prefix but no
discoverable sentinel. `system gc` does *not* sweep archive prefixes —
they live outside the regular `manifests/` and `chunks/` keyspaces —
so the recourse is manual: delete the partial
`archives/<barcode>/<label>/` subtree by hand and re-archive under a
fresh label. A same-label retry would be refused anyway by the
duplicate check in phase 1.

### Exit codes

Same as migrate (0 success / 1 mid-run failure / 2 refuse-gate).

---

## Restore-archive — `thurvtl library restore-archive`

This verb is the inverse of archive: it pulls a frozen archive back
into a live cartridge. It runs as a daemon-routed admin job (kind
`library.restore_archive`), backed by
`core_mediachanger::library::restore_archive::run_restore_archive`,
plus a caller-side `Library::add_or_create_tape` to seat the restored
cartridge into a storage slot.

### CLI surface

```
thurvtl library restore-archive
    --backend <NAME> --barcode <BC> --label <LABEL>
    [--as-barcode <NEW>]      (rename + fresh UUID)
    [--allow-existing]
    [--dry-run]
```

### Phases

1. **Validate.** The backend must be named in `storage.backends:`, and
   the archive sentinel must exist on it — confirmed with a HEAD of
   `manifest.json` under the archive prefix.
2. **Plan destination.** The local cartridge directory is
   `<tapes_dir>/<local_barcode>/`, where `local_barcode` defaults to
   the source barcode unless the operator overrides it with
   `--as-barcode <NEW>`. If that directory already exists, the restore
   refuses — unless `--allow-existing` is set, in which case it simply
   treats the restore as a no-op.
3. **Download + rewrite manifest.** GET
   `archives/<barcode>/<label>/manifest.json` and rewrite it for the
   new local cartridge: `label` becomes the new barcode, `backend`
   becomes the restoring backend, a fresh `uuid` is minted at restore
   time, `index_epoch` and `pending_partition_layout` are cleared, and
   the `archived_from_*` / `archived_at` provenance fields are
   preserved.
4. **Download index files.** Fetch `chunks.idx` and every
   `blocks-p<N>.idx` — enumerated with `list_objects` on the archive
   prefix — into the new cartridge directory.
5. **Download chunks.** Walk the local `chunks.idx`, and for each
   sealed entry, download the chunk from the archive prefix and
   `ChunkPool::insert_verified_bytes` it into the local pool. Each
   `chunks.idx` record is rewritten to `LocationTag::LocalOnly,
   uploaded=false`, so that the daemon's orphan-upload sweep will
   eventually mirror each chunk into the backend's *regular*
   `chunks/[ns/]<aa>/<bb>/<hash>.dat` key — which is where the live
   cartridge will look for it on a cache eviction and storage refetch.
6. **Seat.** The caller — the daemon handler — briefly acquires the
   library mutex and calls `Library::add_or_create_tape(local_barcode,
   backend_name)` to land the cartridge in a free storage slot. The
   mutex is released before any subsequent `JobEmitter` await.

### Refuse-gates

- Daemon running.
- Backend named in `storage.backends:`.
- Archive sentinel exists on the backend at the named
  `(barcode, label)`.
- Local cart dir doesn't already exist (unless `--allow-existing`).

### Audit

`library.restore_archive` — `source_barcode`, `local_barcode`,
`backend`, `label`, `chunks_total`, `chunks_downloaded`,
`bytes_downloaded`, `index_files_downloaded`, `seated_in_slot`,
`skipped_existing`, `dry_run`. Both Ok and Err paths;
`skipped_existing=true` does not generate an Ok audit entry (the
operation was a no-op).

### Crash semantics

A crash mid-restore leaves a partial cartridge directory at
`<tapes_dir>/<local_barcode>/`. The next attempt will refuse because
that directory exists, so the recovery is to `rm -rf
<tapes_dir>/<local_barcode>/` first and then re-run. Nothing on the
backend can become inconsistent — the archive prefix is read-only
throughout the operation.

### Exit codes

Same as migrate (0 / 1 / 2).

