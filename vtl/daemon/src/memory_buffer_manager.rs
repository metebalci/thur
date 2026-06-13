// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Memory Buffer Manager
//
// Per-tape RAM-staged read/write buffer tracking and dispatcher for
// upload / prefetch / eviction work. Distinct from the on-disk
// `DiskCacheManager` (the shared content-addressed chunk pool).

#![allow(dead_code)] // Memory buffer management infrastructure

use anyhow::Result;
use core_mediachanger::TapeEvent;
use std::collections::{HashMap, HashSet};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Upload request sent from MemoryBufferManager to upload worker
#[derive(Debug, Clone)]
pub struct UploadRequest {
    /// Tape ID
    pub tape_id: String,
    /// Chunk IDs to upload (in order of priority - oldest first)
    pub chunk_ids: Vec<u32>,
}

/// Prefetch request sent from MemoryBufferManager to prefetch worker
#[derive(Debug, Clone)]
pub struct PrefetchRequest {
    /// Tape ID
    pub tape_id: String,
    /// Chunk IDs to prefetch (in order)
    pub chunk_ids: Vec<u32>,
}

/// Eviction request sent from MemoryBufferManager to eviction handler
#[derive(Debug, Clone)]
pub struct EvictionRequest {
    /// Tape ID
    pub tape_id: String,
    /// Current head position (evict chunks before this LBA)
    pub head_position: u64,
}

/// Per-tape buffer state
///
/// Tracks read/write buffer usage, pending uploads, and prefetch state
/// for a single tape cartridge.
#[derive(Debug, Clone)]
pub struct TapeBufferState {
    /// Tape ID (label)
    pub tape_id: String,
    /// Current tape head position (LBA)
    pub head_position: u64,
    /// Drive number if loaded, None if in slot
    pub loaded_drive: Option<u8>,

    // Write buffer tracking
    /// Bytes in write buffer (not yet dispatched for upload)
    pub write_buffer_usage: u64,
    /// Write buffer limit (per-tape)
    pub write_buffer_limit: u64,
    /// Chunk IDs pending S3 upload
    pub pending_uploads: HashSet<u32>,
    /// Per-chunk byte tally for chunks in `pending_uploads`. Lets us
    /// decrement `write_buffer_usage` correctly when a batch is
    /// dispatched, instead of letting the counter grow monotonically
    /// (which permanently parks the tape over its buffer limit and
    /// re-fires `trigger_upload_batch` on every block).
    pub chunk_bytes: HashMap<u32, u64>,

    // Read buffer tracking
    /// Bytes in read buffer (prefetched from S3)
    pub read_buffer_usage: u64,
    /// Read buffer limit (per-tape)
    pub read_buffer_limit: u64,
    /// Prefetched chunk IDs
    pub prefetched_chunks: HashSet<u32>,
    /// Last chunk read (for sequential detection)
    pub last_read_chunk: Option<u32>,
}

impl TapeBufferState {
    /// Create new buffer state for a tape
    pub fn new(tape_id: String, write_limit: u64, read_limit: u64) -> Self {
        Self {
            tape_id,
            head_position: 0,
            loaded_drive: None,
            write_buffer_usage: 0,
            write_buffer_limit: write_limit,
            pending_uploads: HashSet::new(),
            chunk_bytes: HashMap::new(),
            read_buffer_usage: 0,
            read_buffer_limit: read_limit,
            prefetched_chunks: HashSet::new(),
            last_read_chunk: None,
        }
    }
}

fn clear_prefetch_and_break_sequence(tape: &mut TapeBufferState) {
    tape.prefetched_chunks.clear();
    tape.last_read_chunk = None;
}

/// Buffer Manager
///
/// Manages per-tape read/write buffers and coordinates S3 uploads/prefetch.
/// Phase 3: Tracks buffer usage per tape
/// Phase 4: Event-driven uploads via upload_tx channel
/// Phase 5: Event-driven prefetch via prefetch_tx channel
pub struct MemoryBufferManager {
    event_rx: broadcast::Receiver<TapeEvent>,
    /// Per-tape buffer state
    tapes: HashMap<String, TapeBufferState>,
    /// Running sum of `write_buffer_usage` across all tapes. Reported as
    /// the library-wide `tape_write_buffer_used` gauge instead of one
    /// series per cartridge, which is unbounded over the library's life
    /// and overflows the OTel cardinality cap past ~2000 cartridges
    /// (issue #205).
    total_write_buffer_usage: u64,
    /// Default write buffer limit per tape
    write_buffer_limit: u64,
    /// Default read buffer limit per tape
    read_buffer_limit: u64,
    /// Channel to send upload requests to upload worker
    upload_tx: mpsc::Sender<UploadRequest>,
    /// Channel to send prefetch requests to prefetch worker
    prefetch_tx: mpsc::Sender<PrefetchRequest>,
}

impl MemoryBufferManager {
    /// Create a new MemoryBufferManager. `write_buffer_limit` and
    /// `read_buffer_limit` are per-tape byte counts already resolved
    /// out of `memory_buffers_size::MemoryBuffersSize::resolve_bytes`
    /// by the caller — auto vs explicit decisions and the host-RAM
    /// safety check both live in `main.rs` so this constructor stays
    /// a pure byte sink.
    pub fn new(
        event_rx: broadcast::Receiver<TapeEvent>,
        write_buffer_limit: u64,
        read_buffer_limit: u64,
        upload_tx: mpsc::Sender<UploadRequest>,
        prefetch_tx: mpsc::Sender<PrefetchRequest>,
    ) -> Self {
        info!(
            "MemoryBufferManager created (write_buffer={} bytes, read_buffer={} bytes per tape)",
            write_buffer_limit, read_buffer_limit
        );
        Self {
            event_rx,
            tapes: HashMap::new(),
            total_write_buffer_usage: 0,
            write_buffer_limit,
            read_buffer_limit,
            upload_tx,
            prefetch_tx,
        }
    }

    /// Get or create buffer state for a tape
    fn get_or_create_tape(&mut self, tape_id: &str) -> &mut TapeBufferState {
        self.tapes.entry(tape_id.to_string()).or_insert_with(|| {
            debug!("Creating buffer state for tape {}", tape_id);
            TapeBufferState::new(
                tape_id.to_string(),
                self.write_buffer_limit,
                self.read_buffer_limit,
            )
        })
    }

    /// Run the buffer manager event loop
    ///
    /// Subscribes to tape events and processes them.
    pub async fn run(mut self) -> Result<()> {
        info!("MemoryBufferManager started - listening for tape events");

        loop {
            match self.event_rx.recv().await {
                Ok(event) => {
                    self.handle_event(event).await;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // The bus dropped events under us; we can't replay
                    // the missed BlockWritten signals. Sweep every tape
                    // we already know about so chunks already tracked
                    // in `pending_uploads` still get dispatched, even
                    // though we may have undercounted bytes for the
                    // skipped window. Chunks emitted *only* in the
                    // dropped window are out of view here; the
                    // upload-completion / next-event tick is the
                    // backstop for those.
                    warn!(
                        "MemoryBufferManager lagged, skipped {} events - sweeping {} known tapes",
                        skipped,
                        self.tapes.len()
                    );
                    let tape_ids: Vec<String> = self.tapes.keys().cloned().collect();
                    for tape_id in tape_ids {
                        self.trigger_upload_batch(&tape_id);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Event channel closed, MemoryBufferManager shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle a single tape event
    async fn handle_event(&mut self, event: TapeEvent) {
        match event {
            TapeEvent::CartridgeLoaded { tape_id, drive_num } => {
                self.on_cartridge_loaded(&tape_id, drive_num);
            }
            TapeEvent::CartridgeUnloaded { tape_id, drive_num } => {
                self.on_cartridge_unloaded(&tape_id, drive_num).await;
            }
            TapeEvent::BlockWritten {
                tape_id,
                chunk_id,
                lba,
                size,
            } => {
                self.on_block_written(&tape_id, chunk_id, lba, size).await;
            }
            TapeEvent::BlockRead {
                tape_id,
                chunk_id,
                lba,
            } => {
                self.on_block_read(&tape_id, chunk_id, lba).await;
            }
            TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason,
            } => {
                self.on_head_position_changed(&tape_id, old_lba, new_lba, reason);
            }
        }
    }

    /// Handle cartridge loaded event
    fn on_cartridge_loaded(&mut self, tape_id: &str, drive_num: u8) {
        info!("Cartridge {} loaded into drive {}", tape_id, drive_num);
        let tape = self.get_or_create_tape(tape_id);
        tape.loaded_drive = Some(drive_num);
        tape.head_position = 0; // Reset to BOT
    }

    /// Handle cartridge unloaded event
    async fn on_cartridge_unloaded(&mut self, tape_id: &str, drive_num: u8) {
        info!("Cartridge {} unloaded from drive {}", tape_id, drive_num);
        let has_pending = self
            .tapes
            .get_mut(tape_id)
            .map(|tape| {
                tape.loaded_drive = None;
                !tape.pending_uploads.is_empty()
            })
            .unwrap_or(false);
        if has_pending {
            info!("Flushing pending uploads for {} before unload", tape_id);
            // Blocking drain: dispatch every pending chunk (batched at
            // UPLOAD_MAX_BATCH_SIZE) before the per-load state is
            // cleared below. The unload path can afford to block on a
            // full worker queue; the steady-state path cannot.
            self.flush_all_pending(tape_id).await;
        }
        // Reset volatile per-load state. write_buffer_usage and the
        // chunk_bytes side map are bookkeeping, not durability — the
        // upload pipeline owns chunk durability via `chunks.idx`.
        // Leaving them populated would carry stale accounting into the
        // next load.
        let cleared = if let Some(tape) = self.tapes.get_mut(tape_id) {
            let cleared = tape.write_buffer_usage;
            tape.write_buffer_usage = 0;
            tape.pending_uploads.clear();
            tape.chunk_bytes.clear();
            tape.read_buffer_usage = 0;
            tape.prefetched_chunks.clear();
            tape.last_read_chunk = None;
            cleared
        } else {
            0
        };
        self.total_write_buffer_usage = self.total_write_buffer_usage.saturating_sub(cleared);
        shared_telemetry::record::tape_write_buffer_used(self.total_write_buffer_usage);
        self.publish_upload_queue_depth();
    }

    /// Handle block written event
    async fn on_block_written(&mut self, tape_id: &str, chunk_id: u32, lba: u64, size: u64) {
        debug!(
            "Block written: tape={} chunk={} lba={} size={}",
            tape_id, chunk_id, lba, size
        );
        let warn_payload = {
            let tape = self.get_or_create_tape(tape_id);

            // Update write buffer usage
            tape.write_buffer_usage += size;
            *tape.chunk_bytes.entry(chunk_id).or_insert(0) += size;
            tape.head_position = lba + 1; // Advance head

            // Track chunk for upload
            if tape.pending_uploads.insert(chunk_id) {
                debug!(
                    "Chunk {} added to pending uploads for {}",
                    chunk_id, tape_id
                );
            }

            if tape.write_buffer_usage >= tape.write_buffer_limit {
                Some((
                    tape.write_buffer_usage,
                    tape.write_buffer_limit,
                    tape.pending_uploads.len(),
                ))
            } else {
                None
            }
        };
        self.total_write_buffer_usage += size;
        shared_telemetry::record::tape_write_buffer_used(self.total_write_buffer_usage);
        self.publish_upload_queue_depth();

        if let Some((usage, limit, pending)) = warn_payload {
            warn!(
                "Write buffer full for {}: {} / {} bytes ({} pending uploads)",
                tape_id, usage, limit, pending
            );
            self.trigger_upload_batch(tape_id);
        }
    }

    /// Handle block read event
    async fn on_block_read(&mut self, tape_id: &str, chunk_id: u32, lba: u64) {
        debug!(
            "Block read: tape={} chunk={} lba={}",
            tape_id, chunk_id, lba
        );

        // Determine actions before borrowing tape
        let (should_prefetch, should_evict) = {
            let tape = self.get_or_create_tape(tape_id);

            // Detect sequential read pattern. `chunk_id == last + 1` —
            // use `checked_add` so a `last == u32::MAX` never panics
            // in debug or wraps in release. Re-reading the same chunk
            // (`chunk_id == last`) is *not* sequential; it's a re-read
            // and shouldn't trigger prefetch.
            let is_sequential = tape
                .last_read_chunk
                .and_then(|last| last.checked_add(1))
                .map(|next| chunk_id == next)
                .unwrap_or(false);

            let needs_eviction = tape.read_buffer_usage > tape.read_buffer_limit;

            (is_sequential, needs_eviction)
        };

        // Now update tape state
        let tape = self.get_or_create_tape(tape_id);
        tape.head_position = lba + 1; // Advance head
        tape.last_read_chunk = Some(chunk_id);

        // Trigger prefetch if sequential
        if should_prefetch {
            debug!(
                "Sequential read detected for {}: chunk {}",
                tape_id, chunk_id
            );
            self.trigger_prefetch(tape_id, chunk_id + 1);
        }

        // Trigger eviction if read buffer full
        if should_evict {
            warn!(
                "Read buffer full for {}: {} / {} bytes",
                tape_id,
                self.tapes
                    .get(tape_id)
                    .map(|t| t.read_buffer_usage)
                    .unwrap_or(0),
                self.read_buffer_limit
            );
            self.evict_behind_head(tape_id);
        }
    }

    /// Handle head position changed event
    fn on_head_position_changed(
        &mut self,
        tape_id: &str,
        old_lba: u64,
        new_lba: u64,
        reason: core_mediachanger::PositionChangeReason,
    ) {
        debug!(
            "Head position changed: tape={} {}->{} ({:?})",
            tape_id, old_lba, new_lba, reason
        );
        let tape = self.get_or_create_tape(tape_id);
        tape.head_position = new_lba;

        // Cancel prefetch on non-sequential operations (Phase 5)
        use core_mediachanger::PositionChangeReason;
        match reason {
            PositionChangeReason::Rewind | PositionChangeReason::Locate => {
                if !tape.prefetched_chunks.is_empty() {
                    info!(
                        "Canceling {} prefetched chunks for {} (reason: {:?})",
                        tape.prefetched_chunks.len(),
                        tape_id,
                        reason
                    );
                }
                clear_prefetch_and_break_sequence(tape);
            }
            PositionChangeReason::Space => {
                // SPACE may be sequential (skip records) or not — clear to be safe.
                if !tape.prefetched_chunks.is_empty() {
                    debug!(
                        "Clearing {} prefetched chunks for {} (SPACE operation)",
                        tape.prefetched_chunks.len(),
                        tape_id
                    );
                }
                clear_prefetch_and_break_sequence(tape);
            }
            PositionChangeReason::SequentialRead | PositionChangeReason::SequentialWrite => {
                // Sequential operations don't break pattern
            }
        }
    }

    /// Trigger upload batch for a tape (Phase 4: Event-Driven Uploads)
    ///
    /// Selects up to `MAX_BATCH_SIZE` of the oldest pending chunks and
    /// **non-blockingly** enqueues them to the upload worker. The
    /// dispatched chunk IDs are removed from `pending_uploads` (and
    /// their bytes decremented from the per-tape write-buffer
    /// accounting) *only* once the request is accepted by the worker's
    /// queue.
    ///
    /// Decoupled from upload-worker backpressure on purpose: this runs
    /// on the broadcast event-ingestion path, so blocking on a full
    /// `upload_tx` (the previous `send().await`) would stall
    /// `event_rx.recv()` until the broadcast ring overflowed
    /// (`RecvError::Lagged`), dropping `BlockWritten` events and
    /// permanently undercounting buffered bytes. Instead, a full queue
    /// leaves the chunks pending for the next event / upload-completion
    /// tick to retry; host-write backpressure is still applied upstream
    /// via the per-tape write-buffer limit (chunks that don't dispatch
    /// keep `write_buffer_usage` high).
    fn trigger_upload_batch(&mut self, tape_id: &str) {
        let chunk_ids = self.select_upload_batch(tape_id);
        if chunk_ids.is_empty() {
            return;
        }
        let request = UploadRequest {
            tape_id: tape_id.to_string(),
            chunk_ids: chunk_ids.clone(),
        };
        match self.upload_tx.try_send(request) {
            // Accepted — commit the pending removal + byte accounting.
            // The upload worker owns chunk durability from here
            // (`chunks.idx`'s `uploaded` flag + HEAD-skip on retry); a
            // dispatched-then-failed chunk reappears on a later trigger
            // via the crash-recovery scan.
            Ok(()) => self.commit_upload_dispatch(tape_id, &chunk_ids),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Worker saturated; leave the chunks pending and retry
                // on the next tick. No bus blocking, no byte drift.
                debug!(
                    "Upload worker queue full for {}; {} chunks stay pending (retry next tick)",
                    tape_id,
                    chunk_ids.len()
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Failed to send upload request for {} (upload channel closed)",
                    tape_id
                );
            }
        }
    }

    /// Maximum chunks dispatched to the upload worker per request.
    const UPLOAD_MAX_BATCH_SIZE: usize = 8;

    /// Pick up to [`Self::UPLOAD_MAX_BATCH_SIZE`] of the oldest pending
    /// chunk ids for `tape_id` *without* mutating state. The caller
    /// commits the removal via [`Self::commit_upload_dispatch`] only
    /// once the corresponding request is actually accepted by the
    /// upload worker, so a rejected (queue-full) enqueue leaves the
    /// pending set untouched.
    fn select_upload_batch(&self, tape_id: &str) -> Vec<u32> {
        let Some(tape) = self.tapes.get(tape_id) else {
            return Vec::new();
        };
        let mut chunk_ids: Vec<u32> = tape.pending_uploads.iter().copied().collect();
        chunk_ids.sort_unstable();
        chunk_ids.truncate(Self::UPLOAD_MAX_BATCH_SIZE);
        chunk_ids
    }

    /// Remove a dispatched batch from `pending_uploads` and decrement
    /// the per-tape write-buffer accounting by its byte total. Called
    /// only after the request was accepted by the upload worker.
    fn commit_upload_dispatch(&mut self, tape_id: &str, chunk_ids: &[u32]) {
        let (dispatched_bytes, decrease, new_usage) = {
            let Some(tape) = self.tapes.get_mut(tape_id) else {
                return;
            };
            let mut dispatched_bytes: u64 = 0;
            for cid in chunk_ids {
                tape.pending_uploads.remove(cid);
                if let Some(b) = tape.chunk_bytes.remove(cid) {
                    dispatched_bytes = dispatched_bytes.saturating_add(b);
                }
            }
            let before = tape.write_buffer_usage;
            tape.write_buffer_usage = before.saturating_sub(dispatched_bytes);
            // The library-wide total must move by the actual decrease,
            // which `saturating_sub` may clamp below `dispatched_bytes`.
            (dispatched_bytes, before - tape.write_buffer_usage, tape.write_buffer_usage)
        };
        self.total_write_buffer_usage = self.total_write_buffer_usage.saturating_sub(decrease);
        shared_telemetry::record::tape_write_buffer_used(self.total_write_buffer_usage);
        info!(
            "Dispatched upload batch for {}: {} chunks ({} bytes), per-tape write_buffer_usage now {}",
            tape_id,
            chunk_ids.len(),
            dispatched_bytes,
            new_usage,
        );
        self.publish_upload_queue_depth();
    }

    /// Push the current daemon-wide upload backlog — the sum of every
    /// loaded tape's pending-upload set — to the `upload_queue_depth`
    /// gauge. Called after every mutation of any tape's `pending_uploads`
    /// (chunk-seal insert, dispatch removal, unload clear), mirroring the
    /// absolute-push idiom of `iscsi_sessions_active` / `prefetch_queue_depth`.
    fn publish_upload_queue_depth(&self) {
        shared_telemetry::record::upload_queue_depth(self.current_upload_queue_depth());
    }

    /// Daemon-wide upload backlog: the sum of every loaded tape's
    /// pending-upload set. The value pushed to the gauge.
    fn current_upload_queue_depth(&self) -> i64 {
        self.tapes
            .values()
            .map(|t| t.pending_uploads.len())
            .sum::<usize>() as i64
    }

    /// Blocking drain used only on the cartridge-unload path: dispatch
    /// every pending chunk to the worker before the per-load state is
    /// cleared. Blocking `send().await` is acceptable here — unload is
    /// a rare, explicit event, and we must not strand a tape's tail
    /// behind a full queue right before forgetting its pending set. The
    /// hot `BlockWritten` path uses the non-blocking
    /// [`Self::trigger_upload_batch`] instead.
    async fn flush_all_pending(&mut self, tape_id: &str) {
        loop {
            let chunk_ids = self.select_upload_batch(tape_id);
            if chunk_ids.is_empty() {
                break;
            }
            let request = UploadRequest {
                tape_id: tape_id.to_string(),
                chunk_ids: chunk_ids.clone(),
            };
            if self.upload_tx.send(request).await.is_err() {
                warn!(
                    "Upload channel closed during unload flush for {} ({} chunks stranded)",
                    tape_id,
                    chunk_ids.len()
                );
                break;
            }
            self.commit_upload_dispatch(tape_id, &chunk_ids);
        }
    }

    /// Trigger prefetch for a tape (Phase 5: Event-Driven Prefetch)
    fn trigger_prefetch(&mut self, tape_id: &str, start_chunk_id: u32) {
        let needs_prefetch: Vec<u32> = {
            let Some(tape) = self.tapes.get(tape_id) else {
                return;
            };
            // Prefetch next 1-2 chunks
            const PREFETCH_COUNT: u32 = 2;
            (start_chunk_id..start_chunk_id + PREFETCH_COUNT)
                .filter(|id| !tape.prefetched_chunks.contains(id))
                .collect()
        };

        if needs_prefetch.is_empty() {
            return;
        }

        debug!(
            "Triggering prefetch for {}: chunks {:?}",
            tape_id, needs_prefetch
        );

        let request = PrefetchRequest {
            tape_id: tape_id.to_string(),
            chunk_ids: needs_prefetch,
        };

        // Non-blocking, like `trigger_upload_batch`: this runs on the
        // broadcast event-ingestion path, so it must not park and lag
        // the bus. Prefetch is best-effort read-ahead — a full queue
        // just drops this hint and the read path fetches on miss.
        match self.prefetch_tx.try_send(request) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!(
                    "Prefetch queue full for {}; dropping read-ahead hint",
                    tape_id
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Failed to send prefetch request for {} (prefetch channel closed)",
                    tape_id
                );
            }
        }
    }

    /// Evict chunks behind the tape head (Phase 5: Event-Driven Prefetch)
    ///
    /// Removes old chunks from read buffer when capacity is exceeded.
    /// Only evicts chunks that:
    /// - Have LBA < head_position (behind the head)
    /// - Are already uploaded to S3 (safe to delete locally)
    fn evict_behind_head(&mut self, tape_id: &str) {
        if let Some(tape) = self.tapes.get_mut(tape_id) {
            let head_pos = tape.head_position;

            // For now, just clear prefetched chunks (simplified eviction)
            // Full implementation would need to:
            // 1. Find chunks with LBA < head_position
            // 2. Check if uploaded to S3
            // 3. Delete local chunk files
            // 4. Update read_buffer_usage
            //
            // This is a simplified version that just clears the prefetch tracking
            if !tape.prefetched_chunks.is_empty() {
                let count = tape.prefetched_chunks.len();
                tape.prefetched_chunks.clear();
                debug!(
                    "Evicted {} prefetched chunks for {} (head at LBA {})",
                    count, tape_id, head_pos
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::PositionChangeReason;

    /// Build a manager with small (1 GiB) per-tape limits and 4-deep
    /// channels. Returns the manager plus the receivers so a test can
    /// drain dispatched upload/prefetch requests.
    fn make_manager() -> (
        MemoryBufferManager,
        mpsc::Receiver<UploadRequest>,
        mpsc::Receiver<PrefetchRequest>,
    ) {
        let (_event_tx, event_rx) = broadcast::channel(16);
        let (upload_tx, upload_rx) = mpsc::channel(8);
        let (prefetch_tx, prefetch_rx) = mpsc::channel(8);
        // 1 GiB each — large enough that the test writes (≤ ~10 KiB)
        // never hit the buffer-full watermark, matching the pre-2026-05
        // GB-based ctor that passed (1, 1) GB.
        let one_gib = 1024 * 1024 * 1024;
        let mgr = MemoryBufferManager::new(event_rx, one_gib, one_gib, upload_tx, prefetch_tx);
        (mgr, upload_rx, prefetch_rx)
    }

    #[test]
    fn tape_buffer_state_new_starts_empty() {
        let st = TapeBufferState::new("TAPE001".to_string(), 100, 200);
        assert_eq!(st.tape_id, "TAPE001");
        assert_eq!(st.head_position, 0);
        assert!(st.loaded_drive.is_none());
        assert_eq!(st.write_buffer_usage, 0);
        assert_eq!(st.write_buffer_limit, 100);
        assert_eq!(st.read_buffer_limit, 200);
        assert!(st.pending_uploads.is_empty());
        assert!(st.chunk_bytes.is_empty());
        assert!(st.last_read_chunk.is_none());
    }

    #[test]
    fn clear_prefetch_resets_sequence_tracking() {
        let mut st = TapeBufferState::new("T".to_string(), 1, 1);
        st.prefetched_chunks.insert(7);
        st.last_read_chunk = Some(5);
        clear_prefetch_and_break_sequence(&mut st);
        assert!(st.prefetched_chunks.is_empty());
        assert!(st.last_read_chunk.is_none());
    }

    #[tokio::test]
    async fn block_written_increments_write_buffer_usage() {
        let (mut mgr, _u, _p) = make_manager();
        mgr.on_block_written("T1", 0, 0, 4096).await;
        mgr.on_block_written("T1", 1, 1, 8192).await;
        let tape = mgr.tapes.get("T1").expect("tape created");
        assert_eq!(tape.write_buffer_usage, 12288);
        assert_eq!(tape.head_position, 2);
        assert_eq!(tape.pending_uploads.len(), 2);
        assert_eq!(tape.chunk_bytes.get(&0).copied(), Some(4096));
    }

    #[tokio::test]
    async fn block_written_same_chunk_accumulates_bytes() {
        let (mut mgr, _u, _p) = make_manager();
        mgr.on_block_written("T1", 5, 0, 1000).await;
        mgr.on_block_written("T1", 5, 1, 2000).await;
        let tape = mgr.tapes.get("T1").expect("tape created");
        assert_eq!(tape.chunk_bytes.get(&5).copied(), Some(3000));
        // Same chunk id only counts once in the pending set.
        assert_eq!(tape.pending_uploads.len(), 1);
    }

    #[tokio::test]
    async fn cartridge_loaded_then_unloaded_resets_state() {
        let (mut mgr, _u, _p) = make_manager();
        mgr.on_cartridge_loaded("T1", 2);
        {
            let tape = mgr.tapes.get("T1").expect("tape created");
            assert_eq!(tape.loaded_drive, Some(2));
        }
        mgr.on_block_written("T1", 0, 0, 4096).await;
        mgr.on_cartridge_unloaded("T1", 2).await;
        let tape = mgr.tapes.get("T1").expect("tape still tracked");
        assert!(tape.loaded_drive.is_none());
        assert_eq!(tape.write_buffer_usage, 0);
        assert!(tape.pending_uploads.is_empty());
        assert!(tape.chunk_bytes.is_empty());
    }

    #[tokio::test]
    async fn trigger_upload_batch_dispatches_and_decrements() {
        let (mut mgr, mut upload_rx, _p) = make_manager();
        for cid in 0..3u32 {
            mgr.on_block_written("T1", cid, cid as u64, 1000).await;
        }
        mgr.trigger_upload_batch("T1");
        let req = upload_rx.try_recv().expect("upload request dispatched");
        assert_eq!(req.tape_id, "T1");
        assert_eq!(req.chunk_ids, vec![0, 1, 2]);
        let tape = mgr.tapes.get("T1").expect("tape tracked");
        assert_eq!(tape.write_buffer_usage, 0);
        assert!(tape.pending_uploads.is_empty());
    }

    #[tokio::test]
    async fn upload_queue_depth_aggregates_across_tapes_and_tracks_lifecycle() {
        let (mut mgr, _u, _p) = make_manager();
        assert_eq!(mgr.current_upload_queue_depth(), 0);
        // Two tapes, distinct chunks each — depth is the daemon-wide sum.
        mgr.on_block_written("T1", 0, 0, 1000).await;
        mgr.on_block_written("T1", 1, 1, 1000).await;
        mgr.on_block_written("T2", 0, 0, 1000).await;
        assert_eq!(mgr.current_upload_queue_depth(), 3);
        // Dispatch drains T1's pending set; T2's stays.
        mgr.trigger_upload_batch("T1");
        assert_eq!(mgr.current_upload_queue_depth(), 1);
        // Unload clears the remaining tape's backlog.
        mgr.on_cartridge_unloaded("T2", 0).await;
        assert_eq!(mgr.current_upload_queue_depth(), 0);
    }

    #[tokio::test]
    async fn trigger_upload_batch_caps_at_max_batch_size() {
        let (mut mgr, mut upload_rx, _p) = make_manager();
        for cid in 0..12u32 {
            mgr.on_block_written("T1", cid, cid as u64, 100).await;
        }
        mgr.trigger_upload_batch("T1");
        let req = upload_rx.try_recv().expect("upload request dispatched");
        // MAX_BATCH_SIZE is 8 — the oldest 8 chunk ids go first.
        assert_eq!(req.chunk_ids.len(), 8);
        assert_eq!(req.chunk_ids, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        let tape = mgr.tapes.get("T1").expect("tape tracked");
        assert_eq!(tape.pending_uploads.len(), 4);
    }

    #[tokio::test]
    async fn trigger_upload_batch_noop_for_unknown_tape() {
        let (mut mgr, mut upload_rx, _p) = make_manager();
        mgr.trigger_upload_batch("ghost");
        assert!(upload_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn sequential_reads_trigger_prefetch() {
        let (mut mgr, _u, mut prefetch_rx) = make_manager();
        mgr.on_block_read("T1", 0, 0).await;
        // chunk 1 follows chunk 0 -> sequential, prefetch fires.
        mgr.on_block_read("T1", 1, 1).await;
        let req = prefetch_rx.try_recv().expect("prefetch dispatched");
        assert_eq!(req.tape_id, "T1");
        assert!(req.chunk_ids.contains(&2));
    }

    #[tokio::test]
    async fn non_sequential_read_does_not_prefetch() {
        let (mut mgr, _u, mut prefetch_rx) = make_manager();
        mgr.on_block_read("T1", 0, 0).await;
        // Re-reading chunk 0 is not sequential.
        mgr.on_block_read("T1", 0, 1).await;
        assert!(prefetch_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn head_position_changed_rewind_clears_prefetch() {
        let (mut mgr, _u, _p) = make_manager();
        {
            let tape = mgr.get_or_create_tape("T1");
            tape.prefetched_chunks.insert(3);
            tape.last_read_chunk = Some(2);
        }
        mgr.on_head_position_changed("T1", 10, 0, PositionChangeReason::Rewind);
        let tape = mgr.tapes.get("T1").expect("tape tracked");
        assert_eq!(tape.head_position, 0);
        assert!(tape.prefetched_chunks.is_empty());
        assert!(tape.last_read_chunk.is_none());
    }

    #[tokio::test]
    async fn head_position_changed_sequential_keeps_prefetch() {
        let (mut mgr, _u, _p) = make_manager();
        {
            let tape = mgr.get_or_create_tape("T1");
            tape.prefetched_chunks.insert(3);
            tape.last_read_chunk = Some(2);
        }
        mgr.on_head_position_changed("T1", 5, 6, PositionChangeReason::SequentialRead);
        let tape = mgr.tapes.get("T1").expect("tape tracked");
        assert_eq!(tape.head_position, 6);
        // Sequential ops must not break the prefetch window.
        assert!(tape.prefetched_chunks.contains(&3));
        assert_eq!(tape.last_read_chunk, Some(2));
    }

    #[tokio::test]
    async fn handle_event_routes_block_written() {
        let (mut mgr, _u, _p) = make_manager();
        mgr.handle_event(TapeEvent::BlockWritten {
            tape_id: "T1".to_string(),
            chunk_id: 0,
            lba: 0,
            size: 2048,
        })
        .await;
        assert_eq!(
            mgr.tapes
                .get("T1")
                .map(|t| t.write_buffer_usage)
                .unwrap_or(0),
            2048
        );
    }
}
