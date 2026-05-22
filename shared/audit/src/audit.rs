// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Audit log: append-only event journal for the daemon and CLI.
//!
//! Always tamper-evident: every entry carries a BLAKE3 chain —
//! `entry_hash = blake3(canonical JSON minus entry_hash)`, next entry's
//! `prev_hash` equals it. A `chain.state` file at the dir root caches
//! `(last_seq, last_hash, last_file)` for O(1) startup verification.
//! The chain spans day rollovers. The `AuditMode` enum is kept as a
//! single-variant placeholder so adding e.g. signed-on-finalize later
//! is purely additive.
//!
//! On rollover (UTC midnight), the previous day's file is closed; if
//! `compress_rotated` is true (default) it's then rewritten as
//! `audit-YYYY-MM-DD.jsonl.zst` (zstd level 3) and the original removed.
//! `verify` and `export` transparently read either form.
//!
//! Single-writer model: the daemon is the only process that mutates
//! `audit-*.jsonl` and `chain.state` once it's running. The CLI
//! commands that run daemon-down (`library init`, `library modify`)
//! drop their would-be entries into `<audit_dir>/pending/<sortable>.json`
//! via [`queue_pending`]; the daemon picks them up on startup via
//! [`AuditLog::replay_pending`] and appends them to the chain in
//! filename order. With one in-process writer, in-process safety is a
//! single `Mutex<AuditWriter>`; no cross-process file lock is needed.
//!
//! No tokio dependency — pure synchronous file I/O. Callable from sync
//! and async contexts (the daemon's audit calls happen on the
//! command-dispatch path, not the high-throughput data path).

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sentinel `prev_hash` for the very first entry in a fresh chain.
pub const GENESIS_PREV_HASH: &str =
    "blake3:0000000000000000000000000000000000000000000000000000000000000000";

/// Pre-replay queue for daemon-down audit writes (CLI `library init`
/// / `library modify`). Each `<sortable-timestamp>-<random>.json`
/// file holds one [`PendingAuditEntry`].
pub const PENDING_AUDIT_DIR: &str = "pending";
/// Subdirectory the daemon moves a pending entry to when chain
/// append fails (chain broken etc.); kept for forensics.
pub const PENDING_AUDIT_FAILED_DIR: &str = "failed";

pub const CHAIN_STATE_FILE: &str = "chain.state";

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("audit chain broken at seq {seq}: stored={stored}, recomputed={actual}")]
    ChainBroken {
        seq: u64,
        stored: String,
        actual: String,
    },

    #[error("audit chain state corrupt: {0}")]
    StateCorrupt(String),

    #[error("zstd error: {0}")]
    Zstd(String),

    #[error("audit writer mutex poisoned")]
    MutexPoisoned,
}

pub type Result<T> = std::result::Result<T, AuditError>;

/// Where the event came from. `kind` is one of `"cli"`, `"daemon"`,
/// `"rest"`, `"system"`. `user` and `addr` are optional context (CLI
/// shell user, REST caller IP).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditActor {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

impl AuditActor {
    pub fn system() -> Self {
        Self {
            kind: "system".to_string(),
            user: None,
            addr: None,
        }
    }

    pub fn daemon() -> Self {
        Self {
            kind: "daemon".to_string(),
            user: None,
            addr: None,
        }
    }

    pub fn cli(user: impl Into<String>) -> Self {
        Self {
            kind: "cli".to_string(),
            user: Some(user.into()),
            addr: None,
        }
    }

    pub fn rest(user: impl Into<String>, addr: impl Into<String>) -> Self {
        Self {
            kind: "rest".to_string(),
            user: Some(user.into()),
            addr: Some(addr.into()),
        }
    }

    /// SCSI/iSCSI command audit actor. `user` carries the initiator IQN
    /// (or CHAP username if available); `addr` carries the peer's
    /// `ip:port`. Use for events that originate over the wire from a
    /// SCSI initiator — drive load/unload via SCSI MOVE MEDIUM, AME key
    /// set/clear via SECURITY PROTOCOL OUT, MODE SELECT page 0x0F
    /// compression toggles, and CHAP login success/failure.
    pub fn iscsi(initiator: Option<impl Into<String>>, addr: impl Into<String>) -> Self {
        Self {
            kind: "iscsi".to_string(),
            user: initiator.map(Into::into),
            addr: Some(addr.into()),
        }
    }
}

/// One audit entry as it appears on disk (one entry per JSONL line).
///
/// Hashing covers all fields except `entry_hash` — `compute_entry_hash`
/// projects to a fixed-order subset that excludes it. `prev_hash` and
/// `entry_hash` are populated for every entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub actor: AuditActor,
    pub op: String,
    pub params: serde_json::Value,
    pub result: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub prev_hash: Option<String>,
    #[serde(default)]
    pub entry_hash: Option<String>,
}

/// Audit operating mode. Single-variant placeholder so the on-disk
/// schema can grow (e.g. signed-on-finalize) without churning the
/// public API. Always `TamperEvident` for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuditMode {
    #[default]
    TamperEvident,
}

/// On-disk chain anchor. Rewritten under the in-process writer lock
/// after every tamper-evident append. Lets the daemon verify the tail
/// of the most recent file in O(1) at startup instead of replaying
/// the whole chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChainState {
    last_seq: u64,
    last_hash: String,
    /// Basename only (e.g. `audit-2026-05-03.jsonl`) so the dir is
    /// relocatable.
    last_file: String,
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub dir: PathBuf,
    pub mode: AuditMode,
    pub compress_rotated: bool,
}

impl AuditConfig {
    pub fn new(dir: impl Into<PathBuf>, mode: AuditMode) -> Self {
        Self {
            dir: dir.into(),
            mode,
            compress_rotated: true,
        }
    }
}

pub struct AuditLog {
    config: AuditConfig,
    state: Mutex<AuditWriter>,
}

#[derive(Debug)]
struct AuditWriter {
    current_file: PathBuf,
    current_date: NaiveDate,
    last_seq: u64,
    /// `Some` only in tamper-evident mode.
    last_hash: Option<String>,
}

impl AuditLog {
    /// Open or create the audit log at `config.dir`. In tamper-evident
    /// mode, validates the tail of the last file against `chain.state`
    /// and refuses (returns `ChainBroken`) on mismatch — the daemon
    /// surfaces this to operator and exits.
    pub fn open(config: AuditConfig) -> Result<Self> {
        fs::create_dir_all(&config.dir)?;
        let now = Utc::now();
        let today = now.date_naive();

        let writer = Self::open_tamper_evident(&config.dir, today)?;

        Ok(Self {
            config,
            state: Mutex::new(writer),
        })
    }

    /// Open the log for operator-acknowledged recovery: reads
    /// `chain.state` (or the supplied alternative path) without
    /// running `verify_tail`, so a tampered tail does not block the
    /// open. Used exclusively by `audit rotate --accept-break` to
    /// preserve the broken last_hash for the chain_reset sentinel.
    /// TamperEvident mode only.
    pub fn open_for_recovery(
        config: AuditConfig,
        chain_state_override: Option<&Path>,
    ) -> Result<Self> {
        fs::create_dir_all(&config.dir)?;
        let today = Utc::now().date_naive();
        let cs_path = chain_state_override
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config.dir.join(CHAIN_STATE_FILE));
        let writer = if cs_path.exists() {
            let raw = fs::read(&cs_path)?;
            let state: ChainState = serde_json::from_slice(&raw)
                .map_err(|e| AuditError::StateCorrupt(format!("chain.state: {e}")))?;
            let current_file = resolve_audit_file(&config.dir, &state.last_file);
            let current_date = parse_date_from_filename(&state.last_file).ok_or_else(|| {
                AuditError::StateCorrupt(format!("cannot parse date from {}", state.last_file))
            })?;
            AuditWriter {
                current_file,
                current_date,
                last_seq: state.last_seq,
                last_hash: Some(state.last_hash),
            }
        } else {
            // No chain.state at all — treat as fresh genesis.
            let basename = filename_for(today);
            AuditWriter {
                current_file: config.dir.join(&basename),
                current_date: today,
                last_seq: 0,
                last_hash: Some(GENESIS_PREV_HASH.to_string()),
            }
        };
        Ok(Self {
            config,
            state: Mutex::new(writer),
        })
    }

    fn open_tamper_evident(dir: &Path, today: NaiveDate) -> Result<AuditWriter> {
        let chain_state_path = dir.join(CHAIN_STATE_FILE);
        if chain_state_path.exists() {
            let raw = fs::read(&chain_state_path)?;
            let state: ChainState = serde_json::from_slice(&raw)
                .map_err(|e| AuditError::StateCorrupt(format!("chain.state: {e}")))?;
            let current_file = resolve_audit_file(dir, &state.last_file);
            verify_tail(&current_file, &state)?;
            let current_date = parse_date_from_filename(&state.last_file).ok_or_else(|| {
                AuditError::StateCorrupt(format!("cannot parse date from {}", state.last_file))
            })?;
            // If we're past midnight relative to the last-written file,
            // that's fine — the next append will trigger rollover. We
            // don't pre-rotate here because pre-rotation with no entry
            // would create empty files.
            Ok(AuditWriter {
                current_file,
                current_date,
                last_seq: state.last_seq,
                last_hash: Some(state.last_hash),
            })
        } else {
            // Fresh chain: pick today as genesis date. last_hash starts
            // at the genesis sentinel so the first entry's prev_hash
            // matches it.
            let basename = filename_for(today);
            let current_file = dir.join(&basename);
            Ok(AuditWriter {
                current_file,
                current_date: today,
                last_seq: 0,
                last_hash: Some(GENESIS_PREV_HASH.to_string()),
            })
        }
    }

    pub const fn mode(&self) -> AuditMode {
        self.config.mode
    }

    pub fn dir(&self) -> &Path {
        &self.config.dir
    }

    /// Append an entry. Returns the assigned sequence number.
    ///
    /// Holds the in-process `Mutex<AuditWriter>` for the duration.
    /// Single-writer model — no cross-process locking; the daemon is
    /// the only writer once started, and CLI daemon-down audit
    /// entries route through `<dir>/pending/` for replay at next
    /// startup.
    pub fn append(
        &self,
        op: &str,
        actor: AuditActor,
        params: serde_json::Value,
        result: AuditResult,
    ) -> Result<u64> {
        let mut w = self.state.lock().map_err(|_| AuditError::MutexPoisoned)?;

        let now = Utc::now();
        let today = now.date_naive();
        if today != w.current_date {
            self.rollover(&mut w, today)?;
        }

        let seq = w.last_seq + 1;
        let (result_str, error) = match result {
            AuditResult::Ok => ("ok".to_string(), None),
            AuditResult::Error(msg) => ("error".to_string(), Some(msg)),
        };

        let mut entry = AuditEntry {
            seq,
            ts: now,
            actor,
            op: op.to_string(),
            params,
            result: result_str,
            error,
            prev_hash: None,
            entry_hash: None,
        };

        let prev = w
            .last_hash
            .clone()
            .unwrap_or_else(|| GENESIS_PREV_HASH.to_string());
        entry.prev_hash = Some(prev);
        let h = compute_entry_hash(&entry);
        entry.entry_hash = Some(h.clone());
        self.write_line(&w.current_file, &entry)?;
        w.last_seq = seq;
        w.last_hash = Some(h);
        self.write_chain_state(&w)?;

        shared_telemetry::record::audit_entry(op);
        Ok(seq)
    }

    /// Operator-acknowledged chain reset after a detected break.
    /// Writes a `audit.chain_reset` entry whose `prev_hash` is the
    /// sentinel `blake3:reset:<old_last_hash_hex>` so the discontinuity
    /// stays visible in the chain forever. Subsequent entries chain off
    /// this reset entry's `entry_hash` normally. `params.trigger =
    /// "break_recovery"`.
    pub fn rotate_after_break(&self, actor: AuditActor) -> Result<u64> {
        let mut w = self.state.lock().map_err(|_| AuditError::MutexPoisoned)?;
        self.write_reset_entry(&mut w, actor, "break_recovery", serde_json::Map::new())
    }

    /// Inner helper behind [`rotate_after_break`]. Writes a
    /// `audit.chain_reset` entry with `params: {old_last_hash, trigger,
    /// ...extra}` and the `blake3:reset:<old>` sentinel prev_hash.
    /// `extra` is merged into the params object.
    fn write_reset_entry(
        &self,
        w: &mut AuditWriter,
        actor: AuditActor,
        trigger: &str,
        extra: serde_json::Map<String, serde_json::Value>,
    ) -> Result<u64> {
        let old_hash = w
            .last_hash
            .clone()
            .unwrap_or_else(|| GENESIS_PREV_HASH.to_string());
        let sentinel = format!(
            "blake3:reset:{}",
            old_hash.strip_prefix("blake3:").unwrap_or(&old_hash)
        );

        let now = Utc::now();
        let today = now.date_naive();
        if today != w.current_date {
            self.rollover(w, today)?;
        }

        let mut params = serde_json::Map::new();
        params.insert(
            "old_last_hash".to_string(),
            serde_json::Value::String(old_hash),
        );
        params.insert(
            "trigger".to_string(),
            serde_json::Value::String(trigger.to_string()),
        );
        for (k, v) in extra {
            params.insert(k, v);
        }

        let seq = w.last_seq + 1;
        let mut entry = AuditEntry {
            seq,
            ts: now,
            actor,
            op: "audit.chain_reset".to_string(),
            params: serde_json::Value::Object(params),
            result: "ok".to_string(),
            error: None,
            prev_hash: Some(sentinel),
            entry_hash: None,
        };
        let h = compute_entry_hash(&entry);
        entry.entry_hash = Some(h.clone());
        self.write_line(&w.current_file, &entry)?;
        w.last_seq = seq;
        w.last_hash = Some(h);
        self.write_chain_state(w)?;
        shared_telemetry::record::audit_entry("audit.chain_reset");
        shared_telemetry::record::audit_chain_reset();
        Ok(seq)
    }

    /// Walk the entire chain from genesis to tail, recomputing every
    /// `entry_hash` and verifying `prev_hash` linkage.
    pub fn verify(&self) -> Result<VerifyReport> {
        verify_chain(&self.config.dir)
    }

    /// Drain `<dir>/pending/*.json` into the live chain in filename
    /// order. Each successful append removes the source file; a
    /// failing append moves the file to `<dir>/pending/failed/` for
    /// post-hoc inspection so a single corrupt entry can't wedge the
    /// daemon's startup. Returns `(replayed, failed)` counts.
    ///
    /// Filenames sort lexically by the leading RFC-3339 timestamp the
    /// CLI helper writes ([`queue_pending`]); same-millisecond ties
    /// break on the random suffix. Order across daemon-down sessions
    /// is preserved as long as the operator's clock doesn't roll
    /// backwards (good-faith assumption — the daemon is the only
    /// thing that needs strict monotonicity, and that's enforced via
    /// the `seq` counter on append).
    pub fn replay_pending(&self) -> Result<(usize, usize)> {
        let pending_dir = self.config.dir.join(PENDING_AUDIT_DIR);
        if !pending_dir.is_dir() {
            return Ok((0, 0));
        }
        let mut entries: Vec<PathBuf> = fs::read_dir(&pending_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        entries.sort();

        let failed_dir = pending_dir.join(PENDING_AUDIT_FAILED_DIR);
        let mut replayed = 0usize;
        let mut failed = 0usize;

        for path in entries {
            let raw = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("audit replay: read {} failed: {}", path.display(), e);
                    failed += 1;
                    continue;
                }
            };
            let pending: PendingAuditEntry = match serde_json::from_slice(&raw) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "audit replay: malformed entry {} ({}); moving to failed/",
                        path.display(),
                        e
                    );
                    move_to_failed(&path, &failed_dir);
                    failed += 1;
                    continue;
                }
            };
            let result = match pending.error {
                Some(msg) => AuditResult::Error(msg),
                None if pending.result_kind == "ok" => AuditResult::Ok,
                None => AuditResult::Error(String::new()),
            };
            match self.append(&pending.op, pending.actor, pending.params, result) {
                Ok(_) => {
                    if let Err(e) = fs::remove_file(&path) {
                        tracing::warn!(
                            "audit replay: appended {} but couldn't remove pending file: {}",
                            path.display(),
                            e
                        );
                    }
                    replayed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        "audit replay: append {} failed: {}; moving to failed/",
                        path.display(),
                        e
                    );
                    move_to_failed(&path, &failed_dir);
                    failed += 1;
                }
            }
        }

        Ok((replayed, failed))
    }

    fn write_line(&self, path: &Path, entry: &AuditEntry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{}", line)?;
        f.sync_data()?;
        Ok(())
    }

    fn write_chain_state(&self, w: &AuditWriter) -> Result<()> {
        let last_hash = w.last_hash.clone().unwrap_or_default();
        let last_file = w
            .current_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| AuditError::StateCorrupt("current_file has no basename".into()))?;
        let state = ChainState {
            last_seq: w.last_seq,
            last_hash,
            last_file,
        };
        let path = self.config.dir.join(CHAIN_STATE_FILE);
        let tmp = path.with_extension("state.tmp");
        let raw = serde_json::to_vec(&state)?;
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&raw)?;
            f.sync_data()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn rollover(&self, w: &mut AuditWriter, new_date: NaiveDate) -> Result<()> {
        let old_file = w.current_file.clone();
        w.current_file = self.config.dir.join(filename_for(new_date));
        w.current_date = new_date;
        // Compression is a best-effort cleanup of the previous day's
        // file. Pre-Batch-F a `compress_file_zstd` failure (disk full
        // mid-write was the documented hazard) propagated the error
        // back through `rollover` *after* writer state had already
        // advanced — but `chain.state` is only persisted later by the
        // append that triggered the rollover, so a failure here left
        // the writer pointing at today's file with last_seq still
        // referencing yesterday's tail. Next-day appends would write
        // into a file the chain-state cache didn't reference, and
        // startup tail-verify rejected it.
        //
        // Rollover semantics for chain integrity only need the
        // file-pointer flip to happen — the JSONL → JSONL.zst rename
        // is a storage optimization. Downgrade compression failures
        // to a logged warning so the append path can persist its
        // chain-state advance and the operator can re-run compression
        // out-of-band (or the next rollover does it).
        if self.config.compress_rotated
            && old_file.exists()
            && let Err(e) = compress_file_zstd(&old_file)
        {
            tracing::warn!(
                "audit rollover: compression of {} failed (rollover continues, next rollover or operator can retry): {}",
                old_file.display(),
                e
            );
        }
        Ok(())
    }
}

/// Wire form of an entry the daemon will replay through
/// [`AuditLog::replay_pending`]. The CLI's daemon-down audit helper
/// (`library init` / `library modify`) writes one of these per
/// operation; the daemon translates each into a regular `append`.
/// Stored as JSON so a stuck pending entry is human-debuggable
/// without re-running the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAuditEntry {
    pub op: String,
    pub actor: AuditActor,
    pub params: serde_json::Value,
    /// `"ok"` or `"error"` — split out from `error` so the file is
    /// readable at a glance.
    pub result_kind: String,
    /// Present iff `result_kind == "error"`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Queue an audit entry for the daemon to pick up next time it
/// starts. Used by daemon-down CLI flows (`library init` / `library
/// modify`); chassis-assembly ops can't talk to a running daemon.
///
/// Filename: `<UTC RFC-3339 with millis, sanitized>-<random hex>.json`.
/// Lexically sortable so the daemon replays in submission order;
/// random suffix keeps multiple same-millisecond entries distinct.
pub fn queue_pending(
    audit_dir: &Path,
    op: &str,
    actor: AuditActor,
    params: serde_json::Value,
    result: AuditResult,
) -> Result<()> {
    let pending_dir = audit_dir.join(PENDING_AUDIT_DIR);
    fs::create_dir_all(&pending_dir)?;

    let (result_kind, error) = match result {
        AuditResult::Ok => ("ok".to_string(), None),
        AuditResult::Error(msg) => ("error".to_string(), Some(msg)),
    };
    let entry = PendingAuditEntry {
        op: op.to_string(),
        actor,
        params,
        result_kind,
        error,
    };

    // Sortable: replace ':' with '-' since some filesystems don't
    // tolerate it, and append a 4-byte hex tag to break ties.
    let now = Utc::now();
    let ts = now.format("%Y-%m-%dT%H-%M-%S%.3fZ").to_string();
    let mut tag_bytes = [0u8; 4];
    rand_bytes(&mut tag_bytes);
    let tag = hex::encode(tag_bytes);
    let filename = format!("{}-{}.json", ts, tag);
    let path = pending_dir.join(&filename);

    let tmp = pending_dir.join(format!(".{}.tmp", filename));
    let raw = serde_json::to_vec(&entry)?;
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&raw)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Best-effort move of a malformed/un-appendable pending file into
/// `pending/failed/`. Logs but does not propagate errors — the
/// caller has already counted this entry as failed.
fn move_to_failed(src: &Path, failed_dir: &Path) {
    if let Err(e) = fs::create_dir_all(failed_dir) {
        tracing::warn!("audit replay: mkdir {} failed: {}", failed_dir.display(), e);
        return;
    }
    let dst = src
        .file_name()
        .map(|n| failed_dir.join(n))
        .unwrap_or_else(|| failed_dir.join("unknown.json"));
    if let Err(e) = fs::rename(src, &dst) {
        tracing::warn!(
            "audit replay: rename {} -> {} failed: {}",
            src.display(),
            dst.display(),
            e
        );
    }
}

/// Fill `out` with random bytes. Uses the OS getrandom primitive via
/// `rand`'s thread-local RNG. Pulled into a helper so the audit
/// module doesn't need to import `rand::RngCore` at call sites.
fn rand_bytes(out: &mut [u8]) {
    use rand::RngCore;
    rand::rng().fill_bytes(out);
}

/// Caller-supplied result discriminator for `append`.
#[derive(Debug, Clone)]
pub enum AuditResult {
    Ok,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub entries_checked: u64,
    pub last_seq: u64,
    pub last_hash: String,
}

/// Standalone chain-verification routine used by both `AuditLog::verify`
/// and the daemon startup pre-check (which doesn't want to instantiate
/// the writer until verification passes).
pub fn verify_chain(dir: &Path) -> Result<VerifyReport> {
    let mut files = list_audit_files(dir)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut prev_hash = GENESIS_PREV_HASH.to_string();
    let mut last_seq: u64 = 0;
    let mut entries_checked: u64 = 0;
    for (_date, path) in &files {
        let entries = read_all_entries(path)?;
        for e in entries {
            if e.seq != last_seq + 1 {
                return Err(AuditError::ChainBroken {
                    seq: e.seq,
                    stored: e.entry_hash.clone().unwrap_or_default(),
                    actual: format!("expected seq {}, got {}", last_seq + 1, e.seq),
                });
            }
            let stored_prev = e.prev_hash.clone().ok_or_else(|| AuditError::ChainBroken {
                seq: e.seq,
                stored: "<none>".into(),
                actual: prev_hash.clone(),
            })?;
            if e.op == "audit.chain_reset" {
                // The reset entry's prev_hash is the sentinel
                // `blake3:reset:<old_hash_hex>` and its params carry an
                // `old_last_hash` field. Both must match the chain's
                // current `prev_hash` at the reset point — otherwise a
                // tamperer who corrupted entry N could mint a forged
                // reset that pretends the chain broke at some other
                // hash, "healing" the corruption undetectably.
                //
                // The sentinel embeds the bare hex (the writer strips
                // the `blake3:` namespace prefix when minting it), so
                // compare both forms against the strip-prefixed
                // representation of `prev_hash`.
                let bare_prev = prev_hash
                    .strip_prefix("blake3:")
                    .unwrap_or(prev_hash.as_str())
                    .to_string();
                let expected_sentinel = format!("blake3:reset:{}", bare_prev);
                if stored_prev != expected_sentinel {
                    return Err(AuditError::ChainBroken {
                        seq: e.seq,
                        stored: stored_prev,
                        actual: expected_sentinel,
                    });
                }
                let claimed = e
                    .params
                    .get("old_last_hash")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuditError::ChainBroken {
                        seq: e.seq,
                        stored: "<missing params.old_last_hash>".into(),
                        actual: prev_hash.clone(),
                    })?;
                let claimed_bare = claimed
                    .strip_prefix("blake3:")
                    .unwrap_or(claimed)
                    .to_string();
                if claimed_bare != bare_prev {
                    return Err(AuditError::ChainBroken {
                        seq: e.seq,
                        stored: claimed.to_string(),
                        actual: prev_hash.clone(),
                    });
                }
            } else if stored_prev != prev_hash {
                return Err(AuditError::ChainBroken {
                    seq: e.seq,
                    stored: stored_prev,
                    actual: prev_hash.clone(),
                });
            }
            let recomputed = compute_entry_hash(&e);
            let stored = e.entry_hash.clone().unwrap_or_default();
            if recomputed != stored {
                return Err(AuditError::ChainBroken {
                    seq: e.seq,
                    stored,
                    actual: recomputed,
                });
            }
            prev_hash = recomputed;
            last_seq = e.seq;
            entries_checked += 1;
        }
    }
    Ok(VerifyReport {
        entries_checked,
        last_seq,
        last_hash: prev_hash,
    })
}

/// Verify the tail of the most recent audit file against `chain.state`.
/// Cheap O(file-length) check used at daemon startup.
fn verify_tail(file: &Path, state: &ChainState) -> Result<()> {
    let entries = read_all_entries(file)?;
    let last = entries.last().ok_or_else(|| {
        AuditError::StateCorrupt(format!("file {} has no entries", file.display()))
    })?;
    if last.seq != state.last_seq {
        return Err(AuditError::ChainBroken {
            seq: state.last_seq,
            stored: state.last_hash.clone(),
            actual: format!("seq mismatch: file tail seq={}", last.seq),
        });
    }
    let recomputed = compute_entry_hash(last);
    if recomputed != state.last_hash {
        return Err(AuditError::ChainBroken {
            seq: state.last_seq,
            stored: state.last_hash.clone(),
            actual: recomputed,
        });
    }
    Ok(())
}

fn filename_for(date: NaiveDate) -> String {
    format!("audit-{}.jsonl", date.format("%Y-%m-%d"))
}

fn parse_date_from_filename(name: &str) -> Option<NaiveDate> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    let date_str = stem.strip_prefix("audit-")?;
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()
}

/// Resolve `audit-YYYY-MM-DD.jsonl` to whichever of the plain or .zst
/// variant exists on disk. Used by both startup verification and
/// `verify_chain` so day-of-rollover races don't trip us up.
fn resolve_audit_file(dir: &Path, basename: &str) -> PathBuf {
    let plain = dir.join(basename);
    if plain.exists() {
        return plain;
    }
    let z = dir.join(format!("{}.zst", basename));
    if z.exists() {
        return z;
    }
    plain
}

fn list_audit_files(dir: &Path) -> Result<Vec<(NaiveDate, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("audit-") {
            continue;
        }
        if !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
            continue;
        }
        if let Some(date) = parse_date_from_filename(&name) {
            out.push((date, entry.path()));
        }
    }
    Ok(out)
}

fn read_all_entries(file: &Path) -> Result<Vec<AuditEntry>> {
    let raw = fs::read(file)?;
    // zstd magic: 28 b5 2f fd
    let bytes = if raw.len() >= 4 && raw[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        zstd::decode_all(&raw[..]).map_err(|e| AuditError::Zstd(e.to_string()))?
    } else {
        raw
    };
    let mut entries = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let e: AuditEntry = serde_json::from_slice(line)?;
        entries.push(e);
    }
    Ok(entries)
}

/// Read a date-range slice of entries (decompressing rotated files
/// transparently). Used by the `audit export` and `audit tail` CLI
/// surfaces. `from`/`to` are inclusive; `None` = unbounded.
pub fn read_entries(
    dir: &Path,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<Vec<AuditEntry>> {
    let mut files = list_audit_files(dir)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (date, path) in files {
        if let Some(f) = from
            && date < f
        {
            continue;
        }
        if let Some(t) = to
            && date > t
        {
            continue;
        }
        let mut es = read_all_entries(&path)?;
        out.append(&mut es);
    }
    Ok(out)
}

/// Streaming tail cursor for `audit tail --follow`. Tracks today's
/// active audit file and a byte offset so each poll only reads bytes
/// appended since the last call — instead of re-parsing every JSONL
/// file in the dir from genesis, which on a multi-month chain costs
/// hundreds of MB per 500 ms tick.
#[derive(Debug, Default, Clone)]
pub struct AuditTailCursor {
    /// Path of the file we're currently following. `None` until
    /// `tail_step` has run at least once. Tracked separately from the
    /// offset so a UTC-midnight rollover (today's date changes,
    /// active path becomes a new file) deterministically resets us.
    pub active_path: Option<PathBuf>,
    /// Byte offset within `active_path` we've already consumed.
    pub offset: u64,
}

impl AuditTailCursor {
    /// Fresh cursor; first `tail_step` call will discover today's
    /// file and read from offset 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bias the cursor to skip everything currently on disk. Useful
    /// after the follow loop's initial backlog read so the next poll
    /// only sees newly-appended bytes. No-op if today's file doesn't
    /// exist yet.
    pub fn skip_to_end(&mut self, dir: &Path) -> Result<()> {
        let today_path = dir.join(filename_for(Utc::now().date_naive()));
        if today_path.exists() {
            let len = fs::metadata(&today_path)?.len();
            self.offset = len;
        } else {
            self.offset = 0;
        }
        self.active_path = Some(today_path);
        Ok(())
    }
}

/// Read entries appended to today's audit file since the last call.
/// `cursor` is mutated in place — on UTC-midnight rollover the active
/// path switches to the new day's file and the offset resets to 0.
/// Returns the entries between the previous offset and the file's
/// current end-of-data, with a partial trailing line (writer mid-flush)
/// left for the next call.
pub fn tail_step(cursor: &mut AuditTailCursor, dir: &Path) -> Result<Vec<AuditEntry>> {
    let today_path = dir.join(filename_for(Utc::now().date_naive()));
    if cursor.active_path.as_ref() != Some(&today_path) {
        cursor.active_path = Some(today_path.clone());
        cursor.offset = 0;
    }
    if !today_path.exists() {
        return Ok(Vec::new());
    }
    let mut file = fs::File::open(&today_path)?;
    let len = file.metadata()?.len();
    if len < cursor.offset {
        // Truncation or rotation race — restart from the beginning.
        cursor.offset = 0;
    }
    if len == cursor.offset {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(cursor.offset))?;
    let to_read = (len - cursor.offset) as usize;
    let mut buf = vec![0u8; to_read];
    file.read_exact(&mut buf)?;
    // If the writer is mid-append we may see a trailing line without
    // its terminating '\n'. Stop at the last full newline-terminated
    // line and leave the rest for the next tick — advancing the
    // offset only by what we actually consumed.
    let consumed = match buf.iter().rposition(|&b| b == b'\n') {
        Some(p) => p + 1,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for line in buf[..consumed].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let e: AuditEntry = serde_json::from_slice(line)?;
        out.push(e);
    }
    cursor.offset += consumed as u64;
    Ok(out)
}

/// Hash all fields of `entry` except `entry_hash` itself. Uses
/// `serde_json::to_vec` on a struct whose fields are declared in fixed
/// order, with `serde_json::Value`'s default BTreeMap-backed `Map`
/// keeping nested object keys sorted — so the byte stream is
/// deterministic across runs.
pub fn compute_entry_hash(entry: &AuditEntry) -> String {
    #[derive(Serialize)]
    struct ForHash<'a> {
        seq: u64,
        ts: &'a DateTime<Utc>,
        actor: &'a AuditActor,
        op: &'a str,
        params: &'a serde_json::Value,
        result: &'a str,
        error: &'a Option<String>,
        prev_hash: &'a Option<String>,
    }
    let for_hash = ForHash {
        seq: entry.seq,
        ts: &entry.ts,
        actor: &entry.actor,
        op: &entry.op,
        params: &entry.params,
        result: &entry.result,
        error: &entry.error,
        prev_hash: &entry.prev_hash,
    };
    let bytes = serde_json::to_vec(&for_hash).expect("entry is serializable");
    let h = blake3::hash(&bytes);
    format!("blake3:{}", hex::encode(h.as_bytes()))
}

fn compress_file_zstd(file: &Path) -> Result<()> {
    let raw = fs::read(file)?;
    let compressed = zstd::encode_all(&raw[..], 3).map_err(|e| AuditError::Zstd(e.to_string()))?;
    let zpath = PathBuf::from(format!("{}.zst", file.display()));
    fs::write(&zpath, &compressed)?;
    fs::remove_file(file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_te(dir: &Path) -> AuditLog {
        AuditLog::open(AuditConfig::new(dir, AuditMode::TamperEvident)).unwrap()
    }

    #[test]
    fn tamper_evident_append_chains() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        let s1 = log
            .append(
                "test.op",
                AuditActor::system(),
                json!({"x": 1}),
                AuditResult::Ok,
            )
            .unwrap();
        let s2 = log
            .append(
                "test.op",
                AuditActor::system(),
                json!({"x": 2}),
                AuditResult::Ok,
            )
            .unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);

        let report = log.verify().unwrap();
        assert_eq!(report.entries_checked, 2);
        assert_eq!(report.last_seq, 2);
    }

    #[test]
    fn chain_state_persists_across_open() {
        let tmp = TempDir::new().unwrap();
        {
            let log = open_te(tmp.path());
            log.append("first", AuditActor::system(), json!({}), AuditResult::Ok)
                .unwrap();
            log.append("second", AuditActor::system(), json!({}), AuditResult::Ok)
                .unwrap();
        }
        let log2 = open_te(tmp.path());
        let s = log2
            .append("third", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        assert_eq!(s, 3);
        let report = log2.verify().unwrap();
        assert_eq!(report.entries_checked, 3);
    }

    #[test]
    fn detects_byte_level_tamper() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append(
            "first",
            AuditActor::system(),
            json!({"v": "original"}),
            AuditResult::Ok,
        )
        .unwrap();
        log.append("second", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();

        // Corrupt the first entry by editing the file in place.
        let files = list_audit_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        let path = &files[0].1;
        let mut raw = fs::read(path).unwrap();
        // Replace "original" with "tampered" — same length so byte
        // count is preserved.
        let needle = b"original";
        let replacement = b"tampered";
        let pos = raw
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("substring present");
        raw[pos..pos + needle.len()].copy_from_slice(replacement);
        fs::write(path, &raw).unwrap();

        let err = verify_chain(tmp.path()).unwrap_err();
        match err {
            AuditError::ChainBroken { seq, .. } => assert_eq!(seq, 1),
            other => panic!("expected ChainBroken, got {:?}", other),
        }
    }

    #[test]
    fn detects_dropped_entry() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        for i in 0..3 {
            log.append("op", AuditActor::system(), json!({"i": i}), AuditResult::Ok)
                .unwrap();
        }
        // Drop the middle entry.
        let files = list_audit_files(tmp.path()).unwrap();
        let path = &files[0].1;
        let raw = fs::read_to_string(path).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.remove(1);
        fs::write(path, lines.join("\n") + "\n").unwrap();

        let err = verify_chain(tmp.path()).unwrap_err();
        assert!(matches!(err, AuditError::ChainBroken { .. }));
    }

    #[test]
    fn rotate_after_break_recovers() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append("a", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        // Tamper.
        let files = list_audit_files(tmp.path()).unwrap();
        let path = &files[0].1;
        let mut raw = fs::read(path).unwrap();
        // Flip a byte in the entry_hash region (the entry will fail
        // recomputation).
        let last = raw.len() - 5;
        raw[last] ^= 0x01;
        fs::write(path, &raw).unwrap();
        // verify_chain should now fail.
        assert!(verify_chain(tmp.path()).is_err());

        // Operator runs `audit rotate --accept-break` (simulated):
        // open the log fresh, call rotate_after_break.
        // Note: open() will fail because verify_tail rejects. Workaround
        // is the CLI bypasses verification when --accept-break is set.
        // For the unit test we directly use the existing `log` handle
        // (which still has the in-memory state from before tamper).
        log.rotate_after_break(AuditActor::system()).unwrap();

        // After reset, new appends extend a fresh chain off the reset
        // entry. The chain still verifies through the reset point.
        log.append(
            "post-reset",
            AuditActor::system(),
            json!({}),
            AuditResult::Ok,
        )
        .unwrap();
        // The original tampered entry will still fail verify; the chain
        // is permanently broken at seq 1. The reset is visible at seq 2,
        // and seq 3 chains off seq 2. We accept that: tamper-evidence
        // means the break is visible forever.
    }

    #[test]
    fn forged_chain_reset_with_wrong_old_hash_fails_verify() {
        // A tamperer who corrupted entry N could try to "heal" the
        // chain by minting a chain_reset whose embedded
        // params.old_last_hash points to whatever the tampered chain's
        // current tail hash now claims to be — *not* the actual
        // pre-tamper hash. verify_chain must reject this.
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append("a", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        log.append("b", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();

        // Hand-write a forged chain_reset entry with bogus old_hash.
        let files = list_audit_files(tmp.path()).unwrap();
        let path = &files[0].1;
        let bogus_old = "deadbeef".repeat(8); // 64 hex chars
        let bogus_sentinel = format!("blake3:break:{}", bogus_old);
        let mut entry = AuditEntry {
            seq: 3,
            ts: Utc::now(),
            actor: AuditActor::system(),
            op: "audit.chain_reset".to_string(),
            params: json!({"old_last_hash": format!("blake3:{}", bogus_old)}),
            result: "ok".to_string(),
            error: None,
            prev_hash: Some(bogus_sentinel),
            entry_hash: None,
        };
        let h = compute_entry_hash(&entry);
        entry.entry_hash = Some(h);
        let line = serde_json::to_string(&entry).unwrap();
        let mut existing = fs::read(path).unwrap();
        existing.extend_from_slice(line.as_bytes());
        existing.push(b'\n');
        fs::write(path, &existing).unwrap();

        let err = verify_chain(tmp.path()).unwrap_err();
        match err {
            AuditError::ChainBroken { seq, .. } => assert_eq!(seq, 3),
            other => panic!("expected ChainBroken at forged reset, got {:?}", other),
        }
    }

    #[test]
    fn legitimate_chain_reset_passes_verify() {
        // Regression: the new validation must accept a properly-minted
        // reset (rotate_after_break embeds the real prev hash).
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append("a", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        log.rotate_after_break(AuditActor::system()).unwrap();
        log.append(
            "post-reset",
            AuditActor::system(),
            json!({}),
            AuditResult::Ok,
        )
        .unwrap();
        let report = verify_chain(tmp.path()).unwrap();
        assert_eq!(report.entries_checked, 3);
        assert_eq!(report.last_seq, 3);
    }

    #[test]
    fn pending_replay_drains_queue_in_order() {
        // CLI's daemon-down path drops PendingAuditEntry files into
        // <dir>/pending/; on next startup, the daemon's
        // replay_pending() picks them up in filename order, appends
        // each through the live chain, and removes the source files.
        // Three queued entries should land as seq 1..=3 and the
        // pending dir should be empty afterward.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Manually queue three entries with monotonically-sortable
        // names (queue_pending uses RFC-3339 timestamps; here we
        // hand-write to assert ordering deterministically).
        let pending_dir = dir.join(PENDING_AUDIT_DIR);
        fs::create_dir_all(&pending_dir).unwrap();
        for (i, op) in ["library.init", "library.modify", "library.modify"]
            .iter()
            .enumerate()
        {
            let entry = PendingAuditEntry {
                op: (*op).to_string(),
                actor: AuditActor::cli("op".to_string()),
                params: json!({"step": i}),
                result_kind: "ok".to_string(),
                error: None,
            };
            let path = pending_dir.join(format!("000{}-aaaa.json", i));
            let raw = serde_json::to_vec(&entry).unwrap();
            fs::write(&path, &raw).unwrap();
        }

        let log = open_te(dir);
        let (replayed, failed) = log.replay_pending().unwrap();
        assert_eq!(replayed, 3);
        assert_eq!(failed, 0);

        // Source files gone, chain reflects the three appends in
        // queue order.
        let leftover: Vec<_> = fs::read_dir(&pending_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .collect();
        assert!(leftover.is_empty(), "pending dir should be drained");

        let report = verify_chain(dir).unwrap();
        assert_eq!(report.entries_checked, 3);
        let entries = read_entries(dir, None, None).unwrap();
        assert_eq!(entries[0].op, "library.init");
        assert_eq!(entries[0].params["step"], 0);
        assert_eq!(entries[2].params["step"], 2);
    }

    #[test]
    fn pending_replay_quarantines_malformed_entry() {
        // A pending file that doesn't deserialize is moved to
        // pending/failed/ and counted under `failed`; replay
        // continues with the rest. A single bad entry shouldn't
        // wedge daemon startup.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let pending_dir = dir.join(PENDING_AUDIT_DIR);
        fs::create_dir_all(&pending_dir).unwrap();
        fs::write(pending_dir.join("0001-aaaa.json"), b"not json").unwrap();
        let good = PendingAuditEntry {
            op: "library.init".to_string(),
            actor: AuditActor::cli("op".to_string()),
            params: json!({}),
            result_kind: "ok".to_string(),
            error: None,
        };
        fs::write(
            pending_dir.join("0002-bbbb.json"),
            serde_json::to_vec(&good).unwrap(),
        )
        .unwrap();

        let log = open_te(dir);
        let (replayed, failed) = log.replay_pending().unwrap();
        assert_eq!(replayed, 1);
        assert_eq!(failed, 1);

        let failed_dir = pending_dir.join(PENDING_AUDIT_FAILED_DIR);
        let quarantined: Vec<_> = fs::read_dir(&failed_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(quarantined.len(), 1);
    }

    #[test]
    fn queue_pending_writes_sortable_filename() {
        // `queue_pending` builds a filename whose lex order matches
        // submission order across same-process calls — we don't need
        // to exercise the timestamp itself, just verify the file
        // exists, parses back as PendingAuditEntry, and lives under
        // pending/.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        queue_pending(
            dir,
            "library.init",
            AuditActor::cli("alice".to_string()),
            json!({"slots": 40}),
            AuditResult::Ok,
        )
        .unwrap();
        let pending_dir = dir.join(PENDING_AUDIT_DIR);
        let mut files: Vec<_> = fs::read_dir(&pending_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1);
        let raw = fs::read(&files[0]).unwrap();
        let pe: PendingAuditEntry = serde_json::from_slice(&raw).unwrap();
        assert_eq!(pe.op, "library.init");
        assert_eq!(pe.actor.user.as_deref(), Some("alice"));
        assert_eq!(pe.result_kind, "ok");
    }

    #[test]
    fn compress_rotated_round_trips() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let log = open_te(dir);
        log.append(
            "before",
            AuditActor::system(),
            json!({"k": "v"}),
            AuditResult::Ok,
        )
        .unwrap();

        // Force a rollover by manipulating the writer's date.
        {
            let mut w = log.state.lock().unwrap();
            // Simulate yesterday for the current file.
            let yesterday = w.current_date.pred_opt().unwrap();
            let old_basename = filename_for(yesterday);
            let new_path = dir.join(&old_basename);
            fs::rename(&w.current_file, &new_path).unwrap();
            w.current_file = new_path;
            w.current_date = yesterday;
            // Update chain.state to match the renamed file.
            log.write_chain_state(&w).unwrap();
        }

        // Next append triggers rollover + compression of the renamed file.
        log.append("after", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();

        // Expect: yesterday's file is now .zst, today's is plain.
        let files = list_audit_files(dir).unwrap();
        assert_eq!(files.len(), 2);
        let z = files
            .iter()
            .find(|(_, p)| p.to_string_lossy().ends_with(".zst"))
            .expect("rotated file should be compressed");
        let entries = read_all_entries(&z.1).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].op, "before");

        let report = verify_chain(dir).unwrap();
        assert_eq!(report.entries_checked, 2);
    }

    #[test]
    fn entry_hash_deterministic() {
        let entry = AuditEntry {
            seq: 1,
            ts: DateTime::parse_from_rfc3339("2026-05-03T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            actor: AuditActor::system(),
            op: "test".to_string(),
            params: json!({"b": 2, "a": 1}),
            result: "ok".to_string(),
            error: None,
            prev_hash: Some(GENESIS_PREV_HASH.to_string()),
            entry_hash: None,
        };
        let h1 = compute_entry_hash(&entry);
        let h2 = compute_entry_hash(&entry);
        assert_eq!(h1, h2);
        assert!(h1.starts_with("blake3:"));
        assert_eq!(h1.len(), "blake3:".len() + 64);
    }

    #[test]
    fn refuses_open_with_tampered_chain_state() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append("a", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        drop(log);
        // Corrupt chain.state hash.
        let cs_path = tmp.path().join(CHAIN_STATE_FILE);
        let raw = fs::read(&cs_path).unwrap();
        let mut state: ChainState = serde_json::from_slice(&raw).unwrap();
        state.last_hash = format!("blake3:{}", "0".repeat(64));
        fs::write(&cs_path, serde_json::to_vec(&state).unwrap()).unwrap();

        let result = AuditLog::open(AuditConfig::new(tmp.path(), AuditMode::TamperEvident));
        assert!(matches!(result, Err(AuditError::ChainBroken { .. })));
    }

    #[test]
    fn tail_step_returns_only_new_entries() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append("first.op", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();

        let mut cursor = AuditTailCursor::new();
        // First call picks up the existing entry.
        let first = tail_step(&mut cursor, tmp.path()).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].op, "first.op");

        // No new appends → empty.
        let none = tail_step(&mut cursor, tmp.path()).unwrap();
        assert!(none.is_empty());

        // Append one more, expect just it.
        log.append(
            "second.op",
            AuditActor::system(),
            json!({}),
            AuditResult::Ok,
        )
        .unwrap();
        let second = tail_step(&mut cursor, tmp.path()).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].op, "second.op");
    }

    #[test]
    fn tail_step_skip_to_end_skips_backlog() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append(
            "backlog.op",
            AuditActor::system(),
            json!({}),
            AuditResult::Ok,
        )
        .unwrap();

        let mut cursor = AuditTailCursor::new();
        cursor.skip_to_end(tmp.path()).unwrap();
        // Backlog already on disk should not be re-emitted.
        let none = tail_step(&mut cursor, tmp.path()).unwrap();
        assert!(none.is_empty());

        // Anything appended afterwards comes through.
        log.append("later.op", AuditActor::system(), json!({}), AuditResult::Ok)
            .unwrap();
        let later = tail_step(&mut cursor, tmp.path()).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].op, "later.op");
    }

    #[test]
    fn tail_step_handles_partial_trailing_line() {
        let tmp = TempDir::new().unwrap();
        let log = open_te(tmp.path());
        log.append(
            "complete.op",
            AuditActor::system(),
            json!({}),
            AuditResult::Ok,
        )
        .unwrap();

        // Manually append a partial (no trailing '\n') line to today's
        // file — the writer was mid-flush at poll time. tail_step must
        // emit only the complete line and leave the partial bytes
        // for next tick.
        let today = filename_for(Utc::now().date_naive());
        let path = tmp.path().join(&today);
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{\"seq\":99,\"ts\":\"2026-").unwrap();
        }
        let mut cursor = AuditTailCursor::new();
        let first = tail_step(&mut cursor, tmp.path()).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].op, "complete.op");

        // Finish the partial line into a valid (but unrelated) entry.
        // We don't try to reuse the BLAKE3 chain here — `tail_step` is
        // a pure parser, chain validation lives elsewhere.
        let rest = b"05-10T00:00:00Z\",\"actor\":{\"kind\":\"system\"},\"op\":\"partial.op\",\
                     \"params\":{},\"result\":\"ok\"}\n";
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(rest).unwrap();
        }
        let second = tail_step(&mut cursor, tmp.path()).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].op, "partial.op");
    }
}
