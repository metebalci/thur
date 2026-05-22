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
    /// Create a new MemoryBufferManager
    pub fn new(
        event_rx: broadcast::Receiver<TapeEvent>,
        write_buffer_gb: u64,
        read_buffer_gb: u64,
        upload_tx: mpsc::Sender<UploadRequest>,
        prefetch_tx: mpsc::Sender<PrefetchRequest>,
    ) -> Self {
        let write_buffer_limit = write_buffer_gb * 1024 * 1024 * 1024;
        let read_buffer_limit = read_buffer_gb * 1024 * 1024 * 1024;
        info!(
            "MemoryBufferManager created (write_buffer={} GB, read_buffer={} GB per tape)",
            write_buffer_gb, read_buffer_gb
        );
        Self {
            event_rx,
            tapes: HashMap::new(),
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
                        self.trigger_upload_batch(&tape_id).await;
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
            // Loop until empty: trigger_upload_batch caps at MAX_BATCH_SIZE
            // per call, so a tape with more than that many queued chunks
            // would otherwise leave bytes stranded after this final flush.
            while self
                .tapes
                .get(tape_id)
                .is_some_and(|t| !t.pending_uploads.is_empty())
            {
                self.trigger_upload_batch(tape_id).await;
            }
        }
        // Reset volatile per-load state. write_buffer_usage and the
        // chunk_bytes side map are bookkeeping, not durability — the
        // upload pipeline owns chunk durability via `chunks.idx`.
        // Leaving them populated would carry stale accounting into the
        // next load.
        if let Some(tape) = self.tapes.get_mut(tape_id) {
            tape.write_buffer_usage = 0;
            tape.pending_uploads.clear();
            tape.chunk_bytes.clear();
            tape.read_buffer_usage = 0;
            tape.prefetched_chunks.clear();
            tape.last_read_chunk = None;
        }
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

        if let Some((usage, limit, pending)) = warn_payload {
            warn!(
                "Write buffer full for {}: {} / {} bytes ({} pending uploads)",
                tape_id, usage, limit, pending
            );
            self.trigger_upload_batch(tape_id).await;
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
            self.trigger_prefetch(tape_id, chunk_id + 1).await;
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
    /// Selects up to `MAX_BATCH_SIZE` of the oldest pending chunks,
    /// dispatches them to the upload worker, and decrements the
    /// per-tape write-buffer accounting by their byte total. The
    /// dispatched chunk IDs are removed from `pending_uploads` so
    /// the next BlockWritten doesn't re-fire on the same set.
    /// `send().await` propagates upload-worker backpressure all the
    /// way back to the broadcast bus, instead of `try_send` silently
    /// dropping the request when the mpsc is full.
    async fn trigger_upload_batch(&mut self, tape_id: &str) {
        const MAX_BATCH_SIZE: usize = 8;

        let (chunk_ids, dispatched_bytes) = {
            let Some(tape) = self.tapes.get_mut(tape_id) else {
                return;
            };
            if tape.pending_uploads.is_empty() {
                return;
            }
            let mut chunk_ids: Vec<u32> = tape.pending_uploads.iter().copied().collect();
            chunk_ids.sort_unstable();
            if chunk_ids.len() > MAX_BATCH_SIZE {
                chunk_ids.truncate(MAX_BATCH_SIZE);
            }
            // Pull dispatched chunks out of the pending state. The
            // upload worker owns chunk durability after this point
            // (see `chunks.idx`'s `uploaded` flag + HEAD-skip on
            // retry); a dispatched-then-failed chunk shows up again
            // on the next event-driven trigger via
            // `force_pending_uploads`-style scans. For now we accept
            // the fire-and-forget semantics already in place.
            let mut dispatched_bytes: u64 = 0;
            for cid in &chunk_ids {
                tape.pending_uploads.remove(cid);
                if let Some(b) = tape.chunk_bytes.remove(cid) {
                    dispatched_bytes = dispatched_bytes.saturating_add(b);
                }
            }
            tape.write_buffer_usage = tape.write_buffer_usage.saturating_sub(dispatched_bytes);
            (chunk_ids, dispatched_bytes)
        };

        info!(
            "Triggering upload batch for {}: {} chunks ({} bytes), write_buffer_usage now {}",
            tape_id,
            chunk_ids.len(),
            dispatched_bytes,
            self.tapes
                .get(tape_id)
                .map(|t| t.write_buffer_usage)
                .unwrap_or(0)
        );

        let request = UploadRequest {
            tape_id: tape_id.to_string(),
            chunk_ids,
        };

        // Bounded send — applies backpressure when the upload worker
        // is saturated. A full mpsc here means the worker is the
        // bottleneck; blocking the manager (and ultimately lagging
        // the broadcast bus) is the correct backpressure path.
        if let Err(e) = self.upload_tx.send(request).await {
            warn!(
                "Failed to send upload request for {} (channel closed): {}",
                tape_id, e
            );
        }
    }

    /// Trigger prefetch for a tape (Phase 5: Event-Driven Prefetch)
    async fn trigger_prefetch(&mut self, tape_id: &str, start_chunk_id: u32) {
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

        // Bounded send — same rationale as trigger_upload_batch.
        if let Err(e) = self.prefetch_tx.send(request).await {
            warn!(
                "Failed to send prefetch request for {} (channel closed): {}",
                tape_id, e
            );
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
