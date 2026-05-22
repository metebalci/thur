// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Chunk-seal pipeline on [`Cartridge`].
//!
//! Lifted out of `cartridge/mod.rs` (was a single ~3500-line file
//! post-commit 39b9688) so the seal-state-machine reads on its own.
//! Behaviour-identical move — method names, signatures, body ordering,
//! and fsync sequence are all preserved verbatim. Public method names
//! stay on `Cartridge`; call sites compile unchanged.
//!
//! Covers:
//! - chunk staging allocation (`new_chunk`)
//! - mid-write rolls (`roll_chunk_if_needed`, `maybe_cdc_seal_after_write`)
//! - the seal pipeline (`seal_current_chunk`, `seal_and_start_new_chunk`)
//! - drop-time forced seal (`flush_and_seal`)
//!
//! The six seal-state-machine tests live alongside the methods — they
//! touch private state (`cur_chunk_id`, `cdc_state`, `sealed_bytes`,
//! ...) that only a same-module child test mod can see.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::block_index::derive_iv;
use crate::errors::SmcError;

use super::{
    Cartridge, ChunkRec, ChunkingMode, DedupScope, Result, now_timestamp, open_staging_for_append,
    staging_path,
};

/// Resolve a `ChunkingMode` to its forced-cut threshold (for use in
/// `roll_chunk_if_needed` and capacity accounting). For Fixed mode this
/// is just `size_bytes` (with `0` mapped to `u64::MAX`); for FastCdc
/// mode it's `max` so a single block bigger than the chunker can ever
/// emit doesn't strand the writer.
fn forced_cut_threshold(mode: &ChunkingMode) -> u64 {
    match mode {
        ChunkingMode::Fixed { size_bytes } => {
            if *size_bytes == 0 {
                u64::MAX
            } else {
                *size_bytes
            }
        }
        ChunkingMode::FastCdc { max, .. } => *max,
    }
}

impl Cartridge {
    /// Create a fresh, empty staging chunk: writes an empty
    /// `<root>/.staging/chunk-<id>.dat` and returns a `ChunkRec` with
    /// `hash = None` (it has not been sealed into the pool yet).
    pub(super) fn new_chunk(root: &Path, id: u64) -> Result<ChunkRec> {
        let staging = staging_path(root, id);
        if let Some(parent) = staging.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&staging)?;
        drop(file);

        Ok(ChunkRec::staging())
    }

    /// Seal the current staging chunk into the shared `ChunkStore`.
    /// Hashes the staging file, inserts it under that hash (or drops
    /// `cur` and reuses the existing pool entry on a dedup hit), then
    /// updates the manifest entry for `cur_chunk` with the hash.
    ///
    /// After sealing, the staging file is gone — caller must not append
    /// to it. Empty (size = 0) chunks are not sealed; they're just
    /// dropped from the manifest, since an empty chunk holds no blocks.
    ///
    /// `force` is set by `Cartridge::Drop` / `flush_and_seal` paths
    /// where surfacing `Backpressured` would mean dropping data on the
    /// floor. With `force=true` the pool budget gate is bypassed —
    /// the seal proceeds even if it pushes the pool past the cap.
    /// Bounded overshoot: ≤ chunk_max per concurrent unload (≤32 MiB
    /// under FastCDC defaults). All other call sites (chunk-roll,
    /// content-defined cuts) pass `force=false`.
    /// Seal the active staging chunk into the shared pool.
    ///
    /// Returns `Ok(true)` when an actual seal happened (chunk_id slot
    /// now references a sealed chunk; caller should advance to a fresh
    /// id for the next chunk). Returns `Ok(false)` when the active
    /// chunk was empty — the staging file is dropped, but the
    /// `chunk_index` slot is left untouched so the caller can decide
    /// whether to reuse the id (mid-stream roll over an oversized
    /// block) or truncate it away (flush_and_seal on close).
    fn seal_current_chunk(&mut self, force: bool) -> Result<bool> {
        if self.cur_chunk.hash.is_some() {
            return Ok(true); // already sealed (defensive — shouldn't happen)
        }
        let staging = staging_path(&self.root, self.cur_chunk_id);

        if self.cur_chunk.size == 0 {
            // Nothing was written — drop the staging file. Leave the
            // chunk_index slot intact (no truncate) so the caller can
            // either reuse the id or explicitly drop the slot. No
            // block-index records reference this chunk (size==0 means
            // no writes happened), so reusing the id is safe.
            let _ = fs::remove_file(&staging);
            self.cur_chunk_hasher.reset();
            return Ok(false);
        }

        // Backpressure gate: reserve `cur_chunk.size` bytes against the
        // backend's pool budget *before* the rename. If the pool is at
        // its hard cap and the upload worker can't free space within
        // `backpressure_deadline`, surface `Backpressured` (mapped to
        // SCSI NOT READY at the iSCSI layer; backup software retries).
        // Drop-time flushes pass `force=true` and bypass the gate.
        let staged_bytes = self.cur_chunk.size;
        if force {
            self.pool_budget.force_reserve(staged_bytes);
        } else {
            self.pool_budget
                .try_reserve(staged_bytes, self.backpressure_deadline)?;
        }

        // Make sure all bytes hit disk before the rename. The hash
        // itself comes from the streaming hasher we updated per-block
        // in `write_data` — we do not re-read the chunk to hash it.
        if let Err(e) = self.cur_file.flush() {
            self.pool_budget.release(staged_bytes);
            return Err(e.into());
        }

        // Appliance-side at-rest encryption seam. When the cartridge
        // was opened with a DEK (manifest.encryption is Some), wrap
        // the staged chunk in an AES-256-GCM envelope before pool
        // insertion. The pool — and the cloud objects it eventually
        // syncs — store ciphertext. The dedup hash is therefore over
        // ciphertext: same-content cartridges with different DEKs
        // produce different hashes (no cross-cartridge dedup with
        // at-rest on, same tradeoff VSA accepts). The streaming
        // plaintext hasher we maintained per-block is discarded in
        // this branch.
        //
        // IV is derived from `(cartridge_uuid, chunk_id, 0)` — the
        // third argument is `0` to distinguish from per-block AME
        // IVs (`(uuid, chunk_id, offset)`) and to keep the IV
        // deterministic for the same chunk_id (chunk_ids are
        // monotonic, never reused, so uniqueness under this DEK is
        // guaranteed).
        //
        // `cur_chunk.size` stays as the plaintext byte count even
        // when on-disk bytes include the 16-byte GCM tag —
        // capacity accounting (used_capacity_bytes, EOM/EW) and
        // block-index offsets are plaintext-stream invariants. The
        // ciphertext-overhead delta is a few hundred bytes per
        // multi-MiB chunk, well below pool-budget granularity.
        let hash = if let Some(dek) = self.at_rest_dek {
            let plaintext = std::fs::read(&staging).map_err(|e| {
                self.pool_budget.release(staged_bytes);
                SmcError::from(e)
            })?;
            let iv = derive_iv(&self.manifest.uuid, self.cur_chunk_id, 0);
            let ciphertext = shared_crypto::encrypt_block(&dek, &iv, &plaintext).map_err(|e| {
                self.pool_budget.release(staged_bytes);
                SmcError::EncryptionError(e.to_string())
            })?;
            // Drop the now-stale streaming plaintext hasher; the
            // pool hash is over ciphertext.
            self.cur_chunk_hasher.reset();
            let ct_hash = hex::encode(blake3::hash(&ciphertext).as_bytes());
            // Overwrite the staging file with ciphertext, then insert
            // under the ciphertext hash. The pool's atomic rename
            // moves bytes regardless of content.
            std::fs::write(&staging, &ciphertext).map_err(|e| {
                self.pool_budget.release(staged_bytes);
                SmcError::from(e)
            })?;
            ct_hash
        } else {
            let h = hex::encode(self.cur_chunk_hasher.finalize().as_bytes());
            self.cur_chunk_hasher.reset();
            h
        };

        // Dedup-hit detection: if the destination pool path already
        // exists, `insert_from_path` removes the staging copy and
        // reuses the existing file. The bytes we reserved against the
        // budget never actually consumed disk — release them so we
        // don't leak quota.
        let dedup_hit = self.chunk_store.exists(&hash);
        if let Err(e) = self.chunk_store.insert_from_path(&staging, &hash) {
            self.pool_budget.release(staged_bytes);
            return Err(e.into());
        }
        if dedup_hit {
            self.pool_budget.release(staged_bytes);
        }

        // Update the chunk_index entry in place.
        self.cur_chunk.hash = Some(hash);
        self.chunk_index
            .overwrite(self.cur_chunk_id, &self.cur_chunk)?;

        // Dedup-analytics telemetry. `chunk_seals_total` increments on
        // every successful seal; `chunk_logical_bytes_total` rolls up
        // pre-dedup bytes hosts have written; `chunk_unique_bytes_total`
        // rolls up only the bytes that actually grew the pool.
        // `chunk_dedup_hits_total` covers local-pool dedup at seal time
        // (cloud HEAD-hit dedup is recorded separately at upload time).
        // Operator-facing dedup ratio = logical / unique.
        let backend_name = &self.manifest.backend;
        let scope_str = match self.manifest.dedup {
            DedupScope::Global => "global",
            DedupScope::Local => "local",
        };
        shared_telemetry::record::chunk_seal(backend_name, scope_str);
        shared_telemetry::record::chunk_logical_bytes(backend_name, scope_str, staged_bytes);
        if dedup_hit {
            shared_telemetry::record::chunk_dedup_hit(backend_name, scope_str);
        } else {
            shared_telemetry::record::chunk_unique_bytes(backend_name, scope_str, staged_bytes);
        }

        Ok(true)
    }

    /// Seal the active chunk if appending `to_append` bytes would push
    /// it past the chunking-mode threshold (Fixed: `size_bytes`,
    /// FastCdc: `max`). For FastCdc, *content-defined* cuts are decided
    /// separately by `maybe_cdc_seal_after_write` after the block has
    /// actually been written — this method only enforces the hard
    /// upper bound.
    pub(super) fn roll_chunk_if_needed(&mut self, to_append: u64) -> Result<()> {
        let threshold = forced_cut_threshold(&self.chunking);
        if self.cur_chunk.size + to_append <= threshold {
            return Ok(());
        }
        self.seal_and_start_new_chunk()
    }

    /// Seal the active chunk, reset the FastCDC streaming state if any,
    /// and start a fresh staging chunk for subsequent writes. Used by
    /// both the hard-threshold roll and the FastCDC content-defined
    /// roll.
    ///
    /// After the per-block / per-chunk metadata moved out of
    /// `manifest.json` and into `blocks-pN.idx` + `chunks.idx`, the
    /// manifest itself only carries fields that mutate at LOCATE,
    /// MODE SELECT page 0x11, and FORMAT MEDIUM boundaries — never
    /// at chunk-roll. Chunk-roll persistence is fully covered by
    /// `chunk_index.append`/`overwrite` + fsync below; no
    /// `persist_manifest()` is needed here.
    fn seal_and_start_new_chunk(&mut self) -> Result<()> {
        // Mid-stream chunk roll → backpressure gate is active.
        let sealed = self.seal_current_chunk(false)?;
        if let Some(sc) = self.cdc_state.as_mut() {
            sc.reset();
        }
        let new_id = if sealed {
            // Just-sealed chunk's bytes shift from the live
            // `cur_chunk.size` term into the persisted-and-counted
            // `sealed_bytes` running total. Net total used is
            // unchanged; the bookkeeping just moves between the two
            // halves of `used_capacity_bytes`'s formula.
            self.sealed_bytes = self.sealed_bytes.saturating_add(self.cur_chunk.size);
            // Allocate a fresh slot for the next chunk.
            let id = self.cur_chunk_id + 1;
            let newc = Self::new_chunk(&self.root, id)?;
            self.chunk_index.append(&newc)?;
            self.lru_index.append(now_timestamp())?;
            self.cur_chunk_id = id;
            self.cur_chunk = newc;
            id
        } else {
            // Active chunk was empty — reuse the same slot. The
            // staging file was just deleted; recreate it under the
            // same id and reset the chunk_index entry.
            let newc = Self::new_chunk(&self.root, self.cur_chunk_id)?;
            self.chunk_index.overwrite(self.cur_chunk_id, &newc)?;
            self.lru_index.touch(self.cur_chunk_id, now_timestamp())?;
            self.cur_chunk = newc;
            self.cur_chunk_id
        };
        self.chunk_index.fsync()?;
        self.lru_index.fsync()?;
        self.cur_file = open_staging_for_append(&staging_path(&self.root, new_id))?;
        Ok(())
    }

    /// FastCDC path: feed the just-written block bytes through the
    /// streaming chunker and seal the chunk if the rolling hash matched
    /// a cut. The cut is block-aligned: it's reported at end-of-block
    /// even if the underlying CDC boundary fired earlier in the stream.
    /// No-op for Fixed mode.
    pub(super) fn maybe_cdc_seal_after_write(&mut self, written: &[u8]) -> Result<()> {
        let should_seal = match self.cdc_state.as_mut() {
            Some(sc) => sc.feed(written),
            None => return Ok(()),
        };
        if should_seal {
            self.seal_and_start_new_chunk()?;
        }
        Ok(())
    }

    /// Seal the trailing staging chunk into the shared pool and persist
    /// the manifest. Idempotent: a no-op if the active chunk is already
    /// sealed.
    ///
    /// Empty trailing staging chunks (size == 0, hash == None) are
    /// handled by `seal_current_chunk`: it deletes the staging file
    /// and drops the entry from the manifest. This matters for
    /// read-only opens — every `Cartridge::open(...)` whose manifest
    /// has all chunks already sealed allocates a fresh empty staging
    /// chunk via `resume_or_create_active`. Without this cleanup, that
    /// empty `.staging/chunk-N.dat` leaks across opens and the next
    /// open trips on `create_new(chunk-N.dat)` returning
    /// `AlreadyExists`, which surfaces to the SCSI layer as
    /// `CartridgeNotFound`.
    ///
    /// Exception: a freshly-created cartridge that was never written
    /// has its initial chunk in the same empty-staging shape. We must
    /// preserve it — the manifest needs at least one chunk to be
    /// reopenable. Detect that case by looking for a sealed sibling.
    pub fn flush_and_seal(&mut self) -> Result<()> {
        if self.cur_chunk.hash.is_some() {
            return Ok(());
        }
        if self.cur_chunk.size == 0 {
            // Brand-new cartridge whose only chunk is empty staging
            // must be preserved (chunk_index requires at least one
            // record). Detect by checking for any sealed chunk other
            // than the active one.
            let cur_id = self.cur_chunk_id;
            let has_sealed_predecessor = self.chunk_index.iter().any(|entry| match entry {
                Ok((id, c)) => id != cur_id && c.hash.is_some(),
                Err(_) => false,
            });
            if !has_sealed_predecessor {
                return Ok(());
            }
            // Trailing empty chunk with sealed predecessors — drop
            // both the staging file and the chunk_index slot. The
            // empty record is always at `cur_chunk_id` (the last
            // index), so truncate_to(cur_chunk_id) drops exactly it.
            let _ = fs::remove_file(staging_path(&self.root, cur_id));
            self.chunk_index.truncate_to(cur_id)?;
            self.lru_index.truncate_to(cur_id)?;
            for bif in &self.block_indexes {
                bif.fsync()?;
            }
            self.chunk_index.fsync()?;
            self.lru_index.fsync()?;
            return Ok(());
        }
        // Drop / unload boundary: force the seal past backpressure so
        // we never lose the trailing chunk to a full pool.
        let _ = self.seal_current_chunk(true)?;
        // Force block-index and chunk-index to disk: this is the
        // unload boundary, callers expect on-disk state to reflect
        // every recorded block / filemark / chunk after this returns.
        // No `persist_manifest()` — flush_and_seal mutates no
        // serialized manifest field; partition / format mutations
        // already persisted at their own boundaries.
        for bif in &self.block_indexes {
            bif.fsync()?;
        }
        self.chunk_index.fsync()?;
        self.lru_index.fsync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Pins the seal-state-machine's observable behaviour. Originally
    //! landed in `cartridge/mod.rs::seal_state_machine_tests` in commit
    //! 8773095 to lock the contract pre-extraction; moved here alongside
    //! the methods they cover so private-state assertions stay valid
    //! after the lift.
    //!
    //! 1. empty-chunk defensive path (size==0 → Ok(false), no id advance)
    //! 2. happy path (hash + id + sealed_bytes accounting)
    //! 3. force=true bypasses pool-budget gate
    //! 4. dedup-hit releases the reserved budget
    //! 5. FastCDC streaming chunker is reset after a roll
    //! 6. flush_and_seal on an empty fresh cartridge is a no-op

    use super::*;
    use bytes::Bytes;
    use shared_pool::PoolBudget;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Build a cartridge with the requested chunking mode under
    /// `<tmp>/tapes/<label>/`. Mirrors the public-API shape used by
    /// the existing `core/smc/tests/backpressure_tests.rs::cart_with_budget`.
    fn fresh_cart(tmp: &TempDir, label: &str, mode: ChunkingMode) -> Cartridge {
        let tapes = tmp.path().join("tapes");
        Cartridge::create_with_chunking(
            &tapes,
            label,
            mode,
            8, // lto_generation
            "primary",
            false,
            DedupScope::Global,
        )
        .expect("create_with_chunking")
    }

    /// PoolBudget tied to the cartridge's data_dir. Held by the test as
    /// an Arc so `current_bytes()` can be observed after seals — the
    /// daemon-side wiring works exactly this way (one Arc, plumbed
    /// into every cartridge via `set_pool_budget`).
    fn budget(tmp: &TempDir, cap_bytes: u64) -> Arc<PoolBudget> {
        Arc::new(PoolBudget::new(tmp.path().to_path_buf(), cap_bytes, 0, 80))
    }

    #[test]
    fn seal_empty_chunk_returns_false_reuses_id() {
        let tmp = TempDir::new().unwrap();
        let mut cart = fresh_cart(
            &tmp,
            "EMPTY_SEAL",
            ChunkingMode::Fixed {
                size_bytes: 128 * 1024,
            },
        );

        // Initial state from create_with_chunking: cur_chunk_id == 0,
        // size == 0, no hash, staging file exists.
        let initial_id = cart.cur_chunk_id;
        assert_eq!(initial_id, 0);
        assert_eq!(cart.cur_chunk.size, 0);
        assert!(cart.cur_chunk.hash.is_none());
        let staging = staging_path(&cart.root, initial_id);
        assert!(
            staging.exists(),
            "fresh cartridge should have a staging file"
        );

        let sealed = cart.seal_current_chunk(false).expect("seal");
        assert!(!sealed, "empty chunk must not be reported as sealed");

        // The defensive path:
        //   - id NOT advanced (caller may reuse it)
        //   - staging file removed
        //   - chunk_index slot left intact (no truncate)
        //   - hash still None
        assert_eq!(cart.cur_chunk_id, initial_id);
        assert!(!staging.exists(), "empty-seal must drop the staging file");
        assert!(cart.cur_chunk.hash.is_none());
    }

    #[test]
    fn seal_nonempty_chunk_updates_hash_and_advances_id() {
        let tmp = TempDir::new().unwrap();
        // 1 MiB Fixed → 64 KiB write stays well under the auto-roll
        // threshold; the only seal is the one we trigger explicitly.
        let mut cart = fresh_cart(
            &tmp,
            "FULL_SEAL",
            ChunkingMode::Fixed {
                size_bytes: 1024 * 1024,
            },
        );

        cart.write_data(Bytes::from(vec![0xAB; 64 * 1024]))
            .expect("write_data");
        assert_eq!(cart.cur_chunk_id, 0);
        assert_eq!(cart.cur_chunk.size, 64 * 1024);
        assert!(cart.cur_chunk.hash.is_none());
        assert_eq!(cart.sealed_bytes, 0);

        cart.seal_and_start_new_chunk()
            .expect("seal_and_start_new_chunk");

        // Slot 0: sealed with a hash; slot 1: fresh staging.
        assert_eq!(cart.cur_chunk_id, 1, "id must advance after seal");
        assert_eq!(cart.sealed_bytes, 64 * 1024);
        assert!(cart.cur_chunk.hash.is_none(), "new slot must be unsealed");
        assert_eq!(cart.cur_chunk.size, 0);
        let prev_rec = cart.read_chunk_rec(0).expect("read prev rec");
        assert!(
            prev_rec.hash.is_some(),
            "previous slot must carry a sealed hash"
        );
        assert!(
            staging_path(&cart.root, 1).exists(),
            "new staging file must exist at the advanced id"
        );
    }

    #[test]
    fn force_bypass_pool_budget() {
        let tmp = TempDir::new().unwrap();
        let mut cart = fresh_cart(
            &tmp,
            "FORCE_BYPASS",
            ChunkingMode::Fixed {
                size_bytes: 1024 * 1024,
            },
        );
        // 1-byte cap: any non-zero reservation would `Backpressured` on
        // try_reserve; force_reserve must bypass.
        let b = budget(&tmp, 1);
        cart.set_pool_budget(b.clone(), Duration::from_millis(100));

        cart.write_data(Bytes::from(vec![0xCD; 4096]))
            .expect("write_data");
        assert_eq!(cart.cur_chunk.size, 4096);

        cart.seal_current_chunk(true)
            .expect("force seal must succeed despite tight budget");
        assert!(
            cart.cur_chunk.hash.is_some(),
            "force seal must persist hash"
        );
        assert!(
            b.current_bytes() >= 4096,
            "force_reserve must record 4096 bytes against the budget; got {}",
            b.current_bytes()
        );
    }

    #[test]
    fn dedup_hit_releases_budget() {
        // Two cartridges in the same tapes_dir share a Global-scope
        // chunk pool. Identical payloads -> second seal is a dedup hit.
        // Verify the budget reservation made by the second seal is
        // released (net used bytes stays at one chunk's worth).
        let tmp = TempDir::new().unwrap();
        let b = budget(&tmp, 1024 * 1024);
        let tapes = tmp.path().join("tapes");

        let mut cart_a = Cartridge::create_with_chunking(
            &tapes,
            "DEDUP_A",
            ChunkingMode::Fixed {
                size_bytes: 1024 * 1024,
            },
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .expect("create A");
        cart_a.set_pool_budget(b.clone(), Duration::from_secs(1));

        let mut cart_b = Cartridge::create_with_chunking(
            &tapes,
            "DEDUP_B",
            ChunkingMode::Fixed {
                size_bytes: 1024 * 1024,
            },
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .expect("create B");
        cart_b.set_pool_budget(b.clone(), Duration::from_secs(1));

        let payload = Bytes::from(vec![0x42; 4096]);
        cart_a.write_data(payload.clone()).unwrap();
        cart_b.write_data(payload).unwrap();

        // Force-seal both. Without force, the second seal could
        // succeed normally too, but force keeps the test deterministic
        // regardless of cap.
        cart_a.seal_current_chunk(true).expect("seal A");
        let after_first = b.current_bytes();
        assert_eq!(
            after_first, 4096,
            "first seal reserves one chunk's bytes; got {after_first}"
        );

        cart_b.seal_current_chunk(true).expect("seal B (dedup hit)");
        let after_second = b.current_bytes();
        assert_eq!(
            after_second, 4096,
            "dedup-hit seal must release its reservation; got {after_second}"
        );

        // Sanity: only one unique chunk in the pool.
        assert_eq!(cart_a.referenced_chunk_hashes().len(), 1);
        assert_eq!(
            cart_a.referenced_chunk_hashes(),
            cart_b.referenced_chunk_hashes(),
            "both cartridges must reference the same content hash"
        );
    }

    #[test]
    fn cdc_state_reset_after_seal_and_start() {
        // Small-ish FastCDC envelope so the test runs fast but the
        // write below stays well under `min` (16 KiB) — no CDC cut
        // fires during the write, so the only seal is the explicit
        // one we trigger.
        let tmp = TempDir::new().unwrap();
        let mut cart = fresh_cart(
            &tmp,
            "CDC_RESET",
            ChunkingMode::FastCdc {
                min: 16 * 1024,
                avg: 64 * 1024,
                max: 256 * 1024,
            },
        );

        cart.write_data(Bytes::from(vec![0xEF; 8 * 1024]))
            .expect("write_data");
        let pos_before = cart
            .cdc_state
            .as_ref()
            .expect("FastCDC mode must initialize cdc_state")
            .pos();
        assert!(
            pos_before > 0,
            "post-write cdc_state.pos must reflect consumed bytes; got 0"
        );

        cart.seal_and_start_new_chunk().expect("seal");
        let pos_after = cart
            .cdc_state
            .as_ref()
            .expect("cdc_state must still exist after seal")
            .pos();
        assert_eq!(
            pos_after, 0,
            "seal_and_start_new_chunk must reset the streaming chunker; pos is {pos_after}"
        );
    }

    #[test]
    fn flush_and_seal_empty_fresh_cartridge() {
        // The "brand-new cartridge whose only chunk is empty staging"
        // branch in flush_and_seal must be a no-op:
        // chunk_index keeps its initial empty record so reopen works,
        // no sealed chunks land in the pool, no panic.
        let tmp = TempDir::new().unwrap();
        let mut cart = fresh_cart(
            &tmp,
            "EMPTY_FLUSH",
            ChunkingMode::Fixed {
                size_bytes: 128 * 1024,
            },
        );

        assert_eq!(cart.cur_chunk.size, 0);
        assert!(cart.cur_chunk.hash.is_none());
        let initial_id = cart.cur_chunk_id;

        cart.flush_and_seal().expect("flush_and_seal");

        // No sealed chunks, no advance, hash still absent.
        assert_eq!(cart.cur_chunk_id, initial_id);
        assert!(cart.cur_chunk.hash.is_none());
        assert!(cart.referenced_chunk_hashes().is_empty());
        assert_eq!(cart.next_lba(), 0);
    }
}
