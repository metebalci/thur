// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// io_uring backend for zero-copy I/O (Linux 5.1+ only)

use crate::errors::{Result, SmcError};
#[cfg(feature = "io-uring-support")]
use io_uring::{IoUring, opcode, types};
#[cfg(feature = "io-uring-support")]
use std::os::unix::io::RawFd;

/// io_uring backend for high-performance async I/O
#[cfg(feature = "io-uring-support")]
pub struct IoUringBackend {
    ring: IoUring,
}

#[cfg(feature = "io-uring-support")]
impl IoUringBackend {
    /// Create a new io_uring backend with specified queue depth
    pub fn new(queue_depth: u32) -> Result<Self> {
        let ring = IoUring::new(queue_depth)
            .map_err(|e| SmcError::ObjectStoreError(format!("Failed to create io_uring: {}", e)))?;

        Ok(Self { ring })
    }

    /// Read data from file descriptor at specified offset
    pub async fn read_at(&mut self, fd: RawFd, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let buf_ptr = buf.as_mut_ptr();

        // Submit read operation
        let read_op = opcode::Read::new(types::Fd(fd), buf_ptr, len as u32).offset(offset);

        unsafe {
            self.ring
                .submission()
                .push(&read_op.build().user_data(0x01))
                .map_err(|e| SmcError::ObjectStoreError(format!("Failed to submit read: {}", e)))?;
        }

        self.ring
            .submit_and_wait(1)
            .map_err(|e| SmcError::ObjectStoreError(format!("Failed to submit io_uring: {}", e)))?;

        // Get completion
        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| SmcError::ObjectStoreError("No completion event".to_string()))?;

        let bytes_read = cqe.result();
        if bytes_read < 0 {
            return Err(SmcError::ObjectStoreError(format!(
                "Read failed with error code: {}",
                bytes_read
            )));
        }

        buf.truncate(bytes_read as usize);
        Ok(buf)
    }

    /// Write data to file descriptor at specified offset
    pub async fn write_at(&mut self, fd: RawFd, offset: u64, data: &[u8]) -> Result<()> {
        let write_op =
            opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32).offset(offset);

        unsafe {
            self.ring
                .submission()
                .push(&write_op.build().user_data(0x02))
                .map_err(|e| {
                    SmcError::ObjectStoreError(format!("Failed to submit write: {}", e))
                })?;
        }

        self.ring
            .submit_and_wait(1)
            .map_err(|e| SmcError::ObjectStoreError(format!("Failed to submit io_uring: {}", e)))?;

        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| SmcError::ObjectStoreError("No completion event".to_string()))?;

        if cqe.result() < 0 {
            return Err(SmcError::ObjectStoreError(format!(
                "Write failed with error code: {}",
                cqe.result()
            )));
        }

        Ok(())
    }
}

/// Fallback implementation when io-uring is not available
#[cfg(not(feature = "io-uring-support"))]
pub struct IoUringBackend;

#[cfg(not(feature = "io-uring-support"))]
impl IoUringBackend {
    pub fn new(_queue_depth: u32) -> Result<Self> {
        Err(SmcError::ObjectStoreError(
            "io_uring support not compiled in".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "io-uring-support")]
    #[tokio::test]
    async fn test_io_uring_read_write() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.dat");

        // Create test file with data
        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"Hello, io_uring!").unwrap();
        drop(file);

        // Test read
        let mut backend = IoUringBackend::new(8).unwrap();
        let file = OpenOptions::new().read(true).open(&file_path).unwrap();
        let data = backend.read_at(file.as_raw_fd(), 0, 16).await.unwrap();
        assert_eq!(&data, b"Hello, io_uring!");
    }

    #[cfg(feature = "io-uring-support")]
    #[tokio::test]
    async fn test_io_uring_write() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.dat");

        // Test write
        let mut backend = IoUringBackend::new(8).unwrap();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&file_path)
            .unwrap();

        backend
            .write_at(file.as_raw_fd(), 0, b"Test write")
            .await
            .unwrap();
        drop(file);

        // Verify written data
        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(contents, "Test write");
    }
}
