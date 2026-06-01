// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // Drive management infrastructure - public API for future use

use core_mediachanger::errors::SmcError;
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, CompressionAlgo, DriveCompressionState, DrivePageStore,
    DriveState, GhostList, LibraryDriveState, PoolBudget,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use tracing::{info, warn};

/// Drive Manager - manages tape drives and their reservations.
///
/// Per-drive locking: the drive table is built once in `new()` and
/// never mutated afterward, so the map itself needs no outer lock —
/// each `Drive` lives behind its own `Mutex`. A long WRITE on drive 0
/// (which can park on `PoolBudget` for `backpressure_max_wait`
/// seconds) only holds drive 0's mutex; commands targeting drive 1
/// proceed unblocked. Iteration methods
/// (`get_drive_info`/`cleanup_stale_locks`/`release_session_locks`)
/// lock each drive in turn — they never hold two drive locks at once,
/// so no fixed lock ordering is required.
pub struct DriveManager {
    drives: Arc<HashMap<usize, Arc<Mutex<Drive>>>>,
    tapes_root: PathBuf,
    /// Algorithm the drive uses *when the host turns DCE on*. Real
    /// LTO drives ship DCE off at every cartridge load; we mirror
    /// that — there is intentionally no `drive_compression_default`
    /// knob. The host flips DCE via MODE SELECT page 0x0F per
    /// session. Recorded per-block in the manifest
    /// (`BlockIndex.compression`); changing this knob only affects
    /// future writes, not reads of older blocks.
    drive_compression_algorithm: CompressionAlgo,
    /// Zstd level — only consulted when `drive_compression_algorithm`
    /// is `Zstd`. Ignored for `Lz4` / `Sldc`.
    drive_compression_zstd_level: i32,
    /// Per-backend pool budgets, keyed by `cloud.backends` entry name.
    /// Wired into every cartridge at load time so chunk-seal applies
    /// upload backpressure when the local pool is at its hard cap.
    /// Empty map (and `backpressure_deadline = 60 s`) is the test /
    /// non-daemon default.
    pool_budgets: HashMap<String, Arc<PoolBudget>>,
    /// Per-backend ghost lists, keyed by `cloud.backends` entry name.
    /// Wired into every cartridge at load time so the cache-miss
    /// read path can record `cache_miss_after_eviction` histogram
    /// entries.
    ghost_lists: HashMap<String, Arc<GhostList>>,
    /// Maximum time a chunk-seal blocks on the budget before
    /// surfacing `Backpressured` (mapped to SCSI NOT READY). Mirrors
    /// `upload.backpressure_max_wait_seconds` in the daemon config.
    backpressure_deadline: Duration,
    /// Library-wide drive-state file path
    /// (`<data_dir>/library/drive_state.json`). Loaded at startup,
    /// rewritten atomically every time a host issues MODE SELECT
    /// SP=1. `None` in test setups where `tapes_root` has no parent
    /// — persistence becomes a no-op and state lives in memory only.
    state_file: Option<PathBuf>,
    /// Library-wide drive LTO generation. All drives in a thurvtl
    /// chassis share one generation — SMC-3 doesn't require this,
    /// but we deliberately simplify. Used at `load_cartridge`
    /// time to gate cartridges whose LTO generation exceeds the
    /// drive's read range — including LTO-7 Type M (`M8` barcode),
    /// which is rejected on LTO-7 drives even though the substrate is
    /// LTO-7. `0` = unknown / legacy library, gate disabled.
    library_lto_generation: u8,
    /// Per-cartridge at-rest DEK cache. The daemon populates this
    /// at boot (one async keystore unwrap per encrypted cartridge)
    /// and at `cartridge create --keystore` (the daemon already has
    /// the plaintext DEK from the generate-and-wrap call). The
    /// SCSI MOVE MEDIUM hot path is sync, so we cannot unwrap a
    /// DEK from inside `load_cartridge` — we must look it up.
    /// Empty for cartridges without at-rest encryption; an entry
    /// missing for an encrypted cartridge means the daemon could
    /// not reach the keystore at boot (load_cartridge refuses
    /// rather than silently producing plaintext-vs-ciphertext mix).
    dek_cache: Arc<Mutex<HashMap<String, [u8; shared_crypto::KEY_LEN]>>>,
}

/// Drive - represents a single tape drive
pub struct Drive {
    pub id: usize,
    pub cartridge: Option<Cartridge>,
    pub locked_by_session: Option<u16>, // TSIH of locking session
    pub lock_time: Option<Instant>,
    /// Per-drive runtime state — emulated drive NVRAM. Holds SCSI
    /// MODE SELECT round-trip bodies (host SP=1 persisted) plus any
    /// future drive-side knob the host writes. Survives cartridge
    /// swaps the way real LTO drive state does. Persisted by the
    /// `DriveManager` to `<data_dir>/library/drive_state.json`.
    pub state: DriveState,
    /// Per-I_T-nexus PREVENT/ALLOW MEDIUM REMOVAL state, keyed by
    /// TSIH (one I_T_L nexus per session). Volatile — cleared when
    /// the session goes away. Drive is "prevented" if any entry has
    /// the relevant bit set (SPC-4 multi-initiator rule).
    pub prevent: HashMap<u16, PreventBits>,
}

/// PREVENT/ALLOW MEDIUM REMOVAL state for one I_T_L nexus. cdb[4]
/// bits 1-0 are tracked independently per SPC-4 §6.13:
///
/// - `data_transport` (bit 0) — gates SCSI UNLOAD on this drive and
///   MOVE MEDIUM with this drive as the source element.
/// - `mechanical` (bit 1) — gates the admin
///   `POST /api/v1/changer/unload` endpoint, which is the
///   operator-console analog of pressing the front-panel eject
///   button on a real LTO chassis. `force: true` on the admin
///   request overrides this bit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreventBits {
    pub data_transport: bool,
    pub mechanical: bool,
}

impl PreventBits {
    pub fn is_zero(&self) -> bool {
        !self.data_transport && !self.mechanical
    }
}

impl DriveManager {
    /// Create a new DriveManager with the specified number of drives.
    /// LZ4 algorithm, default zstd level — use
    /// `with_compression_settings` to override.
    pub fn new(num_drives: usize, tapes_root: PathBuf) -> Self {
        Self::with_compression_settings(
            num_drives,
            tapes_root,
            CompressionAlgo::Lz4,
            core_mediachanger::ZSTD_DEFAULT_LEVEL,
        )
    }

    /// Create a new DriveManager configured with the algorithm and
    /// zstd level the drive will use *if the host turns DCE on* via
    /// MODE SELECT page 0x0F. DCE itself starts off at every
    /// cartridge load, matching real LTO drive behavior.
    pub fn with_compression_settings(
        num_drives: usize,
        tapes_root: PathBuf,
        drive_compression_algorithm: CompressionAlgo,
        drive_compression_zstd_level: i32,
    ) -> Self {
        let state_file = derive_drive_state_path(&tapes_root);
        let mut persisted = state_file
            .as_ref()
            .map(load_library_drive_state)
            .unwrap_or_default();

        let mut drives_map = HashMap::new();
        for id in 0..num_drives {
            // Hydrate per-drive state from the on-disk envelope if the
            // host had previously issued MODE SELECT SP=1; otherwise
            // start with defaults.
            let state = persisted.drives.remove(&id).unwrap_or_default();
            drives_map.insert(
                id,
                Arc::new(Mutex::new(Drive {
                    id,
                    cartridge: None,
                    locked_by_session: None,
                    lock_time: None,
                    state,
                    prevent: HashMap::new(),
                })),
            );
            info!("Initialized drive {} (no cartridge loaded)", id);
        }

        Self {
            drives: Arc::new(drives_map),
            tapes_root,
            drive_compression_algorithm,
            drive_compression_zstd_level,
            pool_budgets: HashMap::new(),
            ghost_lists: HashMap::new(),
            backpressure_deadline: Duration::from_secs(60),
            state_file,
            library_lto_generation: 0,
            dek_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Stash an unwrapped at-rest DEK for `cartridge_label`. Called
    /// by the daemon at boot (after async keystore unwrap of every
    /// encrypted cartridge's wrapped DEK) and at `cartridge
    /// create --keystore` (after the daemon's keystore
    /// generate-and-wrap call, which already returned the plaintext
    /// DEK). Once cached, `load_cartridge` injects it into the
    /// Cartridge's runtime state so the SCSI write/read seams can
    /// encrypt/decrypt without re-touching the keystore.
    pub fn set_cartridge_dek(&self, cartridge_label: &str, dek: [u8; shared_crypto::KEY_LEN]) {
        if let Ok(mut g) = self.dek_cache.lock() {
            g.insert(cartridge_label.to_string(), dek);
        }
    }

    /// Drop the cached DEK for `cartridge_label`. Called when the
    /// daemon destroys a cartridge so plaintext key material doesn't
    /// linger.
    pub fn forget_cartridge_dek(&self, cartridge_label: &str) {
        if let Ok(mut g) = self.dek_cache.lock() {
            g.remove(cartridge_label);
        }
    }

    /// Set the library-wide drive LTO generation. Called once during
    /// daemon startup once the `Library` is loaded. Cartridges whose
    /// LTO generation exceeds this value (or which are LTO-7 Type M
    /// against an LTO-7 drive) are refused at `load_cartridge` time
    /// with `IncompatibleMedium`.
    pub fn set_library_lto_generation(&mut self, lto_generation: u8) {
        self.library_lto_generation = lto_generation;
    }

    /// Wire per-backend pool budgets and the backpressure deadline
    /// into this manager. Called once at daemon startup; every
    /// subsequent `load_cartridge` will set the matching budget on
    /// the loaded cartridge so chunk-seal participates in
    /// backpressure.
    pub fn set_pool_budgets(
        &mut self,
        pool_budgets: HashMap<String, Arc<PoolBudget>>,
        backpressure_deadline: Duration,
    ) {
        self.pool_budgets = pool_budgets;
        self.backpressure_deadline = backpressure_deadline;
    }

    /// Wire per-backend ghost lists into this manager. Called once at
    /// daemon startup; every subsequent `load_cartridge` will set the
    /// matching ghost list on the loaded cartridge so the read path's
    /// cache-miss site can record histogram entries.
    pub fn set_ghost_lists(&mut self, ghost_lists: HashMap<String, Arc<GhostList>>) {
        self.ghost_lists = ghost_lists;
    }

    /// Lock a single drive's mutex. Returns `InvalidDrive` if the id
    /// is out of range and `InvalidOp` if the per-drive mutex is
    /// poisoned. Each drive has its own mutex; locking one never
    /// blocks operations on another.
    fn drive_lock(&self, drive_id: usize) -> Result<std::sync::MutexGuard<'_, Drive>, SmcError> {
        self.drives
            .get(&drive_id)
            .ok_or(SmcError::InvalidDrive(drive_id))?
            .lock()
            .map_err(|_| SmcError::InvalidOp("drive mutex poisoned"))
    }

    /// Try to acquire a lock on a drive for a session
    pub fn lock_drive(&self, drive_id: usize, tsih: u16) -> Result<(), SmcError> {
        let mut drive = self.drive_lock(drive_id)?;

        if let Some(locked_tsih) = drive.locked_by_session
            && locked_tsih != tsih
        {
            warn!(
                "Drive {} locked by session TSIH={}, rejecting TSIH={}",
                drive_id, locked_tsih, tsih
            );
            return Err(SmcError::DriveReserved(drive_id));
        }

        drive.locked_by_session = Some(tsih);
        drive.lock_time = Some(Instant::now());
        Ok(())
    }

    /// Release a lock on a drive
    pub fn unlock_drive(&self, drive_id: usize, tsih: u16) -> Result<(), SmcError> {
        let mut drive = self.drive_lock(drive_id)?;

        if drive.locked_by_session == Some(tsih) {
            drive.locked_by_session = None;
            drive.lock_time = None;
        }

        Ok(())
    }

    /// Execute an operation on a drive with automatic locking.
    ///
    /// Holds *only this drive's* mutex across `f`. A long-running
    /// operation here (e.g. `cart.write_data` parking on
    /// `PoolBudget` during upload backpressure) does not block SCSI
    /// commands targeting other drives.
    /// Snapshot the per-drive SCSI mode-page round-trip state.
    /// Returned by value so the caller can release the drive lock
    /// before consulting it from a MODE SENSE page builder. Empty for
    /// drives that have never seen a MODE SELECT.
    pub fn mode_pages_state(&self, drive_id: usize) -> Result<DrivePageStore, SmcError> {
        let drive = self.drive_lock(drive_id)?;
        Ok(drive.state.mode_pages.clone())
    }

    /// Logical Block Protection enables decoded from the saved Mode
    /// Page 0x0A/0xF0 (Control Data Protection) body. Returns
    /// `(write_check, read_check)`; both default to off when the host
    /// has not enabled LBP via MODE SELECT.
    ///
    /// Used by the iSCSI WRITE / READ handlers to gate CRC32C
    /// validation. `write_check = true` makes the handler validate the
    /// 4-byte CRC32C trailer the host appends when WRPROTECT > 0;
    /// `read_check = true` makes the handler append the freshly-
    /// computed trailer when RDPROTECT > 0.
    pub fn lbp_enables(&self, drive_id: usize) -> (bool, bool) {
        let Ok(drive) = self.drive_lock(drive_id) else {
            return (false, false);
        };
        crate::scsi::mode_pages::decode_lbp_enables(&drive.state.mode_pages)
    }

    /// Decode write-mode constraints from the drive's saved Mode Page
    /// 0x10/0x01 (Device Configuration Extension) body, if any:
    ///
    /// - **Append-Only** (LTO-7+) — body byte 0 high nibble (WRITE
    ///   MODE) == 1.
    /// - **Encrypt-Only** (LTO-8+) — body byte 2 bit 0 (WRE) set.
    ///
    /// Returns `WriteModeConstraints::default()` (both off) when the
    /// drive has no saved page or the body is too short.
    pub fn write_mode_constraints(&self, drive_id: usize) -> WriteModeConstraints {
        let Ok(drive) = self.drive_lock(drive_id) else {
            return WriteModeConstraints::default();
        };
        let Some(body) = drive.state.mode_pages.get(0x10, 0x01) else {
            return WriteModeConstraints::default();
        };
        let append_only = !body.is_empty() && ((body[0] >> 4) & 0x0F) == 1;
        let encrypt_only = body.len() >= 3 && (body[2] & 0x01) != 0;
        WriteModeConstraints {
            append_only,
            encrypt_only,
        }
    }

    /// Enforce drive write-mode constraints (Append-Only / Encrypt-
    /// Only) before a WRITE / WRITE FILEMARKS proceeds. Returns:
    ///
    /// - `Ok(())` when neither mode is active, or when active but
    ///   the cartridge's runtime state satisfies the constraint.
    /// - `Err(EncryptOnlyKeyAbsent)` when Encrypt-Only is active and
    ///   no drive encryption key is installed.
    /// - `Err(AppendOnlyMustExtendEod)` when Append-Only is active
    ///   and the head is not at the active partition's EOD.
    ///
    /// Both errors map to DATA PROTECT sense data at the iSCSI layer
    /// — the host sees a recoverable write-protect, not a hardware
    /// fault.
    pub fn enforce_write_mode(&self, drive_id: usize, tsih: u16) -> Result<(), SmcError> {
        let constraints = self.write_mode_constraints(drive_id);
        if !constraints.encrypt_only && !constraints.append_only {
            return Ok(());
        }
        self.with_drive(drive_id, tsih, |cart| {
            if constraints.encrypt_only && cart.encryption_state().is_none() {
                return Err(SmcError::EncryptOnlyKeyAbsent);
            }
            if constraints.append_only && cart.head_lba() != cart.next_lba() {
                return Err(SmcError::AppendOnlyMustExtendEod);
            }
            Ok(())
        })
    }

    /// Apply a MODE SELECT outcome's raw page bodies to this drive's
    /// volatile state. When `save_pages` is true the host requested
    /// SP=1 (persistent save) — the whole library drive-state envelope
    /// is rewritten atomically (`<data_dir>/library/drive_state.json`).
    ///
    /// Behavior-driving fields (page 0x0F DCE bit, page 0x11 partition
    /// layout) are still applied through their cartridge-side setters
    /// by the caller; this method only handles the round-trip storage
    /// that survives cartridge swaps.
    pub fn apply_mode_select_pages(
        &self,
        drive_id: usize,
        saved_pages: &[(u8, u8, Vec<u8>)],
        save_pages: bool,
    ) -> Result<(), SmcError> {
        if saved_pages.is_empty() {
            return Ok(());
        }
        {
            let mut drive = self.drive_lock(drive_id)?;
            for (page_code, subpage_code, body) in saved_pages {
                drive
                    .state
                    .mode_pages
                    .set(*page_code, *subpage_code, body.clone());
            }
        }
        if save_pages {
            self.persist_drive_state();
        }
        Ok(())
    }

    /// Snapshot every drive's runtime state into a single envelope —
    /// used by the persistence path. Each drive is locked only for
    /// the duration of its own clone; no two drive locks held at once.
    fn snapshot_library_drive_state(&self) -> LibraryDriveState {
        let mut env = LibraryDriveState::new();
        for (&id, drive_arc) in self.drives.iter() {
            if let Ok(drive) = drive_arc.lock()
                && !drive.state.is_empty()
            {
                env.drives.insert(id, drive.state.clone());
            }
        }
        env
    }

    /// Atomically persist the library-wide drive-state envelope to
    /// `<data_dir>/library/drive_state.json`. Best-effort: a failure
    /// here surfaces as a warning, not a SCSI CHECK CONDITION — the
    /// in-memory state is already updated and round-trip through MODE
    /// SENSE works for the rest of the daemon's lifetime even if
    /// persistence is broken (full disk, permissions, etc.). Real LTO
    /// drives also degrade gracefully when their NVRAM write fails.
    fn persist_drive_state(&self) {
        let Some(path) = self.state_file.as_ref() else {
            return;
        };
        let env = self.snapshot_library_drive_state();
        if let Err(e) = persist_library_drive_state(path, &env) {
            warn!(
                "drive_state.json persistence failed at {}: {} (in-memory state unaffected)",
                path.display(),
                e
            );
        }
    }

    pub fn with_drive<F, R>(&self, drive_id: usize, tsih: u16, f: F) -> Result<R, SmcError>
    where
        F: FnOnce(&mut Cartridge) -> Result<R, SmcError>,
    {
        let mut drive = self.drive_lock(drive_id)?;

        // Stamp the session lock fields under the same guard we'll
        // hold for the closure — equivalent to the old
        // lock_drive+re-lock dance, minus one re-acquire.
        if let Some(locked_tsih) = drive.locked_by_session
            && locked_tsih != tsih
        {
            warn!(
                "Drive {} locked by session TSIH={}, rejecting TSIH={}",
                drive_id, locked_tsih, tsih
            );
            return Err(SmcError::DriveReserved(drive_id));
        }
        drive.locked_by_session = Some(tsih);
        drive.lock_time = Some(Instant::now());

        let cartridge = drive
            .cartridge
            .as_mut()
            .ok_or(SmcError::NoCartridgeLoaded(drive_id))?;

        f(cartridge)
    }

    /// Load a cartridge into a drive
    ///
    /// This matches real tape library behavior: cartridges must be pre-created
    /// by an administrator (via CLI) before they can be loaded. SCSI commands
    /// cannot create cartridges automatically.
    pub fn load_cartridge(&self, drive_id: usize, cartridge_label: &str) -> Result<(), SmcError> {
        let mut drive = self.drive_lock(drive_id)?;

        // Appliance-side at-rest DEK lookup. Peek the manifest first
        // so we know whether the cartridge needs a key; if it does,
        // pull the pre-unwrapped DEK out of the daemon-managed
        // cache. Encrypted cartridges with no cached DEK refuse the
        // load — better than opening read-only or, worse, opening
        // for writes that would mix plaintext + ciphertext under
        // the same chunk pool.
        let (_uuid, manifest_encryption) =
            Cartridge::read_manifest_identity(&self.tapes_root, cartridge_label).map_err(|_| {
                warn!(
                    "Cartridge {} not found - must be created via CLI first",
                    cartridge_label
                );
                SmcError::CartridgeNotFound(cartridge_label.to_string())
            })?;
        let dek = if manifest_encryption.is_some() {
            let cached = self
                .dek_cache
                .lock()
                .ok()
                .and_then(|g| g.get(cartridge_label).copied());
            if cached.is_none() {
                warn!(
                    "Cartridge {} is at-rest encrypted but no DEK is cached - keystore unreachable at boot?",
                    cartridge_label
                );
                return Err(SmcError::DataDecryptionError(
                    "cartridge at-rest DEK unavailable (keystore unreachable at boot)",
                ));
            }
            cached
        } else {
            None
        };

        // Open existing cartridge only (no auto-creation). For
        // at-rest cartridges, inject the DEK so the chunking-seal
        // encrypt seam and read-side decrypt seam are active.
        let mut opts = core_mediachanger::CartridgeOpenOptions::new();
        if let Some(d) = dek {
            opts = opts.with_dek_for_open(d);
        }
        let mut cartridge = Cartridge::open_with(
            &self.tapes_root,
            cartridge_label,
            CartridgeOpenMode::Open,
            opts,
        )
        .map_err(|_| {
            warn!(
                "Cartridge {} not found - must be created via CLI first",
                cartridge_label
            );
            SmcError::CartridgeNotFound(cartridge_label.to_string())
        })?;

        // Drive-cartridge generation gate. The drive must be at least
        // as new as the cartridge: LTO-N drives read LTO-N and (where
        // applicable) LTO-(N-1) media. Refusing higher-gen cartridges
        // on lower-gen drives mirrors real LTO behavior.
        let drive_gen = self.library_lto_generation;
        let cart_gen = cartridge.lto_generation();
        if drive_gen > 0 && cart_gen > 0 && cart_gen > drive_gen {
            warn!(
                "load_cartridge refused: cartridge {} is LTO-{}; drive {} is LTO-{}",
                cartridge_label, cart_gen, drive_id, drive_gen
            );
            return Err(SmcError::IncompatibleMedium {
                drive_gen,
                cart_gen,
            });
        }

        // Real LTO drives reset compression state on cartridge load to
        // whatever the drive's configured default is (DCE clears on
        // power loss / cartridge eject). Mirror that — apply the daemon
        // config knob now; the host can flip it via MODE SELECT.
        // DCE starts off at every cartridge load (real LTO behavior).
        // The configured algorithm + zstd level are pre-loaded on the
        // state so when the host eventually flips DCE via MODE SELECT
        // page 0x0F, those settings are already in place.
        let initial = DriveCompressionState {
            dce: false,
            algorithm: self.drive_compression_algorithm,
            level: self.drive_compression_zstd_level,
        };
        cartridge.set_compression_state(initial);

        // Wire the cartridge into its bound backend's pool budget
        // (if the daemon configured one). Cartridges whose bound
        // backend isn't present in `pool_budgets` keep the
        // `unbounded` default — no gate, every seal succeeds. That
        // shouldn't happen at runtime (the daemon validates backend
        // names against `cloud.backends` at startup) but it makes
        // tests / partial setups behave sanely.
        if let Some(budget) = self.pool_budgets.get(cartridge.backend()) {
            cartridge.set_pool_budget(budget.clone(), self.backpressure_deadline);
        }
        if let Some(gl) = self.ghost_lists.get(cartridge.backend()) {
            cartridge.set_ghost_list(gl.clone());
        }

        drive.cartridge = Some(cartridge);
        info!(
            "Loaded cartridge {} into drive {} (DCE off; algorithm: {}, zstd_level: {} - host turns DCE on via MODE SELECT page 0x0F)",
            cartridge_label,
            drive_id,
            self.drive_compression_algorithm,
            self.drive_compression_zstd_level
        );
        Ok(())
    }

    /// Unload a cartridge from a drive
    pub fn unload_cartridge(&self, drive_id: usize) -> Result<String, SmcError> {
        let mut drive = self.drive_lock(drive_id)?;

        let cartridge = drive
            .cartridge
            .take()
            .ok_or(SmcError::NoCartridgeLoaded(drive_id))?;

        let label = cartridge.label().to_string();
        info!("Unloaded cartridge {} from drive {}", label, drive_id);
        Ok(label)
    }

    /// Check if a drive has a cartridge loaded
    pub fn has_cartridge(&self, drive_id: usize) -> Result<bool, SmcError> {
        let drive = self.drive_lock(drive_id)?;
        Ok(drive.cartridge.is_some())
    }

    /// Get the label of the loaded cartridge
    pub fn get_cartridge_label(&self, drive_id: usize) -> Result<Option<String>, SmcError> {
        let drive = self.drive_lock(drive_id)?;
        Ok(drive.cartridge.as_ref().map(|c| c.label().to_string()))
    }

    /// Whether the cartridge currently loaded in `drive_id` is WORM.
    /// Returns Ok(false) for "no cartridge loaded" — surfaces only the
    /// loaded-and-WORM case as `true` so VPD/Mode-Page handlers can
    /// short-circuit on a simple bool. `Err(InvalidDrive)` if the
    /// drive ID is out of range.
    pub fn is_loaded_cartridge_worm(&self, drive_id: usize) -> Result<bool, SmcError> {
        let drive = self.drive_lock(drive_id)?;
        Ok(drive.cartridge.as_ref().map(|c| c.worm()).unwrap_or(false))
    }

    /// `(barcode, backend_name)` for the cartridge currently loaded in
    /// `drive_id`. Used by the legal-hold load hook to look up the
    /// cartridge's bound backend without re-parsing manifest.json.
    /// Returns `None` if no cartridge is loaded.
    pub fn get_loaded_cartridge_info(
        &self,
        drive_id: usize,
    ) -> Result<Option<(String, String)>, SmcError> {
        let drive = self.drive_lock(drive_id)?;
        Ok(drive
            .cartridge
            .as_ref()
            .map(|c| (c.label().to_string(), c.backend().to_string())))
    }

    /// Stamp the volatile legal-hold flag on the cartridge loaded in
    /// `drive_id`. Called by the iSCSI MOVE MEDIUM post-hook after
    /// reading the cloud sentinel
    /// (`manifests/<barcode>/manifest-latest.json`). No-op (warns) if
    /// no cartridge is loaded — the post-hook only runs on a successful
    /// load so this should not happen in practice.
    pub fn set_legal_held(&self, drive_id: usize, held: bool) -> Result<(), SmcError> {
        let mut drive = self.drive_lock(drive_id)?;
        match drive.cartridge.as_mut() {
            Some(cart) => {
                cart.set_legal_held(held);
                if held {
                    info!(
                        "Drive {} legal-hold flag SET from cloud sentinel - host writes will return WRITE PROTECTED",
                        drive_id
                    );
                }
                Ok(())
            }
            None => Err(SmcError::NoCartridgeLoaded(drive_id)),
        }
    }

    /// Get (max_capacity_bytes, remaining_capacity_bytes) for the loaded cartridge.
    /// Returns None if the drive has no cartridge.
    /// Falls back to LTO-default capacity (decimal GB) if the cartridge has unlimited capacity.
    pub fn get_cartridge_capacity(&self, drive_id: usize) -> Option<(u64, u64)> {
        let drive = self
            .drives
            .get(&drive_id)?
            .lock()
            .expect("drive mutex poisoned");
        let cart = drive.cartridge.as_ref()?;
        let mut capacity_gb = cart.capacity_gb();
        if capacity_gb == 0 {
            capacity_gb = core_mediachanger::lto_default_capacity_gb(cart.lto_generation());
        }
        let max_bytes = capacity_gb.saturating_mul(1_000_000_000);
        let used_bytes = cart.used_capacity_bytes();
        let remaining = max_bytes.saturating_sub(used_bytes);
        Some((max_bytes, remaining))
    }

    /// Host-written MAM attributes for the cartridge loaded in
    /// `drive_id`, as `(id, format, value)` tuples in ascending-id
    /// order. `None` if the drive has no cartridge. Read-only access
    /// (no session lock) — mirrors [`Self::get_cartridge_capacity`];
    /// READ ATTRIBUTE is a non-mutating command.
    pub fn get_cartridge_mam_attributes(&self, drive_id: usize) -> Option<Vec<(u16, u8, Vec<u8>)>> {
        let drive = self
            .drives
            .get(&drive_id)?
            .lock()
            .expect("drive mutex poisoned");
        let cart = drive.cartridge.as_ref()?;
        Some(cart.mam_attributes())
    }

    /// Persist one host WRITE ATTRIBUTE record onto the cartridge
    /// loaded in `drive_id`. Routed through [`Self::with_drive`] so the
    /// per-session TSIH lock applies and `NoCartridgeLoaded` surfaces
    /// when no medium is present. The caller (the SCSI layer) must
    /// already have rejected device/medium read-only ids; an empty
    /// `value` deletes the id.
    pub fn write_cartridge_mam_attribute(
        &self,
        drive_id: usize,
        tsih: u16,
        id: u16,
        format: u8,
        value: Vec<u8>,
    ) -> Result<(), SmcError> {
        self.with_drive(drive_id, tsih, |cart| {
            cart.write_mam_attribute(id, format, value)
        })
    }

    /// Clean up stale locks (older than timeout_seconds). Locks each
    /// drive individually — never holds two drive locks at once.
    pub fn cleanup_stale_locks(&self, timeout_seconds: u64) {
        let now = Instant::now();

        for mtx in self.drives.values() {
            let mut drive = mtx.lock().expect("drive mutex poisoned");
            if let Some(lock_time) = drive.lock_time {
                let age = now.duration_since(lock_time).as_secs();
                if age > timeout_seconds {
                    warn!(
                        "Releasing stale lock on drive {} (held for {}s by TSIH={:?})",
                        drive.id, age, drive.locked_by_session
                    );
                    drive.locked_by_session = None;
                    drive.lock_time = None;
                }
            }
        }
    }

    /// Update the PREVENT/ALLOW MEDIUM REMOVAL state for a drive on
    /// behalf of the given session. `bits` carries cdb[4] bits 1-0;
    /// passing `PreventBits::default()` (both zero) removes the
    /// session's entry. Multiple initiators can each assert prevent
    /// independently — the drive is considered "prevented" while any
    /// entry has the relevant bit set.
    pub fn set_prevent(
        &self,
        drive_id: usize,
        tsih: u16,
        bits: PreventBits,
    ) -> Result<(), SmcError> {
        let mut drive = self.drive_lock(drive_id)?;
        if bits.is_zero() {
            drive.prevent.remove(&tsih);
        } else {
            drive.prevent.insert(tsih, bits);
        }
        Ok(())
    }

    /// True if any active session has asserted bit 0 (data-transport
    /// removal prevent) for this drive. Gates SCSI UNLOAD and MOVE
    /// MEDIUM-from-drive. Returns false on invalid drive id (the
    /// caller short-circuits on a sane lookup elsewhere).
    pub fn is_data_transport_prevented(&self, drive_id: usize) -> bool {
        let Ok(drive) = self.drive_lock(drive_id) else {
            return false;
        };
        drive.prevent.values().any(|b| b.data_transport)
    }

    /// True if any active session has asserted bit 1 (mechanical
    /// eject prevent) for this drive. Gates the admin
    /// `POST /api/v1/changer/unload` endpoint — the operator-console
    /// analog of pressing the front-panel eject button on a real LTO
    /// chassis. Bit 0 (`is_data_transport_prevented`) gates SCSI
    /// UNLOAD and SCSI MOVE MEDIUM on the host side. Returns false on
    /// invalid drive id.
    pub fn is_mechanical_prevented(&self, drive_id: usize) -> bool {
        let Ok(drive) = self.drive_lock(drive_id) else {
            return false;
        };
        drive.prevent.values().any(|b| b.mechanical)
    }

    /// Drop every prevent entry tagged with `tsih`. Called from the
    /// connection-close path so I_T-nexus state dies with its
    /// session, matching SPC-4 semantics.
    pub fn clear_prevent_for_session(&self, tsih: u16) {
        for mtx in self.drives.values() {
            let mut drive = mtx.lock().expect("drive mutex poisoned");
            drive.prevent.remove(&tsih);
        }
    }

    /// Release all locks held by a specific session (called when session closes)
    pub fn release_session_locks(&self, tsih: u16) {
        let mut count = 0;

        for mtx in self.drives.values() {
            let mut drive = mtx.lock().expect("drive mutex poisoned");
            if drive.locked_by_session == Some(tsih) {
                tracing::info!(
                    "Releasing lock on drive {} (held by closing session TSIH={})",
                    drive.id,
                    tsih
                );
                drive.locked_by_session = None;
                drive.lock_time = None;
                count += 1;
            }
        }

        if count > 0 {
            tracing::info!("Released {} drive locks for session TSIH={}", count, tsih);
        }
    }

    /// Get drive info for monitoring
    pub fn get_drive_info(&self) -> Vec<DriveInfo> {
        self.drives
            .values()
            .map(|mtx| {
                let d = mtx.lock().expect("drive mutex poisoned");
                DriveInfo {
                    id: d.id,
                    barcode: d.cartridge.as_ref().map(|c| c.label().to_string()),
                    locked_by_session: d.locked_by_session,
                    lock_age_seconds: d.lock_time.map(|t| t.elapsed().as_secs()).unwrap_or(0),
                }
            })
            .collect()
    }

    /// Get number of drives
    pub fn drive_count(&self) -> usize {
        self.drives.len()
    }

    /// Get drive status for monitoring
    pub fn get_drive_status(&self) -> Vec<DriveInfo> {
        let mut out: Vec<DriveInfo> = self
            .drives
            .values()
            .map(|mtx| {
                let d = mtx.lock().expect("drive mutex poisoned");
                DriveInfo {
                    id: d.id,
                    barcode: d.cartridge.as_ref().map(|c| c.label().to_string()),
                    locked_by_session: d.locked_by_session,
                    lock_age_seconds: d.lock_time.map(|t| t.elapsed().as_secs()).unwrap_or(0),
                }
            })
            .collect();
        out.sort_by_key(|d| d.id);
        out
    }
}

/// Drive info for monitoring/debugging
#[derive(Debug, Clone, serde::Serialize)]
pub struct DriveInfo {
    pub id: usize,
    pub barcode: Option<String>,
    pub locked_by_session: Option<u16>,
    pub lock_age_seconds: u64,
}

/// Drive-side write-mode flags decoded from saved Mode Page 0x10/0x01
/// (Device Configuration Extension). Both default to `false` when the
/// host has never written that page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteModeConstraints {
    /// LTO-7+ Append-Only: WRITE / WRITE FILEMARKS must extend EOD.
    pub append_only: bool,
    /// LTO-8+ Encrypt-Only: every WRITE / WRITE FILEMARKS requires an
    /// active drive encryption key (set via SECURITY PROTOCOL OUT
    /// 0x20 / SPSP 0x0010).
    pub encrypt_only: bool,
}

/// Derive `<data_dir>/library/drive_state.json` from `tapes_root`
/// (`<data_dir>/tapes`). Returns `None` for unusual paths without a
/// parent (deeply nested tmpdirs in tests sometimes lack one) — the
/// `DriveManager` then runs without persistence.
fn derive_drive_state_path(tapes_root: &Path) -> Option<PathBuf> {
    tapes_root
        .parent()
        .map(|data_dir| data_dir.join("library").join("drive_state.json"))
}

/// Read the library-wide drive-state envelope. Missing file or parse
/// error yield an empty envelope — that matches the "host has never
/// issued SP=1" case and every drive starts with default state.
fn load_library_drive_state(path: &PathBuf) -> LibraryDriveState {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(
                "drive_state.json at {} unreadable ({}); starting with default per-drive state",
                path.display(),
                e
            );
            LibraryDriveState::default()
        }),
        Err(_) => LibraryDriveState::default(),
    }
}

/// Atomically write the library-wide drive-state envelope. Tmp-then-
/// rename so a crash mid-write can't leave a half-flushed file.
/// Creates the parent `library/` directory if missing.
fn persist_library_drive_state(path: &Path, env: &LibraryDriveState) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_drive_manager() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());
        assert_eq!(mgr.drive_count(), 2);
    }

    /// A higher-LTO cartridge in a lower-LTO drive must be refused at
    /// load_cartridge time with `IncompatibleMedium`.
    #[test]
    fn test_load_cartridge_gates_on_generation() {
        let temp_dir = TempDir::new().unwrap();
        let tapes_root = temp_dir.path().join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();

        let mut mgr = DriveManager::new(2, tapes_root.clone());
        // Pretend this is a LTO-7 chassis.
        mgr.set_library_lto_generation(7);

        // Pre-create an LTO-7 and an LTO-8 cartridge. Both live on
        // disk; the gate fires only at load time.
        for (label, lto_gen) in [("STD7L7", 7u8), ("STD8L8", 8u8)] {
            Cartridge::create_with_chunking(
                &tapes_root,
                label,
                core_mediachanger::ChunkingMode::fastcdc_default(),
                lto_gen,
                "primary",
                false,
                core_mediachanger::DedupScope::Local,
            )
            .unwrap();
        }

        // LTO-7 → loads cleanly on the LTO-7 drive.
        mgr.load_cartridge(0, "STD7L7").unwrap();
        mgr.unload_cartridge(0).unwrap();

        // LTO-8 on an LTO-7 drive → IncompatibleMedium.
        let err = mgr.load_cartridge(0, "STD8L8").unwrap_err();
        assert!(matches!(
            err,
            SmcError::IncompatibleMedium {
                drive_gen: 7,
                cart_gen: 8,
            }
        ));

        // Same LTO-8 cartridge loads fine on an LTO-8 chassis.
        let mut mgr8 = DriveManager::new(1, tapes_root.clone());
        mgr8.set_library_lto_generation(8);
        mgr8.load_cartridge(0, "STD8L8").unwrap();
    }

    #[test]
    fn test_lock_unlock_drive() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());

        // Lock drive 0 with session 1
        assert!(mgr.lock_drive(0, 1).is_ok());

        // Try to lock with different session - should fail
        assert!(mgr.lock_drive(0, 2).is_err());

        // Same session can re-lock
        assert!(mgr.lock_drive(0, 1).is_ok());

        // Unlock
        assert!(mgr.unlock_drive(0, 1).is_ok());

        // Now different session can lock
        assert!(mgr.lock_drive(0, 2).is_ok());
    }

    #[test]
    fn test_load_unload_cartridge() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());

        // Initially no cartridge
        assert!(!mgr.has_cartridge(0).unwrap());

        // Pre-create the cartridge — load_cartridge requires it to exist on disk
        // (matches real hardware: admin pre-creates via CLI before SCSI can load it)
        Cartridge::open(
            temp_dir.path(),
            "TEST001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();

        // Load cartridge
        mgr.load_cartridge(0, "TEST001").unwrap();
        assert!(mgr.has_cartridge(0).unwrap());
        assert_eq!(
            mgr.get_cartridge_label(0).unwrap(),
            Some("TEST001".to_string())
        );

        // Unload cartridge
        let label = mgr.unload_cartridge(0).unwrap();
        assert_eq!(label, "TEST001");
        assert!(!mgr.has_cartridge(0).unwrap());
    }

    #[test]
    fn test_invalid_drive() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());

        assert!(mgr.lock_drive(99, 1).is_err());
        assert!(mgr.has_cartridge(99).is_err());
    }

    #[test]
    fn test_drive_state_persists_across_manager_recreate() {
        // Per-drive mode-page state should survive a daemon restart —
        // that's the real-LTO drive-NVRAM model. Two DriveManagers on
        // the same data_dir: the second one rebuilds drive 0's saved
        // pages from disk.
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let tapes_root = data_dir.join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();

        // First daemon lifetime: host issues MODE SELECT page 0x1C
        // SP=1 on drive 0.
        {
            let mgr = DriveManager::new(2, tapes_root.clone());
            let body = vec![0x80, 0x04, 0, 0, 0x12, 0x34, 0, 0, 0, 0x05];
            mgr.apply_mode_select_pages(0, &[(0x1C, 0x00, body.clone())], true)
                .unwrap();
            // Drive 1 unaffected.
            assert!(mgr.mode_pages_state(1).unwrap().is_empty());
        }

        // Second daemon lifetime: state must come back from disk.
        {
            let mgr = DriveManager::new(2, tapes_root);
            let recovered = mgr.mode_pages_state(0).unwrap();
            assert_eq!(
                recovered.get(0x1C, 0x00).unwrap(),
                &[0x80, 0x04, 0, 0, 0x12, 0x34, 0, 0, 0, 0x05]
            );
            assert!(mgr.mode_pages_state(1).unwrap().is_empty());
        }
    }

    #[test]
    fn test_write_mode_constraints_default() {
        // No saved page → both flags off.
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().join("tapes"));
        let c = mgr.write_mode_constraints(0);
        assert!(!c.append_only);
        assert!(!c.encrypt_only);
    }

    #[test]
    fn test_write_mode_constraints_decode_append_only() {
        let temp_dir = TempDir::new().unwrap();
        let tapes_root = temp_dir.path().join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();
        let mgr = DriveManager::new(1, tapes_root);
        // Body byte 0 high nibble = 1 → WRITE MODE = 1 (append-only).
        let mut body = vec![0u8; 28];
        body[0] = 0x10;
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();
        let c = mgr.write_mode_constraints(0);
        assert!(c.append_only);
        assert!(!c.encrypt_only);
    }

    #[test]
    fn test_write_mode_constraints_decode_encrypt_only() {
        let temp_dir = TempDir::new().unwrap();
        let tapes_root = temp_dir.path().join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();
        let mgr = DriveManager::new(1, tapes_root);
        // Body byte 2 bit 0 = 1 → WRE (encrypt-only).
        let mut body = vec![0u8; 28];
        body[2] = 0x01;
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();
        let c = mgr.write_mode_constraints(0);
        assert!(!c.append_only);
        assert!(c.encrypt_only);
    }

    #[test]
    fn test_write_mode_constraints_both_set() {
        let temp_dir = TempDir::new().unwrap();
        let tapes_root = temp_dir.path().join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();
        let mgr = DriveManager::new(1, tapes_root);
        let mut body = vec![0u8; 28];
        body[0] = 0x10; // WRITE MODE = 1
        body[2] = 0x01; // WRE
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();
        let c = mgr.write_mode_constraints(0);
        assert!(c.append_only);
        assert!(c.encrypt_only);
    }

    #[test]
    fn test_enforce_write_mode_noop_when_neither_active() {
        // Both flags off → enforce returns Ok without even touching
        // the cartridge (drives without a loaded tape don't fail).
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        assert!(mgr.enforce_write_mode(0, 1).is_ok());
    }

    #[test]
    fn test_enforce_write_mode_encrypt_only_blocks_without_key() {
        // Encrypt-Only set, cartridge loaded, no key installed →
        // EncryptOnlyKeyAbsent.
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        Cartridge::open(
            temp_dir.path(),
            "ENC001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();
        mgr.load_cartridge(0, "ENC001").unwrap();

        let mut body = vec![0u8; 28];
        body[2] = 0x01; // WRE
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();

        let err = mgr.enforce_write_mode(0, 1).unwrap_err();
        assert!(
            matches!(err, SmcError::EncryptOnlyKeyAbsent),
            "expected EncryptOnlyKeyAbsent, got {:?}",
            err
        );
    }

    #[test]
    fn test_enforce_write_mode_append_only_passes_at_eod() {
        // Append-Only set, fresh cartridge (head == next_lba == 0) →
        // enforce passes (we *are* at EOD on an empty tape).
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        Cartridge::open(
            temp_dir.path(),
            "APP001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();
        mgr.load_cartridge(0, "APP001").unwrap();

        let mut body = vec![0u8; 28];
        body[0] = 0x10; // WRITE MODE = 1
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();

        assert!(mgr.enforce_write_mode(0, 1).is_ok());
    }

    #[test]
    fn test_enforce_write_mode_append_only_blocks_mid_tape() {
        // Append-Only set, write some data, locate back to BOM →
        // head != next_lba → AppendOnlyMustExtendEod.
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        Cartridge::open(
            temp_dir.path(),
            "APP002",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();
        mgr.load_cartridge(0, "APP002").unwrap();

        // Write a block, then rewind to BOM so head < next_lba.
        mgr.with_drive(0, 1, |cart| {
            cart.write_data(bytes::Bytes::from_static(b"hello"))?;
            cart.locate(0)?;
            Ok(())
        })
        .unwrap();

        let mut body = vec![0u8; 28];
        body[0] = 0x10; // WRITE MODE = 1
        mgr.apply_mode_select_pages(0, &[(0x10, 0x01, body)], false)
            .unwrap();

        let err = mgr.enforce_write_mode(0, 1).unwrap_err();
        assert!(
            matches!(err, SmcError::AppendOnlyMustExtendEod),
            "expected AppendOnlyMustExtendEod, got {:?}",
            err
        );
    }

    #[test]
    fn test_prevent_default_clear() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());
        assert!(!mgr.is_data_transport_prevented(0));
        assert!(!mgr.is_mechanical_prevented(0));
    }

    #[test]
    fn test_prevent_data_transport_bit() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(2, temp_dir.path().to_path_buf());
        mgr.set_prevent(
            0,
            42,
            PreventBits {
                data_transport: true,
                mechanical: false,
            },
        )
        .unwrap();
        assert!(mgr.is_data_transport_prevented(0));
        assert!(!mgr.is_mechanical_prevented(0));
        // Other drive untouched.
        assert!(!mgr.is_data_transport_prevented(1));
    }

    #[test]
    fn test_prevent_mechanical_bit() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        mgr.set_prevent(
            0,
            7,
            PreventBits {
                data_transport: false,
                mechanical: true,
            },
        )
        .unwrap();
        assert!(!mgr.is_data_transport_prevented(0));
        assert!(mgr.is_mechanical_prevented(0));
    }

    #[test]
    fn test_prevent_zero_bits_clears_entry() {
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        mgr.set_prevent(
            0,
            7,
            PreventBits {
                data_transport: true,
                mechanical: true,
            },
        )
        .unwrap();
        assert!(mgr.is_data_transport_prevented(0));
        // ALLOW (both bits zero) erases the entry.
        mgr.set_prevent(0, 7, PreventBits::default()).unwrap();
        assert!(!mgr.is_data_transport_prevented(0));
        assert!(!mgr.is_mechanical_prevented(0));
    }

    #[test]
    fn test_prevent_multi_initiator_any_holds() {
        // SPC-4: if any initiator has prevent set, the drive is
        // prevented. ALLOW from one initiator must not unlock another's
        // prevent.
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(1, temp_dir.path().to_path_buf());
        mgr.set_prevent(
            0,
            10,
            PreventBits {
                data_transport: true,
                mechanical: false,
            },
        )
        .unwrap();
        mgr.set_prevent(
            0,
            20,
            PreventBits {
                data_transport: true,
                mechanical: false,
            },
        )
        .unwrap();
        assert!(mgr.is_data_transport_prevented(0));
        // TSIH 10 issues ALLOW.
        mgr.set_prevent(0, 10, PreventBits::default()).unwrap();
        // TSIH 20 still has prevent set → drive remains prevented.
        assert!(mgr.is_data_transport_prevented(0));
        // TSIH 20 ALLOWs → fully clear.
        mgr.set_prevent(0, 20, PreventBits::default()).unwrap();
        assert!(!mgr.is_data_transport_prevented(0));
    }

    #[test]
    fn test_prevent_session_end_clears_all_drives() {
        // clear_prevent_for_session must drop the session's entry on
        // every drive — matches SessionGuard::drop semantics.
        let temp_dir = TempDir::new().unwrap();
        let mgr = DriveManager::new(3, temp_dir.path().to_path_buf());
        let bits = PreventBits {
            data_transport: true,
            mechanical: false,
        };
        for d in 0..3 {
            mgr.set_prevent(d, 99, bits).unwrap();
        }
        // Independent session keeps its hold on drive 1.
        mgr.set_prevent(1, 100, bits).unwrap();

        mgr.clear_prevent_for_session(99);
        assert!(!mgr.is_data_transport_prevented(0));
        assert!(mgr.is_data_transport_prevented(1)); // TSIH 100 still holds
        assert!(!mgr.is_data_transport_prevented(2));
    }

    #[test]
    fn test_drive_state_volatile_without_sp_bit() {
        // SP=0 on MODE SELECT means "set, but don't persist." State
        // must not survive a manager recreate.
        let temp_dir = TempDir::new().unwrap();
        let tapes_root = temp_dir.path().join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();

        {
            let mgr = DriveManager::new(1, tapes_root.clone());
            let body = vec![0x00, 0x06, 0, 0, 0, 0, 0, 0, 0, 0];
            mgr.apply_mode_select_pages(0, &[(0x1C, 0x00, body)], false)
                .unwrap();
            // Visible within the same lifetime.
            assert!(!mgr.mode_pages_state(0).unwrap().is_empty());
        }

        let mgr = DriveManager::new(1, tapes_root);
        assert!(
            mgr.mode_pages_state(0).unwrap().is_empty(),
            "SP=0 mutations must not persist"
        );
    }
}
