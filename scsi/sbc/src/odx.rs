// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Hyper-V ODX (Offloaded Data Transfer) token state.
//!
//! ODX is a token-based variant of the SCSI EXTENDED COPY family:
//! `POPULATE TOKEN` (opcode `0x83` sa `0x10`) freezes a source LBA
//! range and mints a 512-byte ROD token; `WRITE USING TOKEN`
//! (`0x83` sa `0x11`) later applies that token to a destination
//! range (commonly on a different LUN); `RECEIVE ROD TOKEN
//! INFORMATION` (`0x84` sa `0x07`) is the polling channel that
//! returns the minted token by LIST IDENTIFIER and reports completion
//! status for both commands.
//!
//! The two commands run in distinct SCSI dispatches and the token
//! has a configurable inactivity timeout, so the dispatcher owns
//! process-global state that bridges them:
//!
//! - `tokens`: the live ROD tokens. Each entry carries a snapshot of
//!   the source's per-page chunk hashes (taken at POPULATE TOKEN
//!   time) and a `Vec<PoolPinGuard>` so the eviction worker + GC
//!   skip those chunks until the token expires.
//! - `jobs`: LIST IDENTIFIER → outcome. The CDB names a list ID;
//!   the host issues RRTI against the same list ID to fetch the
//!   token or check WRITE USING TOKEN completion. Sync-inline
//!   command path means the entry is always `Done` by the first
//!   poll.
//!
//! In-memory only. Daemon restart drops every token, matching ODX
//! semantics (tokens are TTL-bounded; SPC-4 lets the target invalidate
//! them whenever the source backing store mutates beyond the
//! implementation's snapshot capability).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngCore;
use shared_pool::PoolPinGuard;

/// Default ROD token inactivity timeout, published in VPD 0x8F
/// descriptor `0x0000`. Hosts that don't override via the POPULATE
/// TOKEN CDB get this.
pub const DEFAULT_ROD_INACTIVITY_SECS: u32 = 300;

/// Hard ceiling on ROD token inactivity timeout. Hosts requesting
/// a larger value get clamped down with no error.
pub const MAX_ROD_INACTIVITY_SECS: u32 = 600;

/// 512-byte opaque ROD token handed to initiators. Per SPC-4 the
/// shape is implementation-defined; we mint a 16-byte version /
/// magic prefix followed by 496 bytes of `OsRng` so two tokens can't
/// collide.
pub type RodToken = [u8; ROD_TOKEN_LEN];

/// Size of a ROD token on the wire — fixed by SPC-4 §6.18.
pub const ROD_TOKEN_LEN: usize = 512;

/// Per-token snapshot of source state taken at POPULATE TOKEN time.
/// Pin guards keep referenced chunks unevictable; the rest is the
/// metadata `WRITE USING TOKEN` consults to drive the cross-volume
/// clone.
pub struct TokenState {
    pub source_volume_uuid: [u8; 16],
    pub source_lun: u64,
    pub source_backend: String,
    pub source_namespace: Option<String>,
    pub source_page_size: u32,
    pub sector_size: u32,
    /// Source page IDs covered by the snapshot, in order.
    pub source_pages: Vec<u32>,
    /// `hashes[i]` is the chunk hash for `source_pages[i]` as raw
    /// BLAKE3 bytes (the same shape `PageIndex` stores). `None`
    /// records a sparse hole (SBC-3 reads-as-zero).
    pub hashes: Vec<Option<[u8; 32]>>,
    /// Sum of block counts across every range descriptor. Reported
    /// back via RRTI's TRANSFER COUNT field on the corresponding
    /// WRITE USING TOKEN job.
    pub total_blocks: u64,
    /// Wall-clock expiry. The sweeper task evicts tokens past this
    /// instant; live lookups also check and refuse on miss.
    pub deadline: Instant,
    /// Pin handles keeping every referenced chunk pinned in the
    /// local pool against eviction + GC. Drop releases — the field
    /// is "never read" by design; its lifetime is its purpose.
    #[allow(dead_code)]
    pub pins: Vec<PoolPinGuard>,
}

/// Clonable view of [`TokenState`] for consumers that don't own the
/// pins. `WRITE USING TOKEN` clones one of these out under the
/// table lock then runs the cross-volume page-clone loop without
/// holding the mutex; the pins stay in place inside the manager.
#[derive(Clone)]
#[allow(dead_code)]
pub struct TokenSnapshot {
    /// Source volume's UUID at POPULATE TOKEN time. Surfaced for
    /// future per-source attribution; not consulted by the v1
    /// WRITE USING TOKEN path which trusts the snapshot hashes.
    pub source_volume_uuid: [u8; 16],
    pub source_lun: u64,
    pub source_backend: String,
    pub source_namespace: Option<String>,
    pub source_page_size: u32,
    /// Source sector size — kept for forward compatibility, though
    /// the current cross-volume policy already constrains pool
    /// compatibility through namespace + backend match.
    pub sector_size: u32,
    pub source_pages: Vec<u32>,
    pub hashes: Vec<Option<[u8; 32]>>,
}

/// Outcome of a POPULATE TOKEN or WRITE USING TOKEN job, keyed by
/// the LIST IDENTIFIER from the originating CDB. RRTI looks these
/// up to answer the host's polls.
#[derive(Clone)]
pub struct JobResult {
    pub status: JobStatus,
    /// Populated only for POPULATE TOKEN jobs — RRTI emits this in
    /// the ROD Token Descriptor of its response. None for WRITE
    /// USING TOKEN jobs (the spec's ROD token descriptor count
    /// is 0 in that case).
    pub token: Option<RodToken>,
    /// Number of source blocks transferred by this job. RRTI emits
    /// it in the TRANSFER COUNT field.
    pub transfer_blocks: u64,
    /// Wall-clock expiry — jobs survive the originating ROD token
    /// long enough for the host's first RRTI poll but get swept
    /// after the token's TTL elapses anyway.
    pub deadline: Instant,
}

/// Job-completion summary recorded for the LIST IDENTIFIER. SPC-4
/// allows "in progress" intermediate states; our sync-inline path
/// means the entry is `Done` on the first poll or `Failed` with an
/// op-specific completion-status byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Done,
    Failed { completion_status: u8 },
}

/// Per-dispatcher ODX state. `tokens` keyed by ROD token bytes;
/// `jobs` keyed by LIST IDENTIFIER from POPULATE TOKEN / WRITE
/// USING TOKEN CDBs.
pub struct TokenManager {
    tokens: Mutex<HashMap<RodToken, TokenState>>,
    jobs: Mutex<HashMap<u32, JobResult>>,
    default_ttl: Duration,
    max_ttl: Duration,
}

impl TokenManager {
    pub fn new() -> Self {
        Self::with_ttl(
            Duration::from_secs(DEFAULT_ROD_INACTIVITY_SECS as u64),
            Duration::from_secs(MAX_ROD_INACTIVITY_SECS as u64),
        )
    }

    pub fn with_ttl(default_ttl: Duration, max_ttl: Duration) -> Self {
        Self {
            tokens: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            default_ttl,
            max_ttl,
        }
    }

    /// Effective TTL for a POPULATE TOKEN whose INACTIVITY TIMEOUT
    /// field is `requested`. `0` → default; values larger than
    /// [`Self::max_ttl_secs`] are clamped down with no error per
    /// SPC-4 §6.18.
    pub fn resolve_ttl(&self, requested_secs: u32) -> Duration {
        if requested_secs == 0 {
            return self.default_ttl;
        }
        let r = Duration::from_secs(requested_secs as u64);
        if r > self.max_ttl { self.max_ttl } else { r }
    }

    /// Mint a fresh 512-byte ROD token, insert `state` under it, and
    /// record `(list_id → Done + token + transfer_blocks)` in the
    /// jobs table. Both entries inherit the token's deadline so the
    /// sweeper expires them together.
    pub fn mint_token(&self, list_id: u32, mut state: TokenState) -> RodToken {
        let mut token: RodToken = [0u8; ROD_TOKEN_LEN];
        rand::rngs::OsRng.fill_bytes(&mut token);
        // Loop-on-collision is theoretical (512 bits of OsRng) but
        // cheap insurance.
        {
            let tokens = match self.tokens.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            while tokens.contains_key(&token) {
                rand::rngs::OsRng.fill_bytes(&mut token);
            }
            drop(tokens);
        }
        let total = state.total_blocks;
        let deadline = state.deadline;
        // Mark every snapshot entry as `Some` for sparse holes too
        // (the None markers ride through to WRITE USING TOKEN's
        // sparse-hole path). Sanity asserts.
        debug_assert_eq!(state.hashes.len(), state.source_pages.len());
        state.deadline = deadline;
        {
            let mut tokens = match self.tokens.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            tokens.insert(token, state);
        }
        let job = JobResult {
            status: JobStatus::Done,
            token: Some(token),
            transfer_blocks: total,
            deadline,
        };
        let mut jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        jobs.insert(list_id, job);
        token
    }

    /// Record a WRITE USING TOKEN completion outcome under `list_id`.
    /// `transfer_blocks` is the number of source blocks the command
    /// processed; RRTI surfaces it in TRANSFER COUNT.
    pub fn record_write_outcome(
        &self,
        list_id: u32,
        status: JobStatus,
        transfer_blocks: u64,
        ttl: Duration,
    ) {
        let job = JobResult {
            status,
            token: None,
            transfer_blocks,
            deadline: Instant::now() + ttl,
        };
        let mut jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        jobs.insert(list_id, job);
    }

    /// Look up the snapshot a `WRITE USING TOKEN` should clone from.
    /// Returns `None` for unknown tokens *and* for tokens past their
    /// inactivity deadline (the caller distinguishes the two via
    /// [`Self::is_expired`]).
    pub fn snapshot_for(&self, token: &RodToken) -> Option<TokenSnapshot> {
        let tokens = match self.tokens.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let st = tokens.get(token)?;
        if Instant::now() >= st.deadline {
            return None;
        }
        Some(TokenSnapshot {
            source_volume_uuid: st.source_volume_uuid,
            source_lun: st.source_lun,
            source_backend: st.source_backend.clone(),
            source_namespace: st.source_namespace.clone(),
            source_page_size: st.source_page_size,
            sector_size: st.sector_size,
            source_pages: st.source_pages.clone(),
            hashes: st.hashes.clone(),
        })
    }

    /// Distinguishes "no such token" from "token existed but expired"
    /// so WRITE USING TOKEN can return the right sense
    /// (`INVALID TOKEN TYPE` vs `TOKEN EXPIRED`).
    pub fn is_expired(&self, token: &RodToken) -> bool {
        let tokens = match self.tokens.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match tokens.get(token) {
            Some(st) => Instant::now() >= st.deadline,
            None => false,
        }
    }

    /// Fetch the job outcome for a LIST IDENTIFIER. RRTI calls this
    /// for each poll. None on miss (no operation in progress, per
    /// SPC-4 §6.20).
    pub fn job_result(&self, list_id: u32) -> Option<JobResult> {
        let jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        jobs.get(&list_id).cloned()
    }

    /// Drop every entry whose deadline has passed. Called by the
    /// sweeper task on its interval; safe to call manually from
    /// tests.
    pub fn sweep_expired(&self) {
        let now = Instant::now();
        {
            let mut tokens = match self.tokens.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            tokens.retain(|_, st| st.deadline > now);
        }
        let mut jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        jobs.retain(|_, j| j.deadline > now);
    }
}

impl Default for TokenManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_state(deadline: Instant, total_blocks: u64) -> TokenState {
        TokenState {
            source_volume_uuid: [0u8; 16],
            source_lun: 0,
            source_backend: "primary".to_string(),
            source_namespace: None,
            source_page_size: 65_536,
            sector_size: 4096,
            source_pages: vec![0, 1],
            hashes: vec![Some([0xAAu8; 32]), Some([0xBBu8; 32])],
            total_blocks,
            deadline,
            pins: Vec::new(),
        }
    }

    #[test]
    fn mint_then_snapshot_round_trips() {
        let mgr = TokenManager::new();
        let token = mgr.mint_token(
            0x42,
            dummy_state(Instant::now() + Duration::from_secs(60), 32),
        );
        let snap = mgr.snapshot_for(&token).expect("token present");
        assert_eq!(snap.source_pages, vec![0, 1]);
        let job = mgr.job_result(0x42).expect("job present");
        assert_eq!(job.token, Some(token));
        assert_eq!(job.transfer_blocks, 32);
        assert_eq!(job.status, JobStatus::Done);
    }

    #[test]
    fn snapshot_misses_after_expiry() {
        let mgr = TokenManager::new();
        let token = mgr.mint_token(7, dummy_state(Instant::now() - Duration::from_millis(1), 0));
        assert!(mgr.snapshot_for(&token).is_none());
        assert!(mgr.is_expired(&token));
    }

    #[test]
    fn snapshot_distinguishes_missing_from_expired() {
        let mgr = TokenManager::new();
        let missing = [0u8; ROD_TOKEN_LEN];
        assert!(!mgr.is_expired(&missing));
        assert!(mgr.snapshot_for(&missing).is_none());
    }

    #[test]
    fn sweep_drops_expired_tokens_and_jobs() {
        let mgr = TokenManager::new();
        let alive = mgr.mint_token(1, dummy_state(Instant::now() + Duration::from_secs(60), 0));
        let dead = mgr.mint_token(2, dummy_state(Instant::now() - Duration::from_millis(1), 0));
        mgr.sweep_expired();
        assert!(mgr.snapshot_for(&alive).is_some());
        assert!(mgr.snapshot_for(&dead).is_none());
        assert!(mgr.job_result(1).is_some());
        assert!(mgr.job_result(2).is_none());
    }

    #[test]
    fn resolve_ttl_zero_uses_default_nonzero_clamps_to_max() {
        let mgr = TokenManager::with_ttl(Duration::from_secs(30), Duration::from_secs(120));
        assert_eq!(mgr.resolve_ttl(0), Duration::from_secs(30));
        assert_eq!(mgr.resolve_ttl(60), Duration::from_secs(60));
        assert_eq!(mgr.resolve_ttl(999), Duration::from_secs(120));
    }

    #[test]
    fn record_write_outcome_round_trips() {
        let mgr = TokenManager::new();
        mgr.record_write_outcome(99, JobStatus::Done, 128, Duration::from_secs(60));
        let job = mgr.job_result(99).expect("job present");
        assert_eq!(job.transfer_blocks, 128);
        assert!(job.token.is_none());
        assert_eq!(job.status, JobStatus::Done);
    }
}
