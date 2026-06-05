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
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_block::PageCache;
use core_block::page_index::ChunkHash;
use core_block::uploader::UploaderError;
use rand::RngCore;
use shared_pool::PoolPinGuard;

/// Source-side decryptor a live ROD token retains so `WRITE USING
/// TOKEN` can recrypt its pinned snapshot chunks under the *source*
/// crypto identity when the destination cannot reconstruct their
/// (key, IV) — i.e. the two volumes have distinct crypto identities,
/// the chunk lands at a different page offset, or one side is
/// unencrypted (see [`core_block::rebind_is_sound`]). Implemented by
/// [`core_block::PageCache`] (delegating to the source `VolumeWriter`);
/// the token holds it as `Arc<dyn SourceDecryptor>` so the chunk's
/// decrypt key stays reachable for the token's lifetime even if the
/// source volume is closed or deleted before `WRITE USING TOKEN`
/// arrives. Holding the source handle (rather than re-resolving the
/// source LUN at write time) also closes a LUN-reuse hole: a recycled
/// `source_lun` could otherwise point the recrypt at an unrelated
/// volume's data.
#[async_trait]
pub trait SourceDecryptor: Send + Sync {
    /// Decrypt the pinned chunk identified by (`hash`, `iv_salt`) under
    /// the source identity, deriving the IV from `source_page_id`.
    /// Returns plaintext (or the chunk bytes verbatim for an
    /// unencrypted source).
    async fn decrypt_chunk(
        &self,
        source_page_id: u32,
        hash: ChunkHash,
        iv_salt: u64,
    ) -> Result<Vec<u8>, UploaderError>;
}

#[async_trait]
impl SourceDecryptor for PageCache {
    async fn decrypt_chunk(
        &self,
        source_page_id: u32,
        hash: ChunkHash,
        iv_salt: u64,
    ) -> Result<Vec<u8>, UploaderError> {
        self.writer()
            .decrypt_page_at(source_page_id, &hash, iv_salt)
            .await
    }
}

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
    /// `iv_salts[i]` is the per-page AES-GCM IV salt for
    /// `source_pages[i]` (issue #87), captured alongside the hash so
    /// `WRITE USING TOKEN` rebinds the destination record with the
    /// nonce the ciphertext was sealed under. `0` for sparse holes and
    /// unencrypted sources.
    pub iv_salts: Vec<u64>,
    /// Whether the source volume encrypts at rest, captured at
    /// POPULATE TOKEN so `WRITE USING TOKEN` can choose rebind-vs-recrypt
    /// without re-reading the (possibly mutated/closed) source manifest.
    pub source_encrypted: bool,
    /// Source volume's crypto identity (`dek_uuid()`), captured at
    /// POPULATE TOKEN. The destination may hash-rebind only if it shares
    /// this (and the page lands at the same offset); otherwise the
    /// snapshot chunks are recrypted via [`Self::source_decryptor`]
    /// (issue #88).
    pub source_dek_uuid: [u8; 16],
    /// Decrypt handle for the source's pinned snapshot chunks, retained
    /// for the token's lifetime so the recrypt path can read them back
    /// under the source identity even if the source volume is closed or
    /// deleted before `WRITE USING TOKEN` arrives.
    pub source_decryptor: Arc<dyn SourceDecryptor>,
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
    /// future per-source attribution; the WRITE USING TOKEN path keys
    /// its rebind-vs-recrypt decision on `source_dek_uuid` /
    /// `source_decryptor` instead, so this field itself is not consulted.
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
    /// Per-page IV salts, parallel to `hashes` (issue #87).
    pub iv_salts: Vec<u64>,
    /// Whether the source volume encrypts at rest (issue #88).
    pub source_encrypted: bool,
    /// Source crypto identity (`dek_uuid()`) — rebind is sound only if
    /// the destination shares it and the page offset matches.
    pub source_dek_uuid: [u8; 16],
    /// Decrypt handle for the source's pinned snapshot chunks, used by
    /// the WRITE USING TOKEN recrypt path.
    pub source_decryptor: Arc<dyn SourceDecryptor>,
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
            iv_salts: st.iv_salts.clone(),
            source_encrypted: st.source_encrypted,
            source_dek_uuid: st.source_dek_uuid,
            source_decryptor: st.source_decryptor.clone(),
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

    /// Cancel the ROD token minted by the POPULATE TOKEN job under
    /// `list_id` (CANCEL ROD TOKEN, EXTENDED COPY sa 0x12). Drops the
    /// token's entry — releasing its `PoolPinGuard`s so eviction + GC
    /// can reclaim the referenced chunks — and forgets the job so a
    /// later RRTI reports "no operation in progress". Returns whether
    /// a token was actually freed; cancelling an unknown / already-
    /// expired / non-POPULATE-TOKEN list ID is a no-op (SPC-4 §6.5
    /// makes cancelling a token the copy manager no longer holds a
    /// GOOD no-op, not an error).
    pub fn cancel(&self, list_id: u32) -> bool {
        let token = {
            let mut jobs = match self.jobs.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // Only POPULATE TOKEN jobs own a ROD token; a WRITE USING
            // TOKEN job under this ID carries `token: None` and is
            // left untouched.
            match jobs.get(&list_id).and_then(|j| j.token) {
                Some(t) => {
                    jobs.remove(&list_id);
                    Some(t)
                }
                None => None,
            }
        };
        match token {
            Some(t) => {
                let mut tokens = match self.tokens.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                // Drop releases the pins held in the TokenState.
                tokens.remove(&t).is_some()
            }
            None => false,
        }
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

    /// Trivial decryptor for the token-manager unit tests, which
    /// exercise TTL / mint / sweep logic and never drive the recrypt
    /// path. The cross-identity recrypt itself is covered end-to-end in
    /// the `data_path` ODX tests against real encrypted volumes.
    struct StubDecryptor;

    #[async_trait]
    impl SourceDecryptor for StubDecryptor {
        async fn decrypt_chunk(
            &self,
            _source_page_id: u32,
            _hash: ChunkHash,
            _iv_salt: u64,
        ) -> Result<Vec<u8>, UploaderError> {
            Ok(Vec::new())
        }
    }

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
            iv_salts: vec![0, 0],
            source_encrypted: false,
            source_dek_uuid: [0u8; 16],
            source_decryptor: Arc::new(StubDecryptor),
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

    #[test]
    fn cancel_removes_token_and_job() {
        let mgr = TokenManager::new();
        let token = mgr.mint_token(
            0x55,
            dummy_state(Instant::now() + Duration::from_secs(60), 16),
        );
        assert!(mgr.snapshot_for(&token).is_some());
        assert!(mgr.job_result(0x55).is_some());
        assert!(mgr.cancel(0x55), "cancel freed the token");
        assert!(mgr.snapshot_for(&token).is_none(), "token entry dropped");
        assert!(mgr.job_result(0x55).is_none(), "job forgotten");
    }

    #[test]
    fn cancel_unknown_list_id_is_noop() {
        let mgr = TokenManager::new();
        assert!(!mgr.cancel(0x1234));
    }

    #[test]
    fn cancel_write_using_token_job_leaves_it_untouched() {
        // A WRITE USING TOKEN job owns no ROD token, so CANCEL ROD
        // TOKEN against its list ID must be a no-op and not evict it.
        let mgr = TokenManager::new();
        mgr.record_write_outcome(0x77, JobStatus::Done, 128, Duration::from_secs(60));
        assert!(!mgr.cancel(0x77));
        assert!(mgr.job_result(0x77).is_some());
    }

    /// Build a token state with deliberately non-default crypto fields
    /// — `dummy_state` zeroes them, so the existing round-trip test
    /// would still pass if `snapshot_for` dropped one. These are the
    /// recrypt-decision inputs (issues #87/#88); a dropped field forces
    /// the wrong rebind-vs-recrypt choice and corrupts the destination.
    fn encrypted_state(decryptor: Arc<dyn SourceDecryptor>, encrypted: bool) -> TokenState {
        TokenState {
            source_volume_uuid: [0u8; 16],
            source_lun: 0,
            source_backend: "primary".to_string(),
            source_namespace: None,
            source_page_size: 65_536,
            sector_size: 4096,
            source_pages: vec![0, 1],
            hashes: vec![Some([0xAAu8; 32]), Some([0xBBu8; 32])],
            iv_salts: vec![0xDEAD_BEEF, 0xBEEF_DEAD],
            source_encrypted: encrypted,
            source_dek_uuid: if encrypted {
                [0xAAu8; 16]
            } else {
                [0xBBu8; 16]
            },
            source_decryptor: decryptor,
            total_blocks: 32,
            deadline: Instant::now() + Duration::from_secs(60),
            pins: Vec::new(),
        }
    }

    #[test]
    fn snapshot_copies_all_crypto_fields() {
        let mgr = TokenManager::new();
        let decryptor: Arc<dyn SourceDecryptor> = Arc::new(StubDecryptor);

        // Encrypted source: salts, the flag, the DEK identity, and the
        // decryptor handle all travel into the snapshot verbatim.
        let token = mgr.mint_token(0x88, encrypted_state(Arc::clone(&decryptor), true));
        let snap = mgr.snapshot_for(&token).expect("token present");
        assert_eq!(snap.iv_salts, vec![0xDEAD_BEEF, 0xBEEF_DEAD]);
        assert!(snap.source_encrypted);
        assert_eq!(snap.source_dek_uuid, [0xAAu8; 16]);
        assert!(
            Arc::ptr_eq(&snap.source_decryptor, &decryptor),
            "the same decryptor handle must be shared, not rebuilt"
        );

        // Unencrypted source: the flag and a distinct DEK identity pin
        // that the boolean discriminator is carried, not assumed.
        let token2 = mgr.mint_token(0x89, encrypted_state(Arc::clone(&decryptor), false));
        let snap2 = mgr.snapshot_for(&token2).expect("token present");
        assert!(!snap2.source_encrypted);
        assert_eq!(snap2.source_dek_uuid, [0xBBu8; 16]);
    }

    #[tokio::test]
    async fn snapshot_decryptor_is_callable() {
        let mgr = TokenManager::new();
        let decryptor: Arc<dyn SourceDecryptor> = Arc::new(StubDecryptor);
        let token = mgr.mint_token(0x8A, encrypted_state(decryptor, true));
        let snap = mgr.snapshot_for(&token).expect("token present");
        // The carried handle is a live trait object the WRITE USING
        // TOKEN recrypt path can actually invoke.
        let out = snap
            .source_decryptor
            .decrypt_chunk(0, [0xAAu8; 32], 0)
            .await;
        assert!(out.is_ok());
    }
}
