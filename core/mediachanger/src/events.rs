// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Tape Events
//
// Event-driven architecture for tape operations. Events are emitted by the iSCSI
// target when tape operations occur, and consumed by the MemoryBufferManager to trigger
// S3 uploads, prefetching, and eviction.

use serde::{Deserialize, Serialize};

/// Events emitted during tape operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TapeEvent {
    /// A cartridge was loaded into a drive
    CartridgeLoaded { tape_id: String, drive_num: u8 },

    /// A cartridge was unloaded from a drive to a slot
    CartridgeUnloaded { tape_id: String, drive_num: u8 },

    /// A block was written to tape
    /// This triggers buffer tracking and potentially S3 upload
    BlockWritten {
        tape_id: String,
        chunk_id: u32,
        lba: u64,
        size: u64,
    },

    /// A block was read from tape
    /// This triggers prefetch for sequential reads
    BlockRead {
        tape_id: String,
        chunk_id: u32,
        lba: u64,
    },

    /// The tape head position changed (LOCATE, REWIND, SPACE)
    /// This cancels active prefetch tasks
    HeadPositionChanged {
        tape_id: String,
        old_lba: u64,
        new_lba: u64,
        reason: PositionChangeReason,
    },
}

/// Reason for head position change
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PositionChangeReason {
    /// REWIND command
    Rewind,
    /// LOCATE command (random access)
    Locate,
    /// SPACE command (skip records/filemarks)
    Space,
    /// Sequential read advancing position
    SequentialRead,
    /// Sequential write advancing position
    SequentialWrite,
}

impl TapeEvent {
    /// Get the tape ID from any event
    pub fn tape_id(&self) -> &str {
        match self {
            TapeEvent::CartridgeLoaded { tape_id, .. } => tape_id,
            TapeEvent::CartridgeUnloaded { tape_id, .. } => tape_id,
            TapeEvent::BlockWritten { tape_id, .. } => tape_id,
            TapeEvent::BlockRead { tape_id, .. } => tape_id,
            TapeEvent::HeadPositionChanged { tape_id, .. } => tape_id,
        }
    }

    /// Check if this is a write event
    pub fn is_write(&self) -> bool {
        matches!(self, TapeEvent::BlockWritten { .. })
    }

    /// Check if this is a read event
    pub fn is_read(&self) -> bool {
        matches!(self, TapeEvent::BlockRead { .. })
    }

    /// Check if this event should cancel prefetch
    pub fn cancels_prefetch(&self) -> bool {
        matches!(
            self,
            TapeEvent::HeadPositionChanged {
                reason: PositionChangeReason::Rewind | PositionChangeReason::Locate,
                ..
            }
        )
    }
}
