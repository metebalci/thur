// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SBC-3 dispatcher — routes incoming `ScsiRequest`s to the
//! per-opcode handlers and resolves LUNs via the [`VolumeLookup`]
//! trait (`thurvsad`'s `VolumeRegistry` implements it).
//!
//! Surface today: identity / sizing / discovery (TUR, INQUIRY +
//! VPD 0x00 / 0x80 / 0x83 / 0x8F / 0xB0 / 0xB2, READ CAPACITY 10 /
//! 16, REPORT LUNS, MODE SENSE 6 / 10 caching + control pages) plus
//! the sector-grain data path (WRITE 10 / 16, READ 10 / 16,
//! SYNCHRONIZE CACHE 10 / 16, COMPARE AND WRITE, UNMAP, EXTENDED
//! COPY + RECEIVE COPY RESULTS) routed through the per-volume
//! `PageCache`.
//!
//! The dispatcher is cheap to clone (`Arc` over the registry) and
//! the per-opcode arms are async — the data-path arms await the
//! cache (RMW + flush) which in turn awaits
//! `VolumeWriter::write_page` / `read_page`; the discovery arms
//! resolve synchronously.

use std::sync::Arc;

use async_trait::async_trait;
use shared_iscsi::admission::AdmissionView;
use shared_iscsi::alua::AluaTopology;
use shared_iscsi::unit_attention::UnitAttentionTracker;

use crate::VolumeLookup;
use crate::data_path;
use crate::data_path::CawLocks;
use crate::inquiry;
use crate::maintenance;
use crate::mode_sense;
use crate::odx::TokenManager;
use crate::probes;
use crate::reservations::{ReservationManager, SbcReservations, nexus_from_request};
use crate::sizing;
use crate::types::{ScsiRequest, ScsiResponse, SenseData};

/// thurvsa's default iSCSI target identifier — used when the
/// operator has not set `iscsi.target_iqn`. Sourced from
/// [`shared_naming::DISK`] — thurvtl's tape library uses
/// `iqn.2025-10.com.metebalci:thurvtl` so the two products'
/// target IQNs are distinguishable on the same network. Initiators
/// connect to the per-product port (3260 for VTL,3260for
/// thurvsa) and read this IQN out of the SendTargets / Login
/// response.
pub const ISCSI_DISK_TARGET_IQN: &str = shared_naming::DISK.iqn;

/// SBC-3 dispatcher. Holds an immutable reference to the LUN
/// registry (via `Arc<dyn VolumeLookup>`); concurrent dispatches are
/// safe (`VolumeWriter` methods take `&self`). The PERSISTENT
/// RESERVE manager and the per-LUN CAW lock registry hang off the
/// same handle so their state survives across dispatcher clones
/// (the dispatcher is itself cloneable into the per-connection task).
#[derive(Clone)]
pub struct SbcScsiDispatcher {
    registry: Arc<dyn VolumeLookup>,
    /// iSCSI target IQN advertised through `ScsiHandler::target_iqn`.
    /// Resolved at boot from `iscsi.target_iqn`, defaulting to
    /// [`ISCSI_DISK_TARGET_IQN`].
    target_iqn: String,
    reservations: Arc<ReservationManager>,
    caw_locks: Arc<CawLocks>,
    /// Hyper-V ODX state: outstanding ROD tokens + per-list-ID job
    /// outcomes. The sweeper task spawned in [`Self::new`] runs every
    /// 30 s and drops entries past their inactivity deadline.
    tokens: Arc<TokenManager>,
    /// ALUA topology — per-portal RTPI / TPGT + per-TPG asymmetric
    /// access state. Feeds VPD 0x83 TargetPort designators, VPD 0x86
    /// (TPGS field), and REPORT TARGET PORT GROUPS. Default
    /// single-portal deployments end up with one TPG in
    /// `ActiveOptimized`, which is the truthful answer for a single
    /// host accessing a single advertised iSCSI portal.
    alua: Arc<AluaTopology>,
    /// PR initiator-port policy (issue #57): when `true` the transport
    /// collapses the ISID so reservations key by IQN alone. Sourced from
    /// `iscsi.reservations.initiator_port`; default `false`.
    pr_collapse_isid: bool,
    /// Per-(TSIH, LUN) Unit Attention queue, shared with the iSCSI
    /// reservation-UA sink (issue #67). `None` in test / non-iSCSI
    /// construction, in which case the per-command UA check is skipped.
    /// The daemon injects the same tracker the `IscsiReservationSink`
    /// enqueues into, so a reservation change preempted over either
    /// transport surfaces here on the initiator's next command.
    ua: Option<Arc<UnitAttentionTracker>>,
    /// Live per-CHAP-user volume admission (VSA dynamic admission).
    /// `None` in test / non-admission construction, in which case the
    /// transport falls back to the session's login-time snapshot. When
    /// set, [`Self::live_admission`] resolves the *current* admitted
    /// set per command, so an `iscsi users grant` / `revoke` reaches a
    /// session that is already up — required by the Kubernetes CSI
    /// per-node CHAP model (one session per node, many volumes).
    admission: Option<Arc<AdmissionView>>,
}

impl SbcScsiDispatcher {
    /// Convenience constructor for tests + non-portal-aware call
    /// sites. Builds an `AluaTopology` over a single synthetic portal
    /// (RTPI=1, TPGT=1) so the dispatcher's ALUA surface still
    /// answers truthfully; production code passes through
    /// [`Self::with_alua`] from `thurvsad::main`.
    pub fn new(registry: Arc<dyn VolumeLookup>, target_iqn: String) -> Self {
        let portals = [shared_iscsi::transport::Portal {
            bind: String::new(),
            advertise: None,
            tpgt: 1,
        }];
        let alua = Arc::new(AluaTopology::from_portals(&portals, target_iqn.clone()));
        // In-memory reservation manager (PTPL not capable) + default
        // (IQN, ISID) initiator port + no UA tracker. Production injects a
        // persistence-backed manager, the configured port policy, and the
        // shared UA tracker via `with_alua`.
        Self::with_alua(
            registry,
            target_iqn,
            alua,
            Arc::new(ReservationManager::new()),
            false,
            None,
        )
    }

    /// Production constructor: pass the `AluaTopology` built from the
    /// daemon's `iscsi.listen_portals` so VPD 0x83 / VPD 0x86 / REPORT
    /// TPG reflect the real portal layout, the shared (PTPL-backed)
    /// `ReservationManager` the daemon loads from
    /// `<data_dir>/reservations.json`, and the PR initiator-port policy
    /// (`pr_collapse_isid`) from `iscsi.reservations.initiator_port`
    /// (issue #57).
    pub fn with_alua(
        registry: Arc<dyn VolumeLookup>,
        target_iqn: String,
        alua: Arc<AluaTopology>,
        reservations: Arc<ReservationManager>,
        pr_collapse_isid: bool,
        ua: Option<Arc<UnitAttentionTracker>>,
    ) -> Self {
        let tokens = Arc::new(TokenManager::new());
        // Best-effort sweeper: only spawns when called from within a
        // tokio runtime. Construction from non-tokio contexts (unit
        // tests outside `#[tokio::test]`) silently skips it; the
        // manager still drops expired entries on every live lookup.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let m = Arc::clone(&tokens);
            // Fire-and-forget background sweeper — we don't await or
            // store the JoinHandle. The closure runs for the daemon's
            // process lifetime; the upgraded Arc keeps the manager
            // alive (this is intentional, since the dispatcher Arc
            // holds the other strong reference and they share fate).
            std::mem::drop(handle.spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    m.sweep_expired();
                }
            }));
        }
        Self {
            registry,
            target_iqn,
            reservations,
            caw_locks: Arc::new(CawLocks::new()),
            tokens,
            alua,
            pr_collapse_isid,
            ua,
            admission: None,
        }
    }

    /// Attach the live admission view (VSA dynamic per-CHAP-user LUN
    /// admission). Builder-style so the constructors stay unchanged and
    /// test / VTL call sites need no admission wiring; `thurvsad::main`
    /// chains it after [`Self::with_alua`].
    pub fn with_admission(mut self, admission: Arc<AdmissionView>) -> Self {
        self.admission = Some(admission);
        self
    }

    /// Inject a shared per-LUN CAW lock registry so COMPARE AND WRITE
    /// over iSCSI serializes against fused Compare+Write over NVMe/TCP on
    /// the same volume under a dual-transport export (issue #128). The
    /// daemon hands the *same* `Arc<CawLocks>` to the NVMe dispatcher.
    /// Without this the dispatcher keeps its own per-instance registry.
    pub fn with_caw_locks(mut self, caw_locks: Arc<CawLocks>) -> Self {
        self.caw_locks = caw_locks;
        self
    }

    #[allow(dead_code)] // surfaced for tests / future admin reservations endpoint
    pub fn reservations(&self) -> &ReservationManager {
        &self.reservations
    }

    /// Run one SCSI command end-to-end. The caller (iSCSI session
    /// handler, today direct test code) is responsible for
    /// translating between the wire-format SCSI Command PDU /
    /// Data-Out PDUs and the [`ScsiRequest`] shape, and for
    /// emitting the matching SCSI Response / Data-In PDUs.
    pub async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse {
        if req.cdb.is_empty() {
            return ScsiResponse::check(SenseData::INVALID_OPCODE);
        }
        // Proactive Unit Attention (issue #67): report a pending
        // reservation UA before dispatching. INQUIRY (0x12), REQUEST
        // SENSE (0x03), and REPORT LUNS (0xA0) complete normally even
        // with a UA pending (SPC-4) so the initiator can still read
        // identity / drain sense / enumerate LUNs; any other opcode
        // preempts to CHECK CONDITION + UNIT ATTENTION and pops one UA.
        // The enqueue side is the shared `IscsiReservationSink`.
        if let Some(ua) = &self.ua
            && !matches!(req.cdb[0], 0x12 | 0x03 | 0xA0)
            && let Some(code) = ua.check_and_pop_ua(req.tsih, u8::try_from(req.lun).unwrap_or(0xFF))
        {
            return ScsiResponse::check(SenseData::new(
                scsi_spc::sense::SenseKey::UnitAttention,
                code.asc,
                code.ascq,
            ));
        }
        // Per-CHAP-user volume admission: when the session is bound
        // to an admission set, a non-admitted LUN must be invisible
        // to this session. Treat it as "no LU here" — leave
        // `cache_arc` as `None` so INQUIRY falls into the
        // peripheral-qualifier 0x3 path and TUR / READ CAPACITY /
        // data-path arms emit `LU_NOT_SUPPORTED`. The REPORT LUNS
        // arm uses `luns_filtered` directly so unauthorised LUNs
        // don't surface there either.
        //
        // Hold the cloned `Arc<PageCache>` through the awaited arms
        // below; binding the borrowed `&PageCache` directly would
        // tie its lifetime to the temporary returned by
        // `registry.get`, which Rust drops after the `match`.
        let cache_arc = match req.session_volumes {
            Some(allow) => match self.registry.name_for_lun(req.lun) {
                Some(name) if allow.iter().any(|n| n == &name) => self.registry.get(req.lun),
                _ => None,
            },
            None => self.registry.get(req.lun),
        };
        let cache = cache_arc.as_deref();
        let nexus = nexus_from_request(&req);
        match req.cdb[0] {
            0x00 => sizing::test_unit_ready(&req, cache),
            0x03 => probes::request_sense(&req, cache),
            0x12 => inquiry::dispatch(&req, cache, &self.alua),
            0x15 => mode_sense::mode_select_6(&req, cache),
            0x1A => mode_sense::mode_sense_6(&req, cache),
            0x1B => probes::start_stop_unit(&req, cache),
            0x1E => probes::prevent_allow_medium_removal(&req, cache),
            0x25 => sizing::read_capacity_10(&req, cache),
            0x28 | 0x88 => data_path::read(&req, cache, nexus, &self.reservations).await,
            0x2A | 0x8A => data_path::write(&req, cache, nexus, &self.reservations).await,
            0x2F | 0x8F => data_path::verify(&req, cache, nexus, &self.reservations).await,
            0x35 | 0x91 => {
                data_path::synchronize_cache(&req, cache, nexus, &self.reservations).await
            }
            0x41 | 0x93 => data_path::write_same(&req, cache, nexus, &self.reservations).await,
            0x42 => data_path::unmap(&req, cache, nexus, &self.reservations).await,
            0x4D => probes::log_sense(&req, cache),
            0x55 => mode_sense::mode_select_10(&req, cache),
            0x5A => mode_sense::mode_sense_10(&req, cache),
            0x5E => self
                .reservations
                .persistent_reserve_in(&req, cache.is_some()),
            0x5F => self.reservations.persistent_reserve_out(&req, cache, nexus),
            0x83 => {
                data_path::extended_copy(
                    &req,
                    &self.registry,
                    nexus,
                    &self.reservations,
                    &self.tokens,
                )
                .await
            }
            0x84 => data_path::receive_copy_results(&req, &self.tokens),
            0x89 => {
                data_path::compare_and_write(
                    &req,
                    cache,
                    nexus,
                    &self.reservations,
                    &self.caw_locks,
                )
                .await
            }
            0x9E => sizing::service_action_in_16(&req, cache),
            0xA0 => sizing::report_luns(&req, &self.registry.luns_filtered(req.session_volumes)),
            0xA3 => maintenance::maintenance_in(&req, &self.alua),
            _ => ScsiResponse::check(SenseData::INVALID_OPCODE),
        }
    }
}

#[async_trait]
impl shared_iscsi::ScsiHandler for SbcScsiDispatcher {
    fn target_iqn(&self) -> &str {
        &self.target_iqn
    }

    fn pr_collapse_isid(&self) -> bool {
        self.pr_collapse_isid
    }

    /// Dynamic admission (VSA): resolve a CHAP user's *current*
    /// admitted-volume set from the live view, so the transport fences
    /// each command against the up-to-date set rather than the login
    /// snapshot. `None` when no view is wired (tests / VTL) — the
    /// transport then keeps the snapshot. An unknown user resolves to
    /// the empty set (sees nothing), the safe fallback for a session
    /// whose user was removed mid-flight.
    fn live_admission(&self, username: &str) -> Option<Arc<Vec<String>>> {
        let view = self.admission.as_ref()?;
        Some(view.get(username).unwrap_or_else(|| Arc::new(Vec::new())))
    }

    async fn dispatch(&self, req: shared_iscsi::ScsiRequest<'_>) -> shared_iscsi::ScsiResponse {
        // shared-iscsi's ScsiRequest collapsed into scsi-spc's
        // (Step 5.A.2); thurvsa's local alias resolves to the same
        // type, so the request flows through unchanged.
        self.dispatch(req).await
    }

    fn on_session_close(&self, tsih: u16, _cid: u16) {
        // Drop any pending unit-attention entries keyed by this TSIH so
        // the map can't grow unbounded with session churn and a future
        // session reusing the TSIH can't inherit the dead one's queued
        // UAs (issue #241). Persistent reservations are deliberately NOT
        // released here: SPC-4 PRs survive I_T nexus loss and are keyed
        // by the stable initiator port (IQN + ISID), removed only by an
        // explicit PROUT or, when APTPL=0, a daemon restart (issue #57).
        if let Some(ua) = self.ua.as_ref() {
            ua.clear_session(tsih);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_iscsi::ScsiHandler;
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::collections::BTreeMap;
    use std::sync::RwLock;
    use tempfile::TempDir;

    /// Test-only [`VolumeLookup`] impl. The real daemon's
    /// `VolumeRegistry` lives in `thurvsad`; scsi-sbc carries
    /// only the trait, so test fixtures need their own implementation.
    #[derive(Default)]
    struct TestRegistry {
        by_lun: RwLock<BTreeMap<u64, Arc<PageCache>>>,
    }

    impl TestRegistry {
        fn new() -> Self {
            Self::default()
        }

        fn register(&self, lun: u64, cache: Arc<PageCache>) {
            self.by_lun.write().unwrap().insert(lun, cache);
        }
    }

    impl VolumeLookup for TestRegistry {
        fn get(&self, lun: u64) -> Option<Arc<PageCache>> {
            self.by_lun.read().unwrap().get(&lun).map(Arc::clone)
        }
        fn luns(&self) -> Vec<u64> {
            self.by_lun.read().unwrap().keys().copied().collect()
        }
        fn name_for_lun(&self, lun: u64) -> Option<String> {
            self.by_lun
                .read()
                .unwrap()
                .get(&lun)
                .map(|c| c.manifest().name.clone())
        }
        fn luns_filtered(&self, allow: Option<&[String]>) -> Vec<u64> {
            let m = self.by_lun.read().unwrap();
            match allow {
                None => m.keys().copied().collect(),
                Some(names) => m
                    .iter()
                    .filter(|(_, c)| names.iter().any(|n| n == &c.manifest().name))
                    .map(|(lun, _)| *lun)
                    .collect(),
            }
        }
    }

    async fn handler_with_one_volume() -> (TempDir, SbcScsiDispatcher) {
        let tmp = TempDir::new().unwrap();
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

        VolumeManifest::new(
            "vol1".into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::new(writer);

        let registry = TestRegistry::new();
        registry.register(0, cache);
        let handler = SbcScsiDispatcher::new(Arc::new(registry), ISCSI_DISK_TARGET_IQN.to_string());
        (tmp, handler)
    }

    #[test]
    fn target_iqn_reflects_the_constructed_value() {
        let handler = SbcScsiDispatcher::new(
            Arc::new(TestRegistry::new()),
            "iqn.2025-10.com.example:custom".to_string(),
        );
        assert_eq!(
            shared_iscsi::ScsiHandler::target_iqn(&handler),
            "iqn.2025-10.com.example:custom"
        );
    }

    fn req<'a>(cdb: &'a [u8], lun: u64) -> ScsiRequest<'a> {
        ScsiRequest {
            lun,
            cdb,
            data_out: Vec::new(),
            data_in_max: 4096,
            tsih: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            cid: 0,
            peer: "",
            session_partition: None,
            session_volumes: None,
        }
    }

    #[tokio::test]
    async fn empty_cdb_is_invalid_opcode() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let resp = handler.dispatch(req(&[], 0)).await;
        assert_eq!(resp.sense, Some(SenseData::INVALID_OPCODE));
    }

    #[tokio::test]
    async fn unknown_opcode_is_invalid_opcode() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let resp = handler.dispatch(req(&[0xFFu8; 16], 0)).await;
        assert_eq!(resp.sense, Some(SenseData::INVALID_OPCODE));
    }

    #[tokio::test]
    async fn inquiry_routes_through_dispatch() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0];
        let resp = handler.dispatch(req(&cdb, 0)).await;
        assert!(resp.sense.is_none());
        assert_eq!(&resp.data_in[8..16], b"MB      ");
    }

    #[tokio::test]
    async fn report_luns_lists_registered_lun() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[6..10].copy_from_slice(&64u32.to_be_bytes());
        let resp = handler.dispatch(req(&cdb, 0)).await;
        let listed = u32::from_be_bytes([
            resp.data_in[0],
            resp.data_in[1],
            resp.data_in[2],
            resp.data_in[3],
        ]);
        assert_eq!(listed, 8);
    }

    /// One-volume fixture wired with a shared UA tracker (issue #67), so
    /// the per-command reservation-UA pop can be exercised.
    async fn handler_with_ua() -> (TempDir, SbcScsiDispatcher, Arc<UnitAttentionTracker>) {
        let tmp = TempDir::new().unwrap();
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            "vol1".into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::new(writer);
        let registry = TestRegistry::new();
        registry.register(0, cache);
        let ua = Arc::new(UnitAttentionTracker::new());
        let alua = Arc::new(AluaTopology::from_portals(
            &[shared_iscsi::transport::Portal {
                bind: String::new(),
                advertise: None,
                tpgt: 1,
            }],
            ISCSI_DISK_TARGET_IQN.to_string(),
        ));
        let handler = SbcScsiDispatcher::with_alua(
            Arc::new(registry),
            ISCSI_DISK_TARGET_IQN.to_string(),
            alua,
            Arc::new(ReservationManager::new()),
            false,
            Some(Arc::clone(&ua)),
        );
        (tmp, handler, ua)
    }

    #[tokio::test]
    async fn pending_reservation_ua_preempts_data_path_and_clears() {
        use shared_iscsi::unit_attention::UnitAttentionCode;
        let (_tmp, handler, ua) = handler_with_ua().await;
        ua.add_ua(0, 0, UnitAttentionCode::RESERVATIONS_PREEMPTED);
        // A non-exempt opcode (TEST UNIT READY) is preempted with CHECK
        // CONDITION + UNIT ATTENTION carrying the queued ASC/ASCQ.
        let resp = handler.dispatch(req(&[0x00, 0, 0, 0, 0, 0], 0)).await;
        assert_eq!(
            resp.sense,
            Some(SenseData::new(
                scsi_spc::sense::SenseKey::UnitAttention,
                0x2A,
                0x03
            ))
        );
        // The UA was popped: the same opcode now proceeds normally.
        let resp2 = handler.dispatch(req(&[0x00, 0, 0, 0, 0, 0], 0)).await;
        assert!(resp2.sense.is_none());
    }

    /// Issue #241: closing a session must drop its pending UAs so the
    /// map can't leak and a reused TSIH can't inherit stale UAs.
    #[tokio::test]
    async fn on_session_close_clears_pending_uas() {
        use shared_iscsi::ScsiHandler;
        use shared_iscsi::unit_attention::UnitAttentionCode;
        let (_tmp, handler, ua) = handler_with_ua().await;
        ua.add_ua(7, 0, UnitAttentionCode::RESERVATIONS_PREEMPTED);
        assert!(ua.has_pending_ua(7, 0));

        handler.on_session_close(7, 0);

        assert!(
            !ua.has_pending_ua(7, 0),
            "on_session_close must clear the session's queued UAs (a reused \
             TSIH would otherwise inherit them)"
        );
    }

    #[tokio::test]
    async fn exempt_opcodes_do_not_pop_reservation_ua() {
        use shared_iscsi::unit_attention::UnitAttentionCode;
        let (_tmp, handler, ua) = handler_with_ua().await;
        ua.add_ua(0, 0, UnitAttentionCode::RESERVATIONS_RELEASED);
        // INQUIRY (0x12) completes normally and must NOT pop the UA.
        let inq = handler.dispatch(req(&[0x12, 0, 0, 0x00, 0x60, 0], 0)).await;
        assert!(inq.sense.is_none());
        assert!(
            ua.has_pending_ua(0, 0),
            "exempt opcode must leave the UA queued"
        );
        // A non-exempt opcode then still surfaces it as 0x2A/0x04.
        let resp = handler.dispatch(req(&[0x00, 0, 0, 0, 0, 0], 0)).await;
        assert_eq!(
            resp.sense,
            Some(SenseData::new(
                scsi_spc::sense::SenseKey::UnitAttention,
                0x2A,
                0x04
            ))
        );
    }

    /// Build a fixture with two volumes ("vol1", "vol2") on LUNs 0 and 1
    /// — enough surface to verify admission filters one out.
    async fn handler_with_two_volumes() -> (TempDir, SbcScsiDispatcher) {
        let tmp = TempDir::new().unwrap();
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        let registry = TestRegistry::new();
        for (lun, name) in [(0u64, "vol1"), (1u64, "vol2")] {
            VolumeManifest::new(
                name.into(),
                4 * (1u64 << 20),
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                DedupScope::Local,
                false,
                0,
            )
            .unwrap()
            .create(tmp.path())
            .unwrap();
            let writer =
                Arc::new(VolumeWriter::open(tmp.path(), name, Arc::clone(&backend)).unwrap());
            registry.register(lun, PageCache::new(writer));
        }
        let handler = SbcScsiDispatcher::new(Arc::new(registry), ISCSI_DISK_TARGET_IQN.to_string());
        (tmp, handler)
    }

    fn req_with_volumes<'a>(cdb: &'a [u8], lun: u64, allow: &'a [String]) -> ScsiRequest<'a> {
        ScsiRequest {
            lun,
            cdb,
            data_out: Vec::new(),
            data_in_max: 4096,
            tsih: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            cid: 0,
            peer: "",
            session_partition: None,
            session_volumes: Some(allow),
        }
    }

    #[tokio::test]
    async fn report_luns_filters_to_session_volume_set() {
        let (_tmp, handler) = handler_with_two_volumes().await;
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[6..10].copy_from_slice(&64u32.to_be_bytes());
        let allow = vec!["vol2".to_string()];

        let resp = handler.dispatch(req_with_volumes(&cdb, 0, &allow)).await;
        let lun_list_len = u32::from_be_bytes([
            resp.data_in[0],
            resp.data_in[1],
            resp.data_in[2],
            resp.data_in[3],
        ]);
        // One admitted LUN (vol2 at LUN 1) → 8 bytes of LUN entries.
        assert_eq!(lun_list_len, 8);
        // The reported LUN byte (low byte of the 8-byte LUN entry) is 1.
        assert_eq!(resp.data_in[8 + 1], 1);
    }

    #[tokio::test]
    async fn inquiry_returns_pq_no_lu_for_non_admitted() {
        let (_tmp, handler) = handler_with_two_volumes().await;
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0];
        let allow = vec!["vol1".to_string()];

        // LUN 0 (vol1) is admitted — pq = 0x0, pt = 0x0 (direct-access).
        let resp = handler.dispatch(req_with_volumes(&cdb, 0, &allow)).await;
        assert!(resp.sense.is_none());
        assert_eq!(resp.data_in[0] & 0xE0, 0x00, "pq for admitted LUN is 0");

        // LUN 1 (vol2) is not admitted — pq = 0x3, pt = 0x1F.
        let resp = handler.dispatch(req_with_volumes(&cdb, 1, &allow)).await;
        assert!(resp.sense.is_none());
        let pq = resp.data_in[0] & 0xE0;
        let pt = resp.data_in[0] & 0x1F;
        assert_eq!(pq, 0x60, "pq=0x3 (no LU here) for non-admitted LUN");
        assert_eq!(pt, 0x1F, "pt=0x1F (unknown) for non-admitted LUN");
    }

    #[tokio::test]
    async fn read_capacity_10_routes() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let resp = handler.dispatch(req(&cdb, 0)).await;
        assert!(resp.sense.is_none());
        assert_eq!(resp.data_in.len(), 8);
    }

    #[tokio::test]
    async fn live_admission_is_none_without_a_view() {
        // VTL / test construction wires no admission view → the
        // transport keeps the login snapshot, so the hook yields None.
        let (_tmp, handler) = handler_with_two_volumes().await;
        assert!(handler.live_admission("anyone").is_none());
    }

    #[tokio::test]
    async fn live_admission_unknown_user_sees_nothing() {
        // With a view wired, a CHAP user that isn't in it resolves to
        // the empty set (Some, not None) — a session whose user was
        // removed mid-flight goes dark rather than reverting to a stale
        // snapshot.
        let (_tmp, handler) = handler_with_two_volumes().await;
        let handler = handler.with_admission(Arc::new(AdmissionView::new()));
        let set = handler
            .live_admission("ghost")
            .expect("view present → Some(empty)");
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn live_admission_makes_report_luns_track_a_grant() {
        // The crux of VSA dynamic admission (issue #15): resolving the
        // session's admitted set through `live_admission` each command
        // means an `iscsi users grant` (modeled here as `view.set`) is
        // visible to REPORT LUNS on the SAME handler — no re-login.
        let (_tmp, handler) = handler_with_two_volumes().await;
        let view = Arc::new(AdmissionView::new());
        let handler = handler.with_admission(Arc::clone(&view));
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[6..10].copy_from_slice(&64u32.to_be_bytes());

        // Granted vol2 only → REPORT LUNS shows one LUN (vol2 @ LUN 1).
        view.set("csi-node-a", vec!["vol2".to_string()]);
        let live = handler.live_admission("csi-node-a").unwrap();
        let resp = handler
            .dispatch(req_with_volumes(&cdb, 0, live.as_slice()))
            .await;
        let len = u32::from_be_bytes([
            resp.data_in[0],
            resp.data_in[1],
            resp.data_in[2],
            resp.data_in[3],
        ]);
        assert_eq!(len, 8, "one admitted LUN before the grant");
        assert_eq!(resp.data_in[8 + 1], 1, "the one LUN is vol2 @ LUN 1");

        // Grant vol1 too → the same handler now reports both LUNs.
        view.set("csi-node-a", vec!["vol1".to_string(), "vol2".to_string()]);
        let live = handler.live_admission("csi-node-a").unwrap();
        let resp = handler
            .dispatch(req_with_volumes(&cdb, 0, live.as_slice()))
            .await;
        let len = u32::from_be_bytes([
            resp.data_in[0],
            resp.data_in[1],
            resp.data_in[2],
            resp.data_in[3],
        ]);
        assert_eq!(len, 16, "both LUNs admitted after the grant — no re-login");
    }

    #[tokio::test]
    async fn unmapped_lun_inquiry_still_succeeds() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0];
        let resp = handler.dispatch(req(&cdb, 99)).await;
        assert!(resp.sense.is_none()); // SPC-4 mandates success
        assert_eq!(resp.data_in[0], 0x7F); // peripheral qualifier 0b011, type 0x1F
    }

    #[tokio::test]
    async fn unmapped_lun_capacity_check_condition() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let resp = handler.dispatch(req(&cdb, 99)).await;
        assert_eq!(resp.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn write10_then_read10_round_trip_via_dispatch() {
        // 64 KiB page / 4 KiB sector → 16 sectors per page.
        let (_tmp, handler) = handler_with_one_volume().await;
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();

        let mut wcdb = [0u8; 10];
        wcdb[0] = 0x2A;
        wcdb[7..9].copy_from_slice(&16u16.to_be_bytes());
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &wcdb,
                data_out: payload.to_vec(),
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let mut rcdb = [0u8; 10];
        rcdb[0] = 0x28;
        rcdb[7..9].copy_from_slice(&16u16.to_be_bytes());
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &rcdb,
                data_out: Vec::new(),
                data_in_max: 64 * 1024,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, payload);
    }

    #[tokio::test]
    async fn synchronize_cache_10_routes() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let r = handler.dispatch(req(&cdb, 0)).await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn mode_sense_6_routes_through_dispatch() {
        let (_tmp, handler) = handler_with_one_volume().await;
        // page code 0x08 (caching), DBD=0, alloc=0xFF.
        let cdb = [0x1Au8, 0x00, 0x08, 0x00, 0xFF, 0x00];
        let r = handler.dispatch(req(&cdb, 0)).await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        // 4-byte header + 8-byte block descriptor + 20-byte page = 32.
        assert_eq!(r.data_in.len(), 32);
        assert_eq!(r.data_in[12], 0x08); // caching page header
    }

    #[tokio::test]
    async fn mode_sense_10_routes_through_dispatch() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let mut cdb = [0u8; 10];
        cdb[0] = 0x5A;
        cdb[1] = 0x08; // DBD=1
        cdb[2] = 0x0A; // page code = control
        cdb[7..9].copy_from_slice(&4096u16.to_be_bytes());
        let r = handler.dispatch(req(&cdb, 0)).await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        assert_eq!(r.data_in.len(), 8 + 12); // header + control page
        assert_eq!(r.data_in[8], 0x0A);
    }

    #[tokio::test]
    async fn mode_select_6_routes_through_dispatch() {
        // Issue MODE SENSE first to capture the current caching page,
        // then re-write it via MODE SELECT — the round-trip should
        // succeed.
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x1Au8, 0x00, 0x08, 0x00, 0xFF, 0x00];
        let sense = handler.dispatch(req(&cdb, 0)).await;
        let mut params = sense.data_in;
        params[0] = 0;
        params[1] = 0;
        params[2] = 0;
        let mut sel_cdb = [0u8; 6];
        sel_cdb[0] = 0x15;
        sel_cdb[1] = 0x10; // PF=1
        sel_cdb[4] = params.len() as u8;
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &sel_cdb,
                data_out: params.to_vec(),
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn mode_select_10_routes_through_dispatch() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let mut cdb = [0u8; 10];
        cdb[0] = 0x5A;
        cdb[2] = 0x08;
        cdb[7..9].copy_from_slice(&4096u16.to_be_bytes());
        let sense = handler.dispatch(req(&cdb, 0)).await;
        let mut params = sense.data_in;
        params[0] = 0;
        params[1] = 0;
        params[2] = 0;
        params[3] = 0;
        let mut sel_cdb = [0u8; 10];
        sel_cdb[0] = 0x55;
        sel_cdb[1] = 0x10;
        sel_cdb[7..9].copy_from_slice(&(params.len() as u16).to_be_bytes());
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &sel_cdb,
                data_out: params.to_vec(),
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn mode_sense_6_unmapped_lun_check_condition() {
        let (_tmp, handler) = handler_with_one_volume().await;
        let cdb = [0x1Au8, 0x00, 0x08, 0x00, 0xFF, 0x00];
        let r = handler.dispatch(req(&cdb, 99)).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn unmap_routes_through_dispatch() {
        let (_tmp, handler) = handler_with_one_volume().await;
        // Header-only parameter list (0 descriptors) — valid no-op.
        let mut cdb = [0u8; 10];
        cdb[0] = 0x42;
        cdb[7..9].copy_from_slice(&8u16.to_be_bytes());
        let mut params = [0u8; 8];
        params[0..2].copy_from_slice(&6u16.to_be_bytes()); // unmap data length = 6
        params[2..4].copy_from_slice(&0u16.to_be_bytes()); // descriptor list length = 0
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &cdb,
                data_out: params.to_vec(),
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn compare_and_write_routes_through_dispatch() {
        // 64 KiB page / 4 KiB sector → 16 sectors per page. CAW
        // single-page over an unallocated page with all-zero compare
        // buffer should commit the write.
        let (_tmp, handler) = handler_with_one_volume().await;
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();
        let mut combined = vec![0u8; 64 * 1024];
        combined.extend_from_slice(&payload);

        let mut cdb = [0u8; 16];
        cdb[0] = 0x89;
        cdb[2..10].copy_from_slice(&0u64.to_be_bytes());
        cdb[13] = 16; // 16 sectors = one full page
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &cdb,
                data_out: combined.to_vec(),
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        // Verify the write half landed.
        let mut rcdb = [0u8; 10];
        rcdb[0] = 0x28;
        rcdb[7..9].copy_from_slice(&16u16.to_be_bytes());
        let r = handler
            .dispatch(ScsiRequest {
                lun: 0,
                cdb: &rcdb,
                data_out: Vec::new(),
                data_in_max: 64 * 1024,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, payload);
    }
}
