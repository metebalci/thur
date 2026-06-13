// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Daemon-owned PSK identity file (`<data_dir>/nvmetcp-psks.json`).
//!
//! One entry per host NQN. The operator generates a key on the host
//! side with `nvme gen-tls-key` and pastes the resulting
//! `NVMeTLSkey-X:NN:base64:` string under the host's NQN entry.
//! Layout mirrors `shared-iscsi`'s `iscsi-users.json` (atomic rename
//! on save, chmod 0640 on Unix, default-empty stub on first boot).
//!
//! The file is read fresh on every TLS handshake by the
//! `ClientHelloCallback` in `crate::tls` — edits via the
//! `thurvsa nvmetcp psks` verbs (or hand-edits) take effect on
//! the next session without restart or reload.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::{DhchapError, DhchapKey, parse_dhchap_secret};
use crate::tls::{ParsedInterchangeKey, PskError, parse_interchange_key};

/// One PSK entry per host NQN. The `interchange_key` field holds the
/// operator-pasted `NVMeTLSkey-X:NN:base64:` string verbatim — the
/// daemon parses + CRC-validates it at every TLS handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PskEntry {
    /// Initiator host NQN — must exactly match the
    /// `--hostnqn` value the host uses in `nvme connect`. s2n-tls
    /// picks the matching (identity, TLS-PSK) pair against the
    /// host-NQN field in the PSK identity string the client sends.
    pub host_nqn: String,
    /// `NVMeTLSkey-X:NN:base64:` interchange string. SHA-256 (NN=01)
    /// → 32-byte retained PSK + 4-byte CRC; SHA-384 (NN=02) → 48 + 4.
    /// Generated host-side via `nvme gen-tls-key`.
    pub interchange_key: String,
    /// When `true`, handshakes from this host are skipped — the
    /// callback never derives or appends this PSK. Distinct from
    /// removal so the entry keeps its audit-history continuity.
    #[serde(default)]
    pub disabled: bool,
    /// Volumes this host is admitted to, by name (VSA only). `None`
    /// or omitted = no admission fence (see-everything), preserving
    /// pre-admission behaviour. Captured per-connection at Fabrics
    /// Connect from the in-band host NQN; namespace → volume-name
    /// resolution happens per command.
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
    /// Previous key retained for a rotation grace window. Both keys
    /// derive their (identity, TLS-PSK) pairs while
    /// `previous_expires_at` is in the future; only the current key
    /// derives afterward. `nvmetcp psks rotate ... [--grace D]` is
    /// the operator surface.
    #[serde(default)]
    pub previous_interchange_key: Option<String>,
    /// Wall-clock instant at which `previous_interchange_key` stops
    /// being honored. Evaluated at handshake (no daemon-side timer);
    /// a `rotate.commit` cleanup zeroes the pair on the next mutating
    /// admin verb that observes an expired entry.
    #[serde(default)]
    pub previous_expires_at: Option<DateTime<Utc>>,
}

/// On-disk schema for `<data_dir>/nvmetcp-psks.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmetcpPsksFile {
    #[serde(default = "default_psks_file_version")]
    pub version: u32,
    #[serde(default)]
    pub psks: Vec<PskEntry>,
}

fn default_psks_file_version() -> u32 {
    1
}

impl Default for NvmetcpPsksFile {
    fn default() -> Self {
        Self {
            version: 1,
            psks: Vec::new(),
        }
    }
}

/// Resolve a host NQN to its admission set (the `volumes` field on
/// the matching `PskEntry`). Loads the file fresh — same lifecycle
/// as the TLS-PSK lookup that runs on every handshake.
///
/// Under VSA's mandatory-admission model the caller is the
/// post-Connect path in `serve_connection`, which only consults
/// this when TLS-PSK is on (TLS-off connections skip the lookup and
/// see-everything). Three return shapes:
///
/// - `Ok(Some(names))` when the host has a `volumes` allow-list set.
///   The dispatcher fences against this list.
/// - `Ok(Some(empty))` when the host has a PSK entry but its
///   `volumes` field is missing / null — a legacy entry under the
///   mandatory regime. Safe fallback: see nothing. The operator
///   re-issues with `nvmetcp psks add ... --volume ...` to recover.
/// - `Ok(None)` only when the host has no matching PSK entry at all.
///   In TLS-PSK mode this never happens for a *successful*
///   connection — the TLS handshake would have failed. Defensive
///   reading at the call site treats `None` as a hard refusal.
/// - `Err(_)` only on read-but-malformed JSON or other I/O failures.
///
/// Disabled entries are treated as absent (consistent with the
/// PskTable build path).
pub fn admission_for(
    path: &Path,
    host_nqn: &str,
) -> Result<Option<Vec<String>>, IdentityFileError> {
    let file = NvmetcpPsksFile::load(path)?;
    Ok(file
        .psks
        .into_iter()
        .find(|e| !e.disabled && e.host_nqn == host_nqn)
        .map(|e| e.volumes.unwrap_or_default()))
}

/// Load a JSON file into `T`, returning `T::default()` when the path
/// is absent. Errors only on read-but-malformed JSON or other I/O
/// failure. Shared by [`NvmetcpPsksFile`] and [`NvmetcpDhchapFile`] so
/// the two daemon-managed credential files can't drift on load
/// semantics.
fn load_json_or_default<T: DeserializeOwned + Default>(
    path: &Path,
) -> Result<T, IdentityFileError> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(IdentityFileError::Parse),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(IdentityFileError::Io(e)),
    }
}

/// Atomic JSON save (write tmp -> chmod 0640 on Unix -> rename).
/// Shared by both credential files so the on-disk discipline (atomic
/// rename + 0640) stays identical.
fn save_json_atomic_0640<T: Serialize>(path: &Path, value: &T) -> Result<(), IdentityFileError> {
    let body = serde_json::to_string_pretty(value).map_err(IdentityFileError::Parse)?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp).map_err(IdentityFileError::Io)?;
        f.write_all(body.as_bytes()).map_err(IdentityFileError::Io)?;
        // fsync the data before the rename so a power loss can't leave a
        // zero-length / torn credential file that `load` then rejects,
        // dropping every TLS-PSK / DH-CHAP host secret (issue #157).
        f.sync_all().map_err(IdentityFileError::Io)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))
            .map_err(IdentityFileError::Io)?;
    }
    std::fs::rename(&tmp, path).map_err(IdentityFileError::Io)?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

impl NvmetcpPsksFile {
    /// Load from disk. Returns the default (empty) file when the
    /// path doesn't exist. Errors only on read-but-malformed JSON.
    pub fn load(path: &Path) -> Result<Self, IdentityFileError> {
        load_json_or_default(path)
    }

    /// Atomic save (write tmp -> chmod 0640 -> rename).
    pub fn save(&self, path: &Path) -> Result<(), IdentityFileError> {
        save_json_atomic_0640(path, self)
    }

    /// Load if present, otherwise write an empty stub and return it.
    /// Boot-time entry point — first-boot operators don't have to
    /// learn the schema before the daemon runs.
    pub fn load_or_create_default(path: &Path) -> Result<Self, IdentityFileError> {
        if !path.exists() {
            let stub = Self::default();
            stub.save(path)?;
            return Ok(stub);
        }
        Self::load(path)
    }
}

/// Runtime PSK lookup, keyed on initiator host NQN. Built fresh per
/// TLS handshake by the s2n-tls `ClientHelloCallback`; the value is a
/// `Vec` of 1-2 parsed interchange keys — one for the current key
/// always, plus the previous key during a rotation grace window.
#[derive(Debug, Default)]
pub struct PskTable {
    by_host_nqn: HashMap<String, Vec<ParsedInterchangeKey>>,
}

impl PskTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_host_nqn.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_host_nqn.is_empty()
    }

    /// Iterate `(host_nqn, &[key])` pairs for PSK derivation at
    /// handshake time. Each slice has 1 element in steady state and
    /// 2 during a rotation grace window (current + previous).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[ParsedInterchangeKey])> {
        self.by_host_nqn
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Build a PskTable from a parsed file. Skips `disabled` entries.
    /// Validates every interchange string + CRC, and rejects
    /// duplicate host NQNs (silent override is a footgun). Entries in
    /// a live rotation grace window contribute two derived keys
    /// (current + previous); expired-previous entries contribute one
    /// and the stale `previous_*` fields are silently ignored
    /// (operator-side `rotate.commit` sweep zeroes them on next
    /// mutating verb).
    pub fn from_file(file: &NvmetcpPsksFile) -> Result<Arc<Self>, IdentityFileError> {
        let now = Utc::now();
        let mut by_host_nqn: HashMap<String, Vec<ParsedInterchangeKey>> =
            HashMap::with_capacity(file.psks.len());
        for entry in &file.psks {
            if entry.disabled {
                continue;
            }
            if entry.host_nqn.is_empty() {
                return Err(IdentityFileError::EmptyHostNqn);
            }
            let mut keys = Vec::with_capacity(2);
            let current = parse_interchange_key(&entry.interchange_key).map_err(|e| {
                IdentityFileError::BadKey {
                    host_nqn: entry.host_nqn.clone(),
                    source: e,
                }
            })?;
            keys.push(current);
            if let (Some(prev_key), Some(expires)) =
                (&entry.previous_interchange_key, entry.previous_expires_at)
                && expires > now
            {
                let prev =
                    parse_interchange_key(prev_key).map_err(|e| IdentityFileError::BadKey {
                        host_nqn: entry.host_nqn.clone(),
                        source: e,
                    })?;
                keys.push(prev);
            }
            if by_host_nqn.insert(entry.host_nqn.clone(), keys).is_some() {
                return Err(IdentityFileError::DuplicateHostNqn(entry.host_nqn.clone()));
            }
        }
        Ok(Arc::new(Self { by_host_nqn }))
    }
}

// ===================== DH-HMAC-CHAP secret store =====================
//
// `<data_dir>/nvmetcp-dhchap.json` — the in-band-auth analog of
// `nvmetcp-psks.json`. One entry per host NQN carrying the operator-
// pasted `DHHC-1:...` secret (from `nvme gen-dhchap-key`), an optional
// controller secret for mutual auth, the volume admission set, and the
// same rotation-grace fields. Read fresh on every Connect by the
// transport's auth phase, so `nvmetcp dhchap` edits take effect on the
// next session without restart. Same on-disk discipline as the PSK file
// (atomic rename on save, chmod 0640, default-empty stub on first boot).

/// One DH-HMAC-CHAP entry per host NQN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhchapEntry {
    /// Initiator host NQN — must match the host's `--hostnqn`.
    pub host_nqn: String,
    /// `DHHC-1:NN:base64:` host secret. The host authenticates itself
    /// with this (`nvme connect --dhchap-secret`).
    pub dhchap_key: String,
    /// Optional `DHHC-1:NN:base64:` controller secret. When set, the
    /// controller proves itself back to the host (bidirectional /
    /// mutual auth — the host runs `--dhchap-ctrl-secret <this>`).
    #[serde(default)]
    pub dhchap_ctrl_key: Option<String>,
    /// When `true`, this host's auth attempts are refused (the entry is
    /// treated as absent). Distinct from removal to preserve audit
    /// continuity.
    #[serde(default)]
    pub disabled: bool,
    /// Volumes this host is admitted to (VSA only). `None`/omitted = no
    /// admission (see-nothing safe fallback under mandatory admission).
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
    /// Previous secret retained for a rotation grace window. Both the
    /// current and previous secret validate a host response while
    /// `previous_expires_at` is in the future.
    #[serde(default)]
    pub previous_dhchap_key: Option<String>,
    /// Wall-clock instant at which `previous_dhchap_key` stops being
    /// honored (evaluated at Connect; a `rotate.commit` sweep zeroes the
    /// pair on the next mutating admin verb that observes expiry).
    #[serde(default)]
    pub previous_expires_at: Option<DateTime<Utc>>,
}

/// On-disk schema for `<data_dir>/nvmetcp-dhchap.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmetcpDhchapFile {
    #[serde(default = "default_dhchap_file_version")]
    pub version: u32,
    #[serde(default)]
    pub dhchap: Vec<DhchapEntry>,
}

fn default_dhchap_file_version() -> u32 {
    1
}

impl Default for NvmetcpDhchapFile {
    fn default() -> Self {
        Self {
            version: 1,
            dhchap: Vec::new(),
        }
    }
}

impl NvmetcpDhchapFile {
    /// Load from disk. Returns the default (empty) file when the path
    /// doesn't exist; errors only on read-but-malformed JSON.
    pub fn load(path: &Path) -> Result<Self, IdentityFileError> {
        load_json_or_default(path)
    }

    /// Atomic save (write tmp -> chmod 0640 -> rename).
    pub fn save(&self, path: &Path) -> Result<(), IdentityFileError> {
        save_json_atomic_0640(path, self)
    }

    /// Load if present, otherwise write an empty stub and return it.
    pub fn load_or_create_default(path: &Path) -> Result<Self, IdentityFileError> {
        if !path.exists() {
            let stub = Self::default();
            stub.save(path)?;
            return Ok(stub);
        }
        Self::load(path)
    }
}

/// A host's resolved DH-HMAC-CHAP credentials for one Connect.
#[derive(Debug, Clone)]
pub struct ResolvedDhchap {
    /// Host secret(s) the response is validated against — the current
    /// secret first, plus the previous secret during a rotation grace
    /// window. A host response matching *any* of these authenticates.
    pub secrets: Vec<DhchapKey>,
    /// Controller secret for mutual auth, if configured for this host.
    pub ctrl_secret: Option<DhchapKey>,
    /// Volume admission set (empty = see-nothing).
    pub volumes: Vec<String>,
}

/// Resolve a host NQN to its DH-HMAC-CHAP credentials. Loads the file
/// fresh (same lifecycle as the TLS-PSK lookup). Returns `Ok(None)`
/// when the host has no enabled entry (the auth phase then fails with
/// AUTH_Failure). Parses every `DHHC-1:` secret via
/// [`parse_dhchap_secret`], surfacing a malformed key with host context.
pub fn dhchap_lookup(
    path: &Path,
    host_nqn: &str,
) -> Result<Option<ResolvedDhchap>, IdentityFileError> {
    let file = NvmetcpDhchapFile::load(path)?;
    let now = Utc::now();
    let Some(entry) = file
        .dhchap
        .into_iter()
        .find(|e| !e.disabled && e.host_nqn == host_nqn)
    else {
        return Ok(None);
    };
    let parse = |s: &str| {
        parse_dhchap_secret(s).map_err(|source| IdentityFileError::BadDhchapKey {
            host_nqn: host_nqn.to_string(),
            source,
        })
    };
    let mut secrets = Vec::with_capacity(2);
    secrets.push(parse(&entry.dhchap_key)?);
    if let (Some(prev), Some(expires)) = (&entry.previous_dhchap_key, entry.previous_expires_at)
        && expires > now
    {
        secrets.push(parse(prev)?);
    }
    let ctrl_secret = match &entry.dhchap_ctrl_key {
        Some(k) => Some(parse(k)?),
        None => None,
    };
    Ok(Some(ResolvedDhchap {
        secrets,
        ctrl_secret,
        volumes: entry.volumes.unwrap_or_default(),
    }))
}

// ===================== Shared rotatable-entry surface =====================
//
// [`PskEntry`] and [`DhchapEntry`] are structurally the same rotatable
// per-host credential record: a host NQN, a disabled flag, a volume
// admission set, a current secret, and a (previous secret + expiry)
// rotation grace pair. These two traits expose that common surface so
// the daemon's `nvmetcp {psks,dhchap}` admin handlers can share one
// copy of the rotation state machine (begin / cancel / sweep) and the
// add / remove / grant / revoke lifecycle (issue #70). The secret
// field names (`interchange_key` vs `dhchap_key`) and the DH-HMAC-CHAP
// controller key stay surface-specific.

/// The common surface of a rotatable per-host credential entry.
pub trait HostCredentialEntry {
    fn host_nqn(&self) -> &str;
    fn disabled(&self) -> bool;
    fn set_disabled(&mut self, disabled: bool);
    fn volumes(&self) -> Option<&Vec<String>>;
    fn set_volumes(&mut self, volumes: Option<Vec<String>>);
    fn previous_expires_at(&self) -> Option<DateTime<Utc>>;

    /// True iff a previous key and its expiry are both staged — a
    /// rotation grace window is in progress.
    fn rotation_pending(&self) -> bool;

    /// Stage a rotation: move the current key into the previous slot,
    /// install `new_key` as current, and arm the grace expiry.
    fn begin_rotation(&mut self, new_key: String, expires: DateTime<Utc>);

    /// Cancel a staged rotation: restore the previous key as current
    /// and clear the grace fields. Returns `false` (a no-op) when no
    /// rotation is pending.
    fn cancel_rotation(&mut self) -> bool;

    /// Clear an expired previous key (`expires <= now`). Returns
    /// `true` when a key was cleared, so the caller can emit a
    /// `rotate.commit` audit row.
    fn sweep_expired(&mut self, now: DateTime<Utc>) -> bool;
}

/// The common surface of a daemon-managed credential file: the entry
/// list plus the on-disk load/save lifecycle.
pub trait HostCredentialFile: Default + Sized {
    type Entry: HostCredentialEntry;
    fn entries(&self) -> &[Self::Entry];
    fn entries_mut(&mut self) -> &mut Vec<Self::Entry>;
    fn from_path(path: &Path) -> Result<Self, IdentityFileError>;
    fn to_path(&self, path: &Path) -> Result<(), IdentityFileError>;
}

impl HostCredentialEntry for PskEntry {
    fn host_nqn(&self) -> &str {
        &self.host_nqn
    }
    fn disabled(&self) -> bool {
        self.disabled
    }
    fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
    }
    fn volumes(&self) -> Option<&Vec<String>> {
        self.volumes.as_ref()
    }
    fn set_volumes(&mut self, volumes: Option<Vec<String>>) {
        self.volumes = volumes;
    }
    fn previous_expires_at(&self) -> Option<DateTime<Utc>> {
        self.previous_expires_at
    }
    fn rotation_pending(&self) -> bool {
        self.previous_interchange_key.is_some() && self.previous_expires_at.is_some()
    }
    fn begin_rotation(&mut self, new_key: String, expires: DateTime<Utc>) {
        let old = std::mem::replace(&mut self.interchange_key, new_key);
        self.previous_interchange_key = Some(old);
        self.previous_expires_at = Some(expires);
    }
    fn cancel_rotation(&mut self) -> bool {
        match self.previous_interchange_key.take() {
            Some(prev) => {
                self.previous_expires_at = None;
                self.interchange_key = prev;
                true
            }
            None => false,
        }
    }
    fn sweep_expired(&mut self, now: DateTime<Utc>) -> bool {
        if let Some(expires) = self.previous_expires_at
            && expires <= now
        {
            self.previous_interchange_key = None;
            self.previous_expires_at = None;
            true
        } else {
            false
        }
    }
}

impl HostCredentialFile for NvmetcpPsksFile {
    type Entry = PskEntry;
    fn entries(&self) -> &[PskEntry] {
        &self.psks
    }
    fn entries_mut(&mut self) -> &mut Vec<PskEntry> {
        &mut self.psks
    }
    fn from_path(path: &Path) -> Result<Self, IdentityFileError> {
        Self::load(path)
    }
    fn to_path(&self, path: &Path) -> Result<(), IdentityFileError> {
        self.save(path)
    }
}

impl HostCredentialEntry for DhchapEntry {
    fn host_nqn(&self) -> &str {
        &self.host_nqn
    }
    fn disabled(&self) -> bool {
        self.disabled
    }
    fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
    }
    fn volumes(&self) -> Option<&Vec<String>> {
        self.volumes.as_ref()
    }
    fn set_volumes(&mut self, volumes: Option<Vec<String>>) {
        self.volumes = volumes;
    }
    fn previous_expires_at(&self) -> Option<DateTime<Utc>> {
        self.previous_expires_at
    }
    fn rotation_pending(&self) -> bool {
        self.previous_dhchap_key.is_some() && self.previous_expires_at.is_some()
    }
    fn begin_rotation(&mut self, new_key: String, expires: DateTime<Utc>) {
        let old = std::mem::replace(&mut self.dhchap_key, new_key);
        self.previous_dhchap_key = Some(old);
        self.previous_expires_at = Some(expires);
    }
    fn cancel_rotation(&mut self) -> bool {
        match self.previous_dhchap_key.take() {
            Some(prev) => {
                self.previous_expires_at = None;
                self.dhchap_key = prev;
                true
            }
            None => false,
        }
    }
    fn sweep_expired(&mut self, now: DateTime<Utc>) -> bool {
        if let Some(expires) = self.previous_expires_at
            && expires <= now
        {
            self.previous_dhchap_key = None;
            self.previous_expires_at = None;
            true
        } else {
            false
        }
    }
}

impl HostCredentialFile for NvmetcpDhchapFile {
    type Entry = DhchapEntry;
    fn entries(&self) -> &[DhchapEntry] {
        &self.dhchap
    }
    fn entries_mut(&mut self) -> &mut Vec<DhchapEntry> {
        &mut self.dhchap
    }
    fn from_path(path: &Path) -> Result<Self, IdentityFileError> {
        Self::load(path)
    }
    fn to_path(&self, path: &Path) -> Result<(), IdentityFileError> {
        self.save(path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityFileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("empty host_nqn entry in nvmetcp-psks.json")]
    EmptyHostNqn,
    #[error("duplicate host_nqn entry: {0}")]
    DuplicateHostNqn(String),
    #[error("bad interchange key for host {host_nqn}: {source}")]
    BadKey {
        host_nqn: String,
        #[source]
        source: PskError,
    },
    #[error("bad DH-HMAC-CHAP key for host {host_nqn}: {source}")]
    BadDhchapKey {
        host_nqn: String,
        #[source]
        source: DhchapError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::HashKind;
    use base64::Engine;

    fn make_interchange(key: &[u8], hash_code: &str) -> String {
        let crc = crc32fast::hash(key);
        let mut body = key.to_vec();
        body.extend_from_slice(&crc.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        format!("NVMeTLSkey-1:{}:{}:", hash_code, b64)
    }

    #[test]
    fn empty_file_round_trip_via_tmp_path() {
        let tmp = tempfile_path("empty");
        let _ = std::fs::remove_file(&tmp);
        let loaded = NvmetcpPsksFile::load_or_create_default(&tmp).unwrap();
        assert_eq!(loaded.version, 1);
        assert!(loaded.psks.is_empty());
        // File exists now, mode 0640 (Unix).
        assert!(tmp.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
        std::fs::remove_file(&tmp).unwrap();
    }

    fn entry(host: &str, key: &[u8]) -> PskEntry {
        PskEntry {
            host_nqn: host.into(),
            interchange_key: make_interchange(key, "01"),
            disabled: false,
            volumes: None,
            previous_interchange_key: None,
            previous_expires_at: None,
        }
    }

    fn lookup<'a>(table: &'a PskTable, host: &str) -> Option<&'a [ParsedInterchangeKey]> {
        table.iter().find(|(h, _)| *h == host).map(|(_, ks)| ks)
    }

    #[test]
    fn psk_table_builds_from_valid_entries() {
        let key = vec![0xAB; 32];
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![entry("nqn.host.a", &key)],
        };
        let table = PskTable::from_file(&file).unwrap();
        assert_eq!(table.len(), 1);
        let ks = lookup(&table, "nqn.host.a").unwrap();
        assert_eq!(ks.len(), 1);
        assert_eq!(ks[0].hash, HashKind::Sha256);
        assert_eq!(ks[0].configured_psk, key);
        assert!(lookup(&table, "nqn.host.b").is_none());
    }

    #[test]
    fn psk_table_rejects_duplicate_host_nqn() {
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![entry("nqn.host.a", &[0; 32]), entry("nqn.host.a", &[0; 32])],
        };
        assert!(matches!(
            PskTable::from_file(&file),
            Err(IdentityFileError::DuplicateHostNqn(_))
        ));
    }

    #[test]
    fn psk_table_rejects_empty_host_nqn() {
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![entry("", &[0; 32])],
        };
        assert!(matches!(
            PskTable::from_file(&file),
            Err(IdentityFileError::EmptyHostNqn)
        ));
    }

    #[test]
    fn psk_table_surfaces_bad_interchange_key_with_host_context() {
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![PskEntry {
                host_nqn: "nqn.host.a".into(),
                interchange_key: "not-a-valid-key".into(),
                disabled: false,
                volumes: None,
                previous_interchange_key: None,
                previous_expires_at: None,
            }],
        };
        let err = PskTable::from_file(&file).unwrap_err();
        match err {
            IdentityFileError::BadKey { host_nqn, .. } => assert_eq!(host_nqn, "nqn.host.a"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn psk_table_skips_disabled_entries() {
        let active = entry("nqn.host.active", &[0xAA; 32]);
        let mut off = entry("nqn.host.off", &[0xBB; 32]);
        off.disabled = true;
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![active, off],
        };
        let table = PskTable::from_file(&file).unwrap();
        assert_eq!(table.len(), 1);
        assert!(lookup(&table, "nqn.host.active").is_some());
        assert!(lookup(&table, "nqn.host.off").is_none());
    }

    #[test]
    fn psk_table_emits_two_keys_during_grace() {
        use chrono::Duration;
        let mut e = entry("nqn.host.a", &[0xAA; 32]);
        e.previous_interchange_key = Some(make_interchange(&[0xBB; 32], "01"));
        e.previous_expires_at = Some(Utc::now() + Duration::hours(1));
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![e],
        };
        let table = PskTable::from_file(&file).unwrap();
        let ks = lookup(&table, "nqn.host.a").unwrap();
        assert_eq!(ks.len(), 2);
        assert_eq!(ks[0].configured_psk, vec![0xAA; 32]);
        assert_eq!(ks[1].configured_psk, vec![0xBB; 32]);
    }

    #[test]
    fn psk_table_drops_expired_previous_key() {
        use chrono::Duration;
        let mut e = entry("nqn.host.a", &[0xAA; 32]);
        e.previous_interchange_key = Some(make_interchange(&[0xBB; 32], "01"));
        e.previous_expires_at = Some(Utc::now() - Duration::seconds(1));
        let file = NvmetcpPsksFile {
            version: 1,
            psks: vec![e],
        };
        let table = PskTable::from_file(&file).unwrap();
        let ks = lookup(&table, "nqn.host.a").unwrap();
        assert_eq!(ks.len(), 1);
        assert_eq!(ks[0].configured_psk, vec![0xAA; 32]);
    }

    fn tempfile_path(slug: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nvme-tcp-identity-test-{}-{}.json",
            slug,
            std::process::id()
        ))
    }

    // ----- DH-HMAC-CHAP secret store -----

    fn dhchap_entry(host: &str) -> DhchapEntry {
        DhchapEntry {
            host_nqn: host.into(),
            dhchap_key: crate::auth::encode_dhchap_secret(&[0xAB; 32], 0),
            dhchap_ctrl_key: None,
            disabled: false,
            volumes: Some(vec!["vol-a".into()]),
            previous_dhchap_key: None,
            previous_expires_at: None,
        }
    }

    #[test]
    fn dhchap_empty_file_round_trip_0640() {
        let tmp = tempfile_path("dhchap-empty");
        let _ = std::fs::remove_file(&tmp);
        let loaded = NvmetcpDhchapFile::load_or_create_default(&tmp).unwrap();
        assert_eq!(loaded.version, 1);
        assert!(loaded.dhchap.is_empty());
        assert!(tmp.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn dhchap_lookup_finds_secret_and_volumes() {
        let tmp = tempfile_path("dhchap-lookup");
        let file = NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![dhchap_entry("nqn.host.a")],
        };
        file.save(&tmp).unwrap();
        let r = dhchap_lookup(&tmp, "nqn.host.a").unwrap().unwrap();
        assert_eq!(r.secrets.len(), 1);
        assert_eq!(r.secrets[0].raw, vec![0xAB; 32]);
        assert_eq!(r.volumes, vec!["vol-a".to_string()]);
        assert!(r.ctrl_secret.is_none());
        // Absent host -> None.
        assert!(dhchap_lookup(&tmp, "nqn.host.missing").unwrap().is_none());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn dhchap_lookup_skips_disabled() {
        let tmp = tempfile_path("dhchap-disabled");
        let mut e = dhchap_entry("nqn.host.off");
        e.disabled = true;
        NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![e],
        }
        .save(&tmp)
        .unwrap();
        assert!(dhchap_lookup(&tmp, "nqn.host.off").unwrap().is_none());
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn dhchap_lookup_carries_ctrl_secret() {
        let tmp = tempfile_path("dhchap-ctrl");
        let mut e = dhchap_entry("nqn.host.mutual");
        e.dhchap_ctrl_key = Some(crate::auth::encode_dhchap_secret(&[0xCD; 48], 2));
        NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![e],
        }
        .save(&tmp)
        .unwrap();
        let r = dhchap_lookup(&tmp, "nqn.host.mutual").unwrap().unwrap();
        let ctrl = r.ctrl_secret.unwrap();
        assert_eq!(ctrl.raw, vec![0xCD; 48]);
        assert_eq!(ctrl.hash, 2);
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn dhchap_lookup_grace_window_includes_then_drops_previous() {
        use chrono::Duration;
        let tmp = tempfile_path("dhchap-grace");
        let mut e = dhchap_entry("nqn.host.rot");
        e.previous_dhchap_key = Some(crate::auth::encode_dhchap_secret(&[0x11; 32], 0));
        e.previous_expires_at = Some(Utc::now() + Duration::hours(1));
        NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![e.clone()],
        }
        .save(&tmp)
        .unwrap();
        let r = dhchap_lookup(&tmp, "nqn.host.rot").unwrap().unwrap();
        assert_eq!(r.secrets.len(), 2, "current + previous during grace");
        assert_eq!(r.secrets[1].raw, vec![0x11; 32]);

        // Expired previous -> only current.
        e.previous_expires_at = Some(Utc::now() - Duration::seconds(1));
        NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![e],
        }
        .save(&tmp)
        .unwrap();
        let r = dhchap_lookup(&tmp, "nqn.host.rot").unwrap().unwrap();
        assert_eq!(r.secrets.len(), 1);
        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn dhchap_lookup_bad_key_surfaces_host_context() {
        let tmp = tempfile_path("dhchap-badkey");
        let mut e = dhchap_entry("nqn.host.bad");
        e.dhchap_key = "not-a-dhchap-key".into();
        NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![e],
        }
        .save(&tmp)
        .unwrap();
        match dhchap_lookup(&tmp, "nqn.host.bad") {
            Err(IdentityFileError::BadDhchapKey { host_nqn, .. }) => {
                assert_eq!(host_nqn, "nqn.host.bad")
            }
            other => panic!("unexpected: {other:?}"),
        }
        std::fs::remove_file(&tmp).unwrap();
    }

    // ----- HostCredentialEntry rotation state machine -----
    //
    // The daemon's `nvmetcp {psks,dhchap}` admin handlers drive
    // rotation entirely through these trait methods, so the state
    // machine is unit-tested here once for both entry types.

    #[test]
    fn psk_entry_begin_cancel_rotation_round_trips() {
        let mut e = entry("nqn.host.a", &[0xAA; 32]);
        assert!(!e.rotation_pending());
        let expires = Utc::now() + chrono::Duration::hours(1);
        e.begin_rotation("new-current".into(), expires);
        assert!(e.rotation_pending());
        assert_eq!(e.interchange_key, "new-current");
        assert_eq!(e.previous_expires_at, Some(expires));
        // Cancel restores the previous key as current and clears grace.
        assert!(e.cancel_rotation());
        assert!(!e.rotation_pending());
        assert!(e.previous_interchange_key.is_none());
        assert!(e.previous_expires_at.is_none());
        // A second cancel is a no-op.
        assert!(!e.cancel_rotation());
    }

    #[test]
    fn psk_entry_sweep_clears_only_expired_previous() {
        let now = Utc::now();
        let mut active = entry("nqn.host.a", &[0xAA; 32]);
        active.begin_rotation("k".into(), now + chrono::Duration::hours(1));
        assert!(!active.sweep_expired(now), "future grace must survive");
        assert!(active.rotation_pending());

        let mut expired = entry("nqn.host.b", &[0xBB; 32]);
        expired.begin_rotation("k".into(), now - chrono::Duration::seconds(1));
        assert!(expired.sweep_expired(now), "expired grace must be swept");
        assert!(!expired.rotation_pending());
        assert!(expired.previous_interchange_key.is_none());

        // No previous staged -> sweep is a no-op.
        let mut fresh = entry("nqn.host.c", &[0xCC; 32]);
        assert!(!fresh.sweep_expired(now));
    }

    #[test]
    fn psk_entry_volume_and_disabled_accessors() {
        let mut e = entry("nqn.host.a", &[0xAA; 32]);
        assert_eq!(e.host_nqn(), "nqn.host.a");
        assert!(!e.disabled());
        e.set_disabled(true);
        assert!(e.disabled());
        assert!(e.volumes().is_none());
        e.set_volumes(Some(vec!["v1".into()]));
        assert_eq!(e.volumes().map(|v| v.len()), Some(1));
    }

    #[test]
    fn dhchap_entry_begin_cancel_sweep_round_trip() {
        let now = Utc::now();
        let mut e = dhchap_entry("nqn.host.a");
        assert!(!e.rotation_pending());
        e.begin_rotation("new-secret".into(), now + chrono::Duration::hours(1));
        assert!(e.rotation_pending());
        assert_eq!(e.dhchap_key, "new-secret");
        assert!(!e.sweep_expired(now), "future grace survives");
        assert!(e.cancel_rotation());
        assert!(e.previous_dhchap_key.is_none());

        e.begin_rotation("s2".into(), now - chrono::Duration::seconds(1));
        assert!(e.sweep_expired(now), "expired grace swept");
        assert!(e.previous_dhchap_key.is_none());
        assert!(e.previous_expires_at.is_none());
    }
}
