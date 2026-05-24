// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Test-only `CloudBackend` mock shared by the `inert` and `pipeline`
//! unit tests. `LocalBackend` is too well-behaved for failure-path
//! coverage — it never returns `CloudError`, never reports a
//! compressed size, and doesn't distinguish HEAD vs PUT call counts.
//!
//! Gated on `#[cfg(test)]` via `lib.rs` so it never ships.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use shared_cloud::{CloudBackend, CloudError, CompressionAlgo, LockState, Result};

#[derive(Debug, Default)]
pub(crate) struct MockCounters {
    pub heads: AtomicUsize,
    pub puts: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct MockBackend {
    pub counters: MockCounters,
    pub head_returns: Mutex<bool>,
    pub head_err: Mutex<Option<CloudError>>,
    pub put_err: Mutex<Option<CloudError>>,
    pub put_compressed_as: Mutex<Option<(u64, CompressionAlgo)>>,
    pub fail_put_for_keys: Mutex<HashSet<String>>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            counters: MockCounters::default(),
            head_returns: Mutex::new(false),
            head_err: Mutex::new(None),
            put_err: Mutex::new(None),
            put_compressed_as: Mutex::new(None),
            fail_put_for_keys: Mutex::new(HashSet::new()),
        }
    }
}

impl MockBackend {
    pub fn puts(&self) -> usize {
        self.counters.puts.load(Ordering::SeqCst)
    }

    pub fn heads(&self) -> usize {
        self.counters.heads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CloudBackend for MockBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        self.counters.puts.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.put_err.lock().unwrap().take() {
            return Err(e);
        }
        if self.fail_put_for_keys.lock().unwrap().contains(key) {
            return Err(CloudError::Other(format!("mock PUT fail for {}", key)));
        }
        let logical = data.len() as u64;
        if let Some((compressed_len, algo)) = *self.put_compressed_as.lock().unwrap() {
            return Ok((logical, Some(compressed_len), Some(algo)));
        }
        Ok((logical, None, None))
    }

    async fn upload_chunk_zerocopy(&self, _key: &str, _file_path: &Path) -> Result<u64> {
        Ok(0)
    }

    async fn download_chunk(&self, _key: &str) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn download_chunks_parallel(&self, _keys: &[String]) -> Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }

    async fn upload_manifest(&self, _key: &str, _json: &str) -> Result<()> {
        Ok(())
    }

    async fn download_manifest(&self, _key: &str) -> Result<String> {
        Ok(String::new())
    }

    async fn chunk_exists(&self, _key: &str) -> Result<bool> {
        self.counters.heads.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.head_err.lock().unwrap().take() {
            return Err(e);
        }
        Ok(*self.head_returns.lock().unwrap())
    }

    async fn list_objects(&self, _key_prefix: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn delete_object(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "mock"
    }

    async fn lock_state(&self) -> Result<LockState> {
        Ok(LockState::Off)
    }

    async fn set_object_legal_hold(&self, _key: &str, _held: bool) -> Result<()> {
        Ok(())
    }

    async fn get_object_legal_hold(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }

    fn clone_box(&self) -> Box<dyn CloudBackend> {
        unimplemented!("MockBackend does not support clone_box")
    }
}
