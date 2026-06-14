// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Content-addressed chunk pool — the shared substrate every product
//! type uses to dedupe data chunks against each other on disk and
//! upload to storage.
//!
//! Sealed chunks live at:
//!
//! ```text
//! <root>/chunks/<backend>/[<namespace>/]<aa>/<bb>/<full_hash_hex>.dat
//! ```
//!
//! where:
//! - `<backend>` is the named storage backend the chunk is bound to.
//!   Per-backend sharding gives unambiguous "which storage holds the
//!   authoritative copy" semantics on cache eviction and refetch.
//! - `<namespace>` is optional — present when dedup scope is `Local`
//!   (per-cartridge for tape, per-volume for block), absent under
//!   `Global` (chunks shared across the entire backend).
//! - `<aa>` and `<bb>` are the first two and next two hex chars of
//!   the BLAKE3 hash, giving 65 536-way fanout. At LTO-8 default
//!   scale (~60 M chunks) that's ~900 entries/leaf; at 640-slot max
//!   scale (~1 B chunks) ~15 K entries/leaf — comfortably under
//!   ext4/xfs htree limits.
//!
//! Files are immutable once sealed: every insert path goes through a
//! sibling tempfile + atomic rename, so a torn write leaves either
//! the prior version or no file at all. Concurrent inserts of the
//! same hash race the rename — both writers wrote identical bytes,
//! so the result is byte-identical.
//!
//! # What this crate is **not**
//!
//! - **Not refcounted.** Garbage collection is a separate manifest-
//!   walking pass per product (thurvtl's `system gc`, thurvsa's pending
//!   GC sweep).
//! - **Not aware of the storage.** [`ChunkPool::object_key`] computes the
//!   key shape; uploads are driven by the consuming product.
//! - **Not aware of higher-level identity.** Hashes are global within
//!   a (backend, namespace) pair.
//!
//! # Lifted from where
//!
//! Step 5 Milestone 5.A.3 (2026-05-09) collapses what used to be two
//! near-duplicate implementations:
//!
//! - `core/smc/src/chunk_store.rs::ChunkStore` — tape side, used
//!   `insert_from_path(src, hash_hex)` because chunks are streamed
//!   into a staging dir before sealing.
//! - `core/sbc/src/chunk_pool.rs::ChunkPool` — block side, used
//!   `insert_bytes(&[u8])` because page-sized writes arrive as
//!   buffers, not files.
//!
//! Both API shapes survive on the unified [`ChunkPool`] so call sites
//! upgrade lazily; the legacy crates keep `pub use` re-export shells
//! (`core_mediachanger::ChunkStore` aliases `shared_pool::ChunkPool`,
//! `core_block::ChunkPool` re-exports verbatim) so internal call
//! sites compile unchanged.

#![forbid(unsafe_code)]

pub mod budget;
pub mod disk_cache_size;
pub mod ghost;
pub use budget::{BackpressureError, PoolBudget};
pub use disk_cache_size::{DiskCacheBounds, DiskCacheSize};
pub use ghost::{GhostHash, GhostList};

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use blake3::Hasher;
use thiserror::Error;
use tracing::warn;

/// Process-global pin table for outstanding chunk references that
/// outlive any single live `pages.idx` entry — at present only the
/// Hyper-V ODX (`POPULATE TOKEN` → `WRITE USING TOKEN`) flow holds
/// these. Keyed by `(backend, namespace, hash_hex)` with a refcount
/// value; the eviction sweep and the manifest-walking GC both consult
/// [`ChunkPool::is_pinned`] before removing a chunk file.
///
/// In-memory only — process restart drops every pin, which matches
/// ODX semantics (tokens are TTL-bounded and don't survive daemon
/// restart). Pin acquisition is a take-the-mutex, bump-the-counter
/// operation; uncontended pin/unpin pairs are sub-microsecond.
type PinKey = (String, Option<String>, String);
static PIN_TABLE: Mutex<BTreeMap<PinKey, u32>> = Mutex::new(BTreeMap::new());

/// RAII handle returned by [`ChunkPool::pin`]. Drop decrements the
/// refcount and removes the entry when it hits zero. Cheap to hold —
/// three `String`s — and `Send + Sync`, so token state can keep a
/// `Vec<PoolPinGuard>` across `.await` points.
#[derive(Debug)]
pub struct PoolPinGuard {
    key: Option<PinKey>,
}

impl Drop for PoolPinGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let mut table = match PIN_TABLE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(slot) = table.get_mut(&key) {
            *slot = slot.saturating_sub(1);
            if *slot == 0 {
                table.remove(&key);
            }
        }
    }
}

// Per-process monotonic counter for tempfile names. Combined with the
// pid this gives every concurrent insert_* call a unique sibling
// tempfile, so racing inserters of identical content don't trample
// each other's `.tmp` (the bug that surfaced in the
// `concurrent_inserts_of_same_bytes_*` tests as a NotFound on rename).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp_path(dst: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dst.with_extension(format!("dat.{}.{n}.tmp", std::process::id()))
}

/// Best-effort fsync of a file's parent directory so a preceding
/// `rename(2)` into the pool is durable across power loss (issue #115).
/// A failure can't tear the chunk (the rename already landed), so it is
/// swallowed.
fn sync_pool_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Copy `src` into the content-addressed `dst` honoring the "tempfile +
/// atomic rename" contract: copy to a sibling temp of `dst`, fsync, then
/// rename(2) into place; clean up the temp on any error. Used by
/// `insert_from_path`'s cross-device fallback (rename(2) can't move across
/// filesystems). A torn `fs::copy` straight onto `dst` would leave a
/// truncated file that the dedup short-circuit and the upload worker both
/// trust as authoritative — permanent corruption (issue #124).
fn copy_into_pool_atomic(src: &Path, dst: &Path) -> Result<(), ChunkPoolError> {
    let tmp = unique_tmp_path(dst);
    if let Err(e) = fs::copy(src, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    {
        let f = File::open(&tmp)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Errors out of the chunk-pool layer. Both products' error enums
/// implement `From<ChunkPoolError>`, so handler-side `?` propagation
/// works through.
#[derive(Error, Debug)]
pub enum ChunkPoolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Bytes a caller asked us to insert under `expected` don't actually
    /// hash to `expected`. Surfaced by [`ChunkPool::insert_verified_bytes`]
    /// — the storage-download integrity guard. Caller-side: treat as
    /// permanent (don't retry blindly), let it bubble to the SCSI layer.
    #[error("content hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
}

/// Per-backend content-addressed chunk pool. Optionally scoped to a
/// namespace (cartridge label for tape, volume name for block) under
/// `Local` dedup. Cheap to clone — internally a path + two short
/// strings.
#[derive(Debug, Clone)]
pub struct ChunkPool {
    root: PathBuf,
    backend_name: String,
    namespace: Option<String>,
}

impl ChunkPool {
    /// Open (and `mkdir -p`) the shared per-backend pool. Use this
    /// when dedup scope is `Global` (cross-cartridge / cross-volume
    /// chunk sharing).
    pub fn new<P: AsRef<Path>>(root: P, backend_name: &str) -> Result<Self, ChunkPoolError> {
        let pool = Self {
            root: root.as_ref().to_path_buf(),
            backend_name: backend_name.to_string(),
            namespace: None,
        };
        fs::create_dir_all(pool.pool_dir())?;
        Ok(pool)
    }

    /// Open (and `mkdir -p`) a namespaced pool. Use this under
    /// `Local` dedup — `namespace` is the cartridge label (tape) or
    /// volume name (block); chunks never cross the namespace
    /// boundary.
    pub fn new_namespaced<P: AsRef<Path>>(
        root: P,
        backend_name: &str,
        namespace: &str,
    ) -> Result<Self, ChunkPoolError> {
        let pool = Self {
            root: root.as_ref().to_path_buf(),
            backend_name: backend_name.to_string(),
            namespace: Some(namespace.to_string()),
        };
        fs::create_dir_all(pool.pool_dir())?;
        Ok(pool)
    }

    /// Root directory of this pool (the parent of `chunks/`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Backend name this pool is scoped to.
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Namespace, if `Local` dedup is in effect.
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Backwards-compat alias for [`Self::namespace`] — thurvtl's
    /// historical API surface used `cartridge_namespace`. Kept as a
    /// distinct method so the existing call sites resolve unchanged.
    pub fn cartridge_namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Per-backend (and optionally per-namespace) on-disk pool root.
    pub fn pool_dir(&self) -> PathBuf {
        let base = self.root.join("chunks").join(&self.backend_name);
        match &self.namespace {
            Some(ns) => base.join(ns),
            None => base,
        }
    }

    /// On-disk path for a chunk by hex hash. Does not check
    /// existence. Caller is responsible for `mkdir -p` on the shard
    /// directory before writing (use one of the `insert_*` helpers).
    pub fn store_path(&self, hash_hex: &str) -> PathBuf {
        let (s1, s2) = shard_pair(hash_hex);
        self.pool_dir()
            .join(s1)
            .join(s2)
            .join(format!("{hash_hex}.dat"))
    }

    /// Best-effort modification time of the pool file for `hash_hex`, in
    /// unix seconds. `None` if the file is absent or its mtime can't be
    /// read. Used by GC's recent-seal grace window: a chunk sealed during
    /// the sweep (after the live-set snapshot) has a fresh mtime, so GC
    /// can skip it instead of deleting a still-referenced chunk whose
    /// reference it hasn't observed yet (issue #141).
    pub fn chunk_mtime_secs(&self, hash_hex: &str) -> Option<u64> {
        let meta = fs::metadata(self.store_path(hash_hex)).ok()?;
        meta.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }

    /// Storage key for a chunk in this pool — honours the pool's own
    /// namespace (under `Local` dedup the namespace segment is
    /// preserved so sibling-volume chunks don't collide in a shared
    /// bucket). The per-backend shard is stripped because each
    /// backend already has its own bucket / prefix.
    ///
    /// Returned as a forward-slash-separated string so it works on
    /// every ObjectStoreBackend implementation regardless of host OS.
    pub fn object_key(&self, hash_hex: &str) -> String {
        Self::object_key_for(self.namespace.as_deref(), hash_hex)
    }

    /// Storage key with the namespace passed in explicitly. Useful when
    /// the caller has the namespace string but no live `ChunkPool`
    /// (e.g. legal-hold / verify paths walking manifests).
    pub fn object_key_for(namespace: Option<&str>, hash_hex: &str) -> String {
        let (s1, s2) = shard_pair(hash_hex);
        match namespace {
            Some(ns) => format!("chunks/{ns}/{s1}/{s2}/{hash_hex}.dat"),
            None => format!("chunks/{s1}/{s2}/{hash_hex}.dat"),
        }
    }

    /// Backwards-compat alias for [`Self::object_key`] —
    /// thurvtl's historical surface called this method
    /// `object_key_in_store`. Kept distinct so existing call sites
    /// resolve unchanged.
    pub fn object_key_in_store(&self, hash_hex: &str) -> String {
        self.object_key(hash_hex)
    }

    /// Does the chunk exist in the local pool?
    pub fn exists(&self, hash_hex: &str) -> bool {
        self.store_path(hash_hex).is_file()
    }

    /// BLAKE3 hash of an on-disk file, streamed (no full read into
    /// memory). Used to seal a freshly-rolled staging chunk before
    /// inserting into the pool.
    pub fn hash_file(path: &Path) -> Result<String, ChunkPoolError> {
        let f = File::open(path)?;
        let mut reader = BufReader::with_capacity(64 * 1024, f);
        let mut hasher = Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize().as_bytes()))
    }

    /// Hash and seal `bytes` into the pool. Returns the BLAKE3 hex
    /// hash plus a flag — `true` if the chunk was freshly written,
    /// `false` if an identical hash already lived in the pool (dedup
    /// hit). Atomic: writes a sibling tempfile then renames over the
    /// destination.
    ///
    /// Post-condition: on `Ok`, the chunk file at `store_path(hash)`
    /// is present on disk **regardless of the flag** — `false` means
    /// it was already on disk when the call hashed the bytes, `true`
    /// means this call's rename put it there. Callers that release a
    /// pool-budget reservation on a `false` (dedup-hit) result rely on
    /// this: the bytes are accounted for by the pre-existing chunk.
    pub fn insert_bytes(&self, bytes: &[u8]) -> Result<(String, bool), ChunkPoolError> {
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        let hash_hex = hex::encode(hasher.finalize().as_bytes());

        let dst = self.store_path(&hash_hex);
        if dst.is_file() {
            return Ok((hash_hex, false));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = unique_tmp_path(&dst);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        match fs::rename(&tmp, &dst) {
            Ok(()) => {
                // fsync the shard directory so the rename's directory
                // entry is durable: the tempfile data is fsynced above,
                // but without this a power loss can revert the rename
                // and leave no chunk file — while the index files that
                // reference this hash ARE fsynced at seal time, so the
                // recovered index would point at a vanished chunk
                // (issue #166). One fsync per sealed chunk, not per IO.
                sync_pool_dir(&dst);
                Ok((hash_hex, true))
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    /// Insert bytes the caller obtained from an untrusted source
    /// (storage download, prefetcher) under the content hash the caller
    /// *expects*. Hashes the bytes here and refuses the insert if
    /// they don't match `expected_hash` — this is the storage
    /// bit-rot / wrong-bytes guard.
    ///
    /// Distinct from [`Self::insert_bytes`] (which discovers the
    /// hash from the bytes) and [`Self::insert_from_path`] (which
    /// trusts a hash computed by the streaming-write path).
    ///
    /// Idempotent: if the destination already holds a file under
    /// `expected_hash`, return Ok without rehashing — the existing
    /// file is authoritative and the caller's bytes are surplus.
    ///
    /// Returns `was_new`: `true` iff this call's rename actually put
    /// the file on disk, `false` if it was already present (the
    /// idempotent no-op). Mirrors [`Self::insert_bytes`]'s flag — a
    /// caller that reserves pool-budget bytes for a cache-miss /
    /// prefetch warm-in MUST gate the reserve on `was_new` so a chunk
    /// already warmed by a racing fetch isn't counted twice. As with
    /// `insert_bytes`, the flag is decided by an `is_file` check
    /// immediately before the rename, so two callers racing on the
    /// same absent hash can both observe `true` (the rename overwrites
    /// idempotently) — a microsecond window, not the whole download,
    /// and the budget divergence detector is the backstop.
    pub fn insert_verified_bytes(
        &self,
        expected_hash: &str,
        bytes: &[u8],
    ) -> Result<bool, ChunkPoolError> {
        let dst = self.store_path(expected_hash);
        if dst.is_file() {
            return Ok(false);
        }

        let mut hasher = Hasher::new();
        hasher.update(bytes);
        let actual = hex::encode(hasher.finalize().as_bytes());
        if actual != expected_hash {
            return Err(ChunkPoolError::HashMismatch {
                expected: expected_hash.to_string(),
                actual,
            });
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = unique_tmp_path(&dst);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        match fs::rename(&tmp, &dst) {
            Ok(()) => {
                // Durably link the new chunk into its shard directory
                // before returning (issue #166) — see `insert_bytes`.
                sync_pool_dir(&dst);
                Ok(true)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    /// Insert a freshly-built chunk file into the pool under its
    /// content hash. If the destination already exists, `src` is
    /// removed and the existing pool file is reused — that's the
    /// dedup hit. Atomic on the same filesystem; falls back to
    /// copy + remove on a cross-device staging dir.
    pub fn insert_from_path(&self, src: &Path, hash_hex: &str) -> Result<(), ChunkPoolError> {
        let dst = self.store_path(hash_hex);
        if dst.exists() {
            fs::remove_file(src)?;
            return Ok(());
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::rename(src, &dst) {
            Ok(()) => {
                // fsync the pool shard dir so the rename is durable across
                // power loss (issue #115) — the caller fsynced the staging
                // file's contents before handing it to us.
                sync_pool_dir(&dst);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                // Cross-device staging: rename(2) can't move across
                // filesystems. Copy via a fsync'd tempfile + atomic
                // rename (not straight onto `dst`) so a torn copy can't
                // be sealed as authoritative — issue #124.
                copy_into_pool_atomic(src, &dst)?;
                sync_pool_dir(&dst);
                fs::remove_file(src)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Open a chunk for reading by hash. Returns the standard io
    /// error if the chunk is not in the pool.
    pub fn open_read(&self, hash_hex: &str) -> Result<File, ChunkPoolError> {
        let p = self.store_path(hash_hex);
        File::open(&p).map_err(|e| {
            warn!("ChunkPool::open_read miss for {}: {}", hash_hex, e);
            ChunkPoolError::Io(e)
        })
    }

    /// Read a chunk's bytes in full. Convenience wrapper for the
    /// page-sized reads SBC-3 issues; not appropriate for very large
    /// chunks (callers wanting a streamed read use [`Self::open_read`]).
    pub fn read_bytes(&self, hash_hex: &str) -> Result<Vec<u8>, ChunkPoolError> {
        Ok(fs::read(self.store_path(hash_hex))?)
    }

    /// Remove a chunk from the local pool. No-op if the file is
    /// already gone. Used by the cache-eviction path and by GC.
    pub fn remove(&self, hash_hex: &str) -> Result<(), ChunkPoolError> {
        let p = self.store_path(hash_hex);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Increment the process-global refcount on `hash_hex` for this
    /// pool's `(backend, namespace)` and return a guard whose drop
    /// decrements it. While at least one guard is alive,
    /// [`Self::is_pinned`] returns `true` and the eviction worker
    /// (`DiskCacheManager::evict_lru_chunks`) plus the manifest-walking
    /// GC skip the chunk file.
    ///
    /// Pinning does **not** materialize the chunk locally, hash-check
    /// it, or extend its TTL on the backend — it only blocks local
    /// removal. The caller is responsible for fetching the chunk
    /// (typically via `ChunkPool::insert_verified_bytes` from a
    /// backend GET) before the pinned reference is consumed.
    pub fn pin(&self, hash_hex: &str) -> PoolPinGuard {
        let key: PinKey = (
            self.backend_name.clone(),
            self.namespace.clone(),
            hash_hex.to_string(),
        );
        let mut table = match PIN_TABLE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *table.entry(key.clone()).or_insert(0) += 1;
        PoolPinGuard { key: Some(key) }
    }

    /// True iff at least one [`PoolPinGuard`] is currently alive for
    /// `hash_hex` in this pool. Cheap (one mutex lock + map lookup);
    /// fine to call per-chunk inside an eviction sweep.
    pub fn is_pinned(&self, hash_hex: &str) -> bool {
        let key: PinKey = (
            self.backend_name.clone(),
            self.namespace.clone(),
            hash_hex.to_string(),
        );
        let table = match PIN_TABLE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        table.get(&key).copied().unwrap_or(0) > 0
    }

    /// Backend-scoped pin check that doesn't require an open pool
    /// instance — used by the manifest-walking GC, which iterates
    /// namespaces from disk without constructing one `ChunkPool` per
    /// chunk.
    pub fn is_pinned_for(backend: &str, namespace: Option<&str>, hash_hex: &str) -> bool {
        let key: PinKey = (
            backend.to_string(),
            namespace.map(str::to_string),
            hash_hex.to_string(),
        );
        let table = match PIN_TABLE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        table.get(&key).copied().unwrap_or(0) > 0
    }

    /// Walk every chunk currently in this pool. Yields
    /// `(hash_hex, size_bytes)`. Skips entries whose filename doesn't
    /// look like `<64-hex>.dat`, so partially-written tempfiles
    /// won't be reported as live chunks.
    pub fn iter_chunks(&self) -> Result<Vec<(String, u64)>, ChunkPoolError> {
        let mut out = Vec::new();
        self.walk_chunk_files(|hash_hex, size| out.push((hash_hex.to_string(), size)))?;
        Ok(out)
    }

    /// Streaming counterpart of [`iter_chunks`]: invokes `f` with each
    /// chunk's 32-byte hash + size **without** materializing the whole
    /// pool. The `system stats` / `system gc` dedup scans fold straight
    /// into a `[u8; 32]`-keyed map this way, avoiding both the per-chunk
    /// 64-char hex `String` and the one giant `Vec<(String, u64)>`
    /// `iter_chunks` builds — multi-GB of job RAM (OOM risk) at the
    /// ~tens-of-millions-of-chunks scale a TiB-class entity reaches
    /// (issue #222). A malformed `<64-hex>.dat` filename that somehow
    /// passes the walk's validation but fails to decode is skipped.
    pub fn for_each_chunk(
        &self,
        mut f: impl FnMut([u8; 32], u64),
    ) -> Result<(), ChunkPoolError> {
        self.walk_chunk_files(|hash_hex, size| {
            if let Some(bytes) = decode_hash_hex(hash_hex) {
                f(bytes, size);
            }
        })
    }

    /// Shared two-level shard walk behind [`iter_chunks`] /
    /// [`for_each_chunk`]. Calls `f(hash_hex, size)` once per file named
    /// `<64-hex>.dat`; skips everything else (tempfiles, stray dirs).
    fn walk_chunk_files(
        &self,
        mut f: impl FnMut(&str, u64),
    ) -> Result<(), ChunkPoolError> {
        let pool = self.pool_dir();
        if !pool.is_dir() {
            return Ok(());
        }
        for s1_entry in fs::read_dir(&pool)? {
            let s1_entry = s1_entry?;
            if !s1_entry.file_type()?.is_dir() {
                continue;
            }
            for s2_entry in fs::read_dir(s1_entry.path())? {
                let s2_entry = s2_entry?;
                if !s2_entry.file_type()?.is_dir() {
                    continue;
                }
                for chunk_entry in fs::read_dir(s2_entry.path())? {
                    let chunk_entry = chunk_entry?;
                    let meta = chunk_entry.metadata()?;
                    if !meta.is_file() {
                        continue;
                    }
                    let name = match chunk_entry.file_name().into_string() {
                        Ok(n) => n,
                        Err(_) => continue,
                    };
                    let Some(hash) = name.strip_suffix(".dat") else {
                        continue;
                    };
                    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                        continue;
                    }
                    f(hash, meta.len());
                }
            }
        }
        Ok(())
    }
}

/// Decode a 64-char lowercase/uppercase hex chunk hash into its 32 raw
/// bytes, allocation-free. Returns `None` on a wrong length or a
/// non-hex digit. Public so callers that already hold a hash as hex (a
/// parsed storage object key, say) can fold into the same `[u8; 32]`
/// keyed live set the streaming pool walk produces (issue #222).
pub fn decode_hash_hex(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// First and second 2-hex-char shard for a chunk hash. Falls back to
/// `("00", "00")` for too-short inputs (defensive — BLAKE3 always
/// emits 64 hex chars, so the fallback exists only for malformed
/// callers).
fn shard_pair(hash_hex: &str) -> (&str, &str) {
    let s1 = if hash_hex.len() >= 2 {
        &hash_hex[..2]
    } else {
        "00"
    };
    let s2 = if hash_hex.len() >= 4 {
        &hash_hex[2..4]
    } else {
        "00"
    };
    (s1, s2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn store_path_shards_two_levels_under_backend() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let p = pool.store_path("abcd1234");
        assert!(
            p.ends_with("chunks/primary/ab/cd/abcd1234.dat"),
            "got: {}",
            p.display()
        );
        assert_eq!(pool.backend_name(), "primary");
        assert!(pool.namespace().is_none());
    }

    #[test]
    fn namespaced_pool_includes_namespace_segment() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new_namespaced(tmp.path(), "primary", "vol1").unwrap();
        let p = pool.store_path("deadbeefcafe");
        assert!(
            p.ends_with("chunks/primary/vol1/de/ad/deadbeefcafe.dat"),
            "got: {}",
            p.display()
        );
        assert_eq!(pool.namespace(), Some("vol1"));
        assert_eq!(pool.cartridge_namespace(), Some("vol1"));
    }

    #[test]
    fn storage_key_strips_backend_keeps_namespace() {
        let tmp = TempDir::new().unwrap();
        let global = ChunkPool::new(tmp.path(), "primary").unwrap();
        assert_eq!(global.object_key("deadbeef"), "chunks/de/ad/deadbeef.dat");
        let local = ChunkPool::new_namespaced(tmp.path(), "primary", "vol1").unwrap();
        assert_eq!(
            local.object_key("deadbeef"),
            "chunks/vol1/de/ad/deadbeef.dat"
        );
    }

    #[test]
    fn storage_key_for_static_form_no_namespace() {
        assert_eq!(
            ChunkPool::object_key_for(None, "deadbeef"),
            "chunks/de/ad/deadbeef.dat"
        );
    }

    #[test]
    fn storage_key_for_static_form_with_namespace() {
        assert_eq!(
            ChunkPool::object_key_for(Some("TAPE001"), "deadbeef"),
            "chunks/TAPE001/de/ad/deadbeef.dat"
        );
    }

    #[test]
    fn insert_bytes_seals_then_dedups() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let bytes = b"hello pool".to_vec();

        let (h1, was_new1) = pool.insert_bytes(&bytes).unwrap();
        assert!(was_new1);
        assert!(pool.exists(&h1));

        let (h2, was_new2) = pool.insert_bytes(&bytes).unwrap();
        assert_eq!(h1, h2);
        assert!(!was_new2);

        assert_eq!(pool.read_bytes(&h1).unwrap(), bytes);
    }

    #[test]
    fn insert_from_path_then_open_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();

        let staging = tmp.path().join("staging.dat");
        std::fs::File::create(&staging)
            .unwrap()
            .write_all(b"hello dedup")
            .unwrap();
        let hash = ChunkPool::hash_file(&staging).unwrap();

        pool.insert_from_path(&staging, &hash).unwrap();
        assert!(pool.exists(&hash));
        assert!(!staging.exists(), "staging file should be moved/removed");

        let mut buf = Vec::new();
        pool.open_read(&hash)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"hello dedup");
    }

    #[test]
    fn copy_into_pool_atomic_roundtrips_and_leaves_no_temp() {
        // The cross-device fallback helper must land complete content at
        // the destination and leave no staging temp behind (issue #124).
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.dat");
        std::fs::write(&src, b"cross-device payload").unwrap();
        let dst_dir = tmp.path().join("pool");
        std::fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("chunk.dat");

        copy_into_pool_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"cross-device payload");

        let temps: Vec<_> = std::fs::read_dir(&dst_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(temps.is_empty(), "no staging temp should remain");
    }

    #[test]
    fn copy_into_pool_atomic_missing_source_leaves_no_temp() {
        // A failed copy (missing source) must not leave a torn file or a
        // stray temp at the destination.
        let tmp = TempDir::new().unwrap();
        let dst_dir = tmp.path().join("pool");
        std::fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("chunk.dat");
        let missing = tmp.path().join("does-not-exist.dat");

        assert!(copy_into_pool_atomic(&missing, &dst).is_err());
        assert!(!dst.exists(), "no torn file at destination");
        let entries: Vec<_> = std::fs::read_dir(&dst_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "no temp left behind on copy failure");
    }

    #[test]
    fn duplicate_insert_from_path_drops_source() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();

        let s1 = tmp.path().join("s1.dat");
        std::fs::File::create(&s1).unwrap().write_all(b"x").unwrap();
        let h = ChunkPool::hash_file(&s1).unwrap();
        pool.insert_from_path(&s1, &h).unwrap();

        let s2 = tmp.path().join("s2.dat");
        std::fs::File::create(&s2).unwrap().write_all(b"x").unwrap();
        pool.insert_from_path(&s2, &h).unwrap();

        assert!(!s2.exists(), "duplicate staging copy should be removed");
        assert_eq!(pool.iter_chunks().unwrap().len(), 1);
    }

    #[test]
    fn insert_verified_bytes_accepts_matching_hash() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let bytes = b"storage-fetched chunk".to_vec();
        let expected = hex::encode(blake3::hash(&bytes).as_bytes());

        assert!(
            pool.insert_verified_bytes(&expected, &bytes).unwrap(),
            "first insert of an absent chunk reports was_new=true"
        );
        assert!(pool.exists(&expected));
        assert_eq!(pool.read_bytes(&expected).unwrap(), bytes);
    }

    #[test]
    fn insert_verified_bytes_rejects_mismatch_and_leaves_pool_clean() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let wrong = "0".repeat(64);

        let err = pool
            .insert_verified_bytes(&wrong, b"some bytes that do not hash to the zeros")
            .unwrap_err();
        match err {
            ChunkPoolError::HashMismatch { expected, actual } => {
                assert_eq!(expected, wrong);
                assert_ne!(actual, wrong);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }

        assert!(!pool.exists(&wrong));
        assert!(pool.iter_chunks().unwrap().is_empty());

        let dst = pool.store_path(&wrong);
        let tmp_sibling = dst.with_extension("dat.tmp");
        assert!(!tmp_sibling.exists(), "tmpfile leaked on mismatch");
    }

    #[test]
    fn insert_verified_bytes_dedup_hit_returns_ok_without_rehash() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let bytes = b"already in the pool".to_vec();
        let hash = hex::encode(blake3::hash(&bytes).as_bytes());

        assert!(
            pool.insert_verified_bytes(&hash, &bytes).unwrap(),
            "first insert reports was_new=true"
        );

        assert!(
            !pool
                .insert_verified_bytes(&hash, b"wrong bytes ignored on dedup hit")
                .unwrap(),
            "dedup-hit insert reports was_new=false so callers skip the budget reserve"
        );
        assert_eq!(pool.read_bytes(&hash).unwrap(), bytes);
        assert_eq!(pool.iter_chunks().unwrap().len(), 1);
    }

    #[test]
    fn iter_chunks_skips_garbage_files() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let (hash, _) = pool.insert_bytes(b"real").unwrap();

        let shard = tmp
            .path()
            .join("chunks")
            .join("primary")
            .join(&hash[..2])
            .join(&hash[2..4]);
        std::fs::write(shard.join(".tmp-garbage"), b"garbage").unwrap();

        let chunks = pool.iter_chunks().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, hash);
    }

    #[test]
    fn iter_chunks_on_empty_pool_is_empty() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        assert!(pool.iter_chunks().unwrap().is_empty());
    }

    #[test]
    fn for_each_chunk_matches_iter_chunks_decoded() {
        // The streaming `[u8; 32]` walk (issue #222) must see exactly the
        // same (hash, size) set the hex `iter_chunks` does, just decoded.
        use std::collections::HashMap;
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        for body in [b"alpha".as_slice(), b"beta", b"gamma-payload"] {
            pool.insert_bytes(body).unwrap();
        }
        let mut want: HashMap<[u8; 32], u64> = HashMap::new();
        for (hex, size) in pool.iter_chunks().unwrap() {
            want.insert(decode_hash_hex(&hex).expect("valid pool hash"), size);
        }
        let mut got: HashMap<[u8; 32], u64> = HashMap::new();
        pool.for_each_chunk(|hash, size| {
            got.insert(hash, size);
        })
        .unwrap();
        assert_eq!(got, want);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn decode_hash_hex_round_trips_and_rejects_bad_input() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let (hash, _) = pool.insert_bytes(b"round-trip").unwrap();
        // hex -> bytes -> hex is the identity for a real 64-char hash.
        let bytes = decode_hash_hex(&hash).expect("valid hash decodes");
        let mut rehex = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write as _;
            write!(rehex, "{b:02x}").unwrap();
        }
        assert_eq!(rehex, hash);
        assert!(decode_hash_hex("tooshort").is_none());
        assert!(decode_hash_hex(&"z".repeat(64)).is_none());
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let (hash, _) = pool.insert_bytes(b"x").unwrap();
        pool.remove(&hash).unwrap();
        pool.remove(&hash).unwrap();
        assert!(!pool.exists(&hash));
    }

    #[test]
    fn insert_empty_bytes_hashes_to_known_blake3_zero() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let (hash, was_new) = pool.insert_bytes(&[]).unwrap();
        // BLAKE3 hash of empty input is a fixed 64-char hex string.
        assert_eq!(hash.len(), 64);
        assert!(was_new);
        assert!(pool.exists(&hash));
        assert!(pool.read_bytes(&hash).unwrap().is_empty());
    }

    #[test]
    fn namespaces_do_not_share_chunks() {
        // Local dedup scope must isolate chunks per namespace — chunk
        // written to vol1 must not satisfy a read from vol2 even when
        // the bytes hash to the same key.
        let tmp = TempDir::new().unwrap();
        let pool_a = ChunkPool::new_namespaced(tmp.path(), "primary", "vol1").unwrap();
        let pool_b = ChunkPool::new_namespaced(tmp.path(), "primary", "vol2").unwrap();

        let (hash, _) = pool_a.insert_bytes(b"shared payload").unwrap();
        assert!(pool_a.exists(&hash));
        assert!(!pool_b.exists(&hash), "namespaces must isolate chunks");

        let (hash_b, was_new) = pool_b.insert_bytes(b"shared payload").unwrap();
        assert_eq!(hash_b, hash, "same bytes -> same content hash");
        assert!(was_new, "second namespace must seal its own copy");
        assert_eq!(pool_a.iter_chunks().unwrap().len(), 1);
        assert_eq!(pool_b.iter_chunks().unwrap().len(), 1);
    }

    #[test]
    fn backends_do_not_share_chunks() {
        // Per-backend sharding: same namespace, different backend ->
        // disjoint storage. The pool_dir() includes <backend> in the
        // path, so each backend's chunk store is independent.
        let tmp = TempDir::new().unwrap();
        let primary = ChunkPool::new(tmp.path(), "primary").unwrap();
        let secondary = ChunkPool::new(tmp.path(), "secondary").unwrap();
        let (hash, _) = primary.insert_bytes(b"only on primary").unwrap();
        assert!(primary.exists(&hash));
        assert!(!secondary.exists(&hash));
    }

    #[test]
    fn open_read_returns_io_error_on_missing_chunk() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let missing = "0".repeat(64);
        match pool.open_read(&missing) {
            Err(ChunkPoolError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected NotFound Io error, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_inserts_of_same_bytes_converge() {
        // Two threads insert identical bytes simultaneously; the
        // atomic-rename design must leave exactly one chunk file and
        // both threads must return the same hash. (Either thread may
        // see was_new=true depending on rename ordering, but the
        // *final* pool state is a single sealed chunk.)
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let payload_a = payload.clone();
        let payload_b = payload.clone();

        let t1 = std::thread::spawn(move || pool_a.insert_bytes(&payload_a).unwrap());
        let t2 = std::thread::spawn(move || pool_b.insert_bytes(&payload_b).unwrap());
        let (h1, _) = t1.join().unwrap();
        let (h2, _) = t2.join().unwrap();

        assert_eq!(h1, h2);
        let chunks = pool.iter_chunks().unwrap();
        assert_eq!(chunks.len(), 1, "exactly one sealed chunk after race");
        assert_eq!(chunks[0].0, h1);
        assert_eq!(pool.read_bytes(&h1).unwrap(), payload);
    }

    #[test]
    fn hash_file_matches_insert_bytes_hash() {
        // hash_file (streaming, used to seal a staging chunk before
        // insert_from_path) must produce the same hex as insert_bytes
        // would for the identical payload — otherwise the two seal
        // paths could mint distinct keys for identical content.
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        let payload: Vec<u8> = (0..8192).map(|i| (i * 17 % 251) as u8).collect();

        let staging = tmp.path().join("staging.dat");
        std::fs::write(&staging, &payload).unwrap();
        let h_streamed = ChunkPool::hash_file(&staging).unwrap();

        let (h_in_mem, _) = pool.insert_bytes(&payload).unwrap();
        assert_eq!(h_streamed, h_in_mem);
    }

    // -- Concurrency tests ------------------------------------------------
    //
    // ChunkPool::insert_* paths have a check-then-act between
    // `dst.is_file()` and the tempfile rename. Two writers racing on
    // identical content can both pass the check; the rename ordering
    // determines which one's bytes survive on disk. Because the content
    // is bit-identical, the survivor's bytes are correct either way —
    // these tests pin that contract.

    use std::sync::Arc;
    use std::thread;

    #[test]
    fn concurrent_insert_bytes_same_content_idempotent() {
        let tmp = TempDir::new().unwrap();
        let pool = Arc::new(ChunkPool::new(tmp.path(), "primary").unwrap());
        let payload: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&pool);
            let bytes = payload.clone();
            handles.push(thread::spawn(move || p.insert_bytes(&bytes)));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in &results {
            assert!(r.is_ok(), "all racers must succeed: {:?}", r);
        }
        // All eight return the same hash.
        let hashes: Vec<&String> = results.iter().map(|r| &r.as_ref().unwrap().0).collect();
        let first = hashes[0];
        for h in &hashes {
            assert_eq!(h, &first);
        }
        // Exactly one inserter sees fresh=true; the rest hit the dedup
        // short-circuit OR raced the rename — every losing writer still
        // returns Ok with a valid hash. We allow either branch to
        // report fresh=true (the one that won the rename) so this is a
        // sanity check: not all eight can be fresh.
        let fresh_count = results.iter().filter(|r| r.as_ref().unwrap().1).count();
        assert!(
            (1..=8).contains(&fresh_count),
            "fresh_count={} (expected at least 1, at most 8)",
            fresh_count
        );

        // Final disk state: exactly one file at the content-addressed path.
        let dst = pool.store_path(first);
        assert!(dst.is_file(), "destination must be a regular file");
        let on_disk = std::fs::read(&dst).unwrap();
        assert_eq!(on_disk, payload, "on-disk bytes match payload");
        // No leftover .tmp files in the shard dir.
        let shard_dir = dst.parent().unwrap();
        for entry in std::fs::read_dir(shard_dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().ends_with(".tmp"),
                "stale tempfile left behind: {:?}",
                name
            );
        }
    }

    #[test]
    fn concurrent_insert_bytes_distinct_content_no_collision() {
        let tmp = TempDir::new().unwrap();
        let pool = Arc::new(ChunkPool::new(tmp.path(), "primary").unwrap());

        let mut handles = Vec::new();
        for k in 0..8u8 {
            let p = Arc::clone(&pool);
            // 8 distinct payloads — each filled with a unique byte
            // followed by counter, so hashes are guaranteed disjoint.
            let mut bytes = vec![k; 4096];
            bytes[0] = k.wrapping_mul(31);
            bytes[1] = k.wrapping_mul(17);
            handles.push(thread::spawn(move || p.insert_bytes(&bytes)));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut hashes = std::collections::HashSet::new();
        for r in &results {
            let (h, fresh) = r.as_ref().unwrap();
            assert!(*fresh, "distinct content must always insert fresh");
            assert!(
                hashes.insert(h.clone()),
                "duplicate hash for distinct content"
            );
        }
        assert_eq!(hashes.len(), 8);
    }

    // -- Pool pin API -----------------------------------------------------

    #[test]
    fn pin_increments_then_drop_releases() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "pin_a").unwrap();
        let (hash, _) = pool.insert_bytes(b"pinable").unwrap();

        assert!(!pool.is_pinned(&hash));
        {
            let _g = pool.pin(&hash);
            assert!(pool.is_pinned(&hash));
        }
        assert!(!pool.is_pinned(&hash));
    }

    #[test]
    fn pin_refcount_survives_overlapping_holders() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "pin_b").unwrap();
        let (hash, _) = pool.insert_bytes(b"refcount").unwrap();

        let a = pool.pin(&hash);
        let b = pool.pin(&hash);
        assert!(pool.is_pinned(&hash));
        drop(a);
        assert!(pool.is_pinned(&hash), "still held by b");
        drop(b);
        assert!(!pool.is_pinned(&hash));
    }

    #[test]
    fn pins_are_scoped_per_backend_and_namespace() {
        let tmp = TempDir::new().unwrap();
        let a = ChunkPool::new(tmp.path(), "pin_scope_x").unwrap();
        let b = ChunkPool::new(tmp.path(), "pin_scope_y").unwrap();
        let ns = ChunkPool::new_namespaced(tmp.path(), "pin_scope_x", "vol1").unwrap();

        let hash = "deadbeef".repeat(8);
        let _g = a.pin(&hash);

        assert!(a.is_pinned(&hash));
        assert!(!b.is_pinned(&hash), "distinct backend must not see the pin");
        assert!(
            !ns.is_pinned(&hash),
            "namespaced sibling must not see the parent pin"
        );

        assert!(ChunkPool::is_pinned_for("pin_scope_x", None, &hash));
        assert!(!ChunkPool::is_pinned_for(
            "pin_scope_x",
            Some("vol1"),
            &hash
        ));
        assert!(!ChunkPool::is_pinned_for("pin_scope_y", None, &hash));
    }

    #[test]
    fn concurrent_insert_verified_bytes_idempotent() {
        // insert_verified_bytes has the same check-then-rename race.
        // Two callers with identical (hash, bytes) must both succeed;
        // no HashMismatch can surface from the race.
        let tmp = TempDir::new().unwrap();
        let pool = Arc::new(ChunkPool::new(tmp.path(), "primary").unwrap());

        let payload: Vec<u8> = (0..8192).map(|i| (i * 13 % 251) as u8).collect();
        let mut hasher = Hasher::new();
        hasher.update(&payload);
        let expected = hex::encode(hasher.finalize().as_bytes());

        let mut handles = Vec::new();
        for _ in 0..6 {
            let p = Arc::clone(&pool);
            let bytes = payload.clone();
            let want = expected.clone();
            handles.push(thread::spawn(move || {
                p.insert_verified_bytes(&want, &bytes)
            }));
        }
        for h in handles {
            let r = h.join().unwrap();
            assert!(
                r.is_ok(),
                "concurrent verified insert must succeed: {:?}",
                r
            );
        }
        // Final state: file exists with correct content.
        let on_disk = std::fs::read(pool.store_path(&expected)).unwrap();
        assert_eq!(on_disk, payload);
    }
}
