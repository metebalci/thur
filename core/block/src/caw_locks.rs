// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-LUN async mutex registry for COMPARE AND WRITE / fused
//! Compare+Write serialization.
//!
//! `compare_and_write_bytes` is *not* internally atomic — it runs the
//! compare phase and the write phase as two separate awaits. Two
//! concurrent test-and-sets against the same LUN must be serialized so
//! they don't both pass the compare against the same stored value and
//! then both commit (split-brain / lock-stealing in clustered
//! filesystems and VMFS heartbeats).
//!
//! This lives in `core-block` rather than a command-set crate so the
//! SBC (iSCSI) and NVM (NVMe/TCP) dispatchers can share *one* instance:
//! when a volume is exported over both transports (issue #66) a fused
//! CAW arriving over NVMe must serialize against a COMPARE AND WRITE
//! arriving over iSCSI on the same volume (issue #128). LUN is the
//! shared key — NVMe resolves `nsid → lun` to the same value the SCSI
//! side uses.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;

/// Per-LUN async mutex registry. A caller holds the per-LUN lock across
/// its whole read+compare+write window. The inner sync mutex is held
/// only for one `BTreeMap` lookup; the `Arc<AsyncMutex>` is what callers
/// actually `.lock().await` on.
#[derive(Default)]
pub struct CawLocks {
    inner: StdMutex<BTreeMap<u64, Arc<AsyncMutex<()>>>>,
}

impl CawLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up (and lazily create) the per-LUN lock. Cheap — one
    /// `BTreeMap` lookup per CAW; the lock entries linger for the
    /// daemon's lifetime, which is fine at the LUN counts in play.
    pub fn lock_for(&self, lun: u64) -> Arc<AsyncMutex<()>> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(lun).or_default().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_for_returns_same_arc_per_lun() {
        let locks = CawLocks::new();
        let a = locks.lock_for(3);
        let b = locks.lock_for(3);
        let c = locks.lock_for(4);
        assert!(Arc::ptr_eq(&a, &b), "same LUN yields the same lock");
        assert!(!Arc::ptr_eq(&a, &c), "different LUN yields a different lock");
    }

    #[tokio::test]
    async fn second_caw_on_same_lun_waits_for_the_first() {
        let locks = Arc::new(CawLocks::new());
        let g1 = locks.lock_for(0).lock_owned().await;
        // A second acquire on the same LUN must not be immediately ready.
        let l2 = locks.lock_for(0);
        assert!(l2.try_lock().is_err(), "held LUN lock blocks the second CAW");
        drop(g1);
        assert!(l2.try_lock().is_ok(), "released lock lets the next CAW in");
    }
}
