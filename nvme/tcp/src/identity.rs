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
use serde::{Deserialize, Serialize};

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

impl NvmetcpPsksFile {
    /// Load from disk. Returns the default (empty) file when the
    /// path doesn't exist. Errors only on read-but-malformed JSON.
    pub fn load(path: &Path) -> Result<Self, IdentityFileError> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(IdentityFileError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(IdentityFileError::Io(e)),
        }
    }

    /// Atomic save (write tmp → chmod 0640 → rename).
    pub fn save(&self, path: &Path) -> Result<(), IdentityFileError> {
        let body = serde_json::to_string_pretty(self).map_err(IdentityFileError::Parse)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(IdentityFileError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))
                .map_err(IdentityFileError::Io)?;
        }
        std::fs::rename(&tmp, path).map_err(IdentityFileError::Io)
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
}
