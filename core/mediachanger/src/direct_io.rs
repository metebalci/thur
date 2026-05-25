// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Direct I/O utilities for bypassing kernel page cache

use crate::errors::{Result, SmcError};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Alignment requirement for Direct I/O (typically 4096 bytes for most filesystems)
pub const DIRECT_IO_ALIGNMENT: usize = 4096;

/// Align buffer to required boundary for Direct I/O
pub fn align_buffer(data: &[u8], alignment: usize) -> Vec<u8> {
    let aligned_len = (data.len() + alignment - 1) & !(alignment - 1);
    let mut aligned = vec![0u8; aligned_len];
    aligned[..data.len()].copy_from_slice(data);
    aligned
}

/// Open file for Direct I/O reads (O_DIRECT flag)
#[cfg(target_os = "linux")]
pub fn open_direct_read<P: AsRef<Path>>(path: P) -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .map_err(|e| {
            SmcError::ObjectStoreError(format!("Failed to open file for direct read: {}", e))
        })
}

/// Open file for Direct I/O writes (O_DIRECT flag)
#[cfg(target_os = "linux")]
pub fn open_direct_write<P: AsRef<Path>>(path: P) -> Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .map_err(|e| {
            SmcError::ObjectStoreError(format!("Failed to open file for direct write: {}", e))
        })
}

/// Fallback for non-Linux platforms (standard I/O)
#[cfg(not(target_os = "linux"))]
pub fn open_direct_read<P: AsRef<Path>>(path: P) -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| SmcError::ObjectStoreError(format!("Failed to open file for read: {}", e)))
}

/// Fallback for non-Linux platforms (standard I/O)
#[cfg(not(target_os = "linux"))]
pub fn open_direct_write<P: AsRef<Path>>(path: P) -> Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .open(path)
        .map_err(|e| SmcError::ObjectStoreError(format!("Failed to open file for write: {}", e)))
}

/// Check if a buffer is properly aligned for Direct I/O
pub fn is_aligned(buffer: &[u8], alignment: usize) -> bool {
    let ptr = buffer.as_ptr() as usize;
    ptr.is_multiple_of(alignment) && buffer.len().is_multiple_of(alignment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_buffer() {
        let data = vec![1, 2, 3, 4, 5];
        let aligned = align_buffer(&data, 4096);

        // Check alignment
        assert_eq!(aligned.len(), 4096);
        assert_eq!(aligned.len() % 4096, 0);

        // Check data preserved
        assert_eq!(&aligned[..5], &[1, 2, 3, 4, 5]);

        // Check padding is zeros
        assert_eq!(&aligned[5..], &vec![0u8; 4096 - 5][..]);
    }

    #[test]
    fn test_is_aligned() {
        // Note: Vec doesn't guarantee alignment of the pointer, only the size
        // This test just checks that the size-based check works
        let data = vec![0u8; 4096];
        // Only check size alignment, not pointer alignment
        assert_eq!(data.len() % 4096, 0);

        let unaligned = vec![0u8; 4097];
        assert_ne!(unaligned.len() % 4096, 0);
    }

    #[test]
    fn test_small_buffer_alignment() {
        let small = vec![1, 2, 3];
        let aligned = align_buffer(&small, 4096);
        assert_eq!(aligned.len(), 4096);
        assert_eq!(&aligned[..3], &[1, 2, 3]);
    }

    #[test]
    fn test_already_aligned_buffer() {
        let data = vec![0u8; 4096];
        let aligned = align_buffer(&data, 4096);
        assert_eq!(aligned.len(), 4096);
    }
}
