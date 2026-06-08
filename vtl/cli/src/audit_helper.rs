// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CLI-side audit log helper for the remaining daemon-down commands
//! — `library partition *` and `library restore`. Each writes a
//! `PendingAuditEntry` JSON file under `<audit_dir>/pending/` via
//! [`core_mediachanger::queue_pending`]; the daemon picks them up at
//! next startup, replays them through the live chain, and removes
//! the source files. (Chassis topology changes — formerly
//! `library init` / `library modify` — are now daemon-side reconcile
//! events emitted directly by [`core_mediachanger::library::reconcile`]
//! and don't pass through this queue.)
//!
//! Routing CLI audit writes through the queue keeps the daemon as
//! the single chain writer — no cross-process file lock required.

use anyhow::Result;
use core_mediachanger::{AuditActor, AuditResult, queue_pending};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Single source of truth for the `audit:` YAML slice.
#[derive(Debug, Deserialize)]
pub(crate) struct AuditYaml {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    pub compress_rotated: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AuditYaml {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            dir: None,
            compress_rotated: default_true(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct ConfigSlice {
    #[serde(default)]
    audit: Option<AuditYaml>,
}

pub(crate) fn load_audit_yaml(config_path: &str) -> AuditYaml {
    let p = Path::new(config_path);
    if !p.exists() {
        return AuditYaml::default();
    }
    let raw = match std::fs::read_to_string(p) {
        Ok(s) => s,
        Err(_) => return AuditYaml::default(),
    };
    let cfg: ConfigSlice = serde_yaml::from_str(&raw).unwrap_or_default();
    cfg.audit.unwrap_or_default()
}

fn audit_dir(data_dir: &str, yaml: &AuditYaml) -> PathBuf {
    yaml.dir
        .as_ref()
        .map_or_else(|| PathBuf::from(data_dir).join("audit"), PathBuf::from)
}

/// Queue an `Ok` audit entry. No-op when audit is disabled. Failures
/// to write the queue file degrade to a stderr warning so the CLI
/// command's result is what the operator sees, not a transient queue
/// hiccup.
pub fn record_ok(data_dir: &str, config_path: &str, op: &str, params: serde_json::Value) {
    queue(data_dir, config_path, op, params, AuditResult::Ok);
}

/// Queue an `Error` audit entry.
pub fn record_err(
    data_dir: &str,
    config_path: &str,
    op: &str,
    params: serde_json::Value,
    error: &str,
) {
    queue(
        data_dir,
        config_path,
        op,
        params,
        AuditResult::Error(error.to_string()),
    );
}

/// Wrap a `Result<T>` in audit recording. On Ok, queues `op` Ok; on
/// Err, queues `op` Error with the error message. Returns the original
/// result unchanged.
pub fn record_result<T>(
    data_dir: &str,
    config_path: &str,
    op: &str,
    params: serde_json::Value,
    result: Result<T>,
) -> Result<T> {
    match &result {
        Ok(_) => record_ok(data_dir, config_path, op, params),
        Err(e) => record_err(data_dir, config_path, op, params, &e.to_string()),
    }
    result
}

fn queue(
    data_dir: &str,
    config_path: &str,
    op: &str,
    params: serde_json::Value,
    result: AuditResult,
) {
    let yaml = load_audit_yaml(config_path);
    if !yaml.enabled {
        return;
    }
    let dir = audit_dir(data_dir, &yaml);
    if let Err(e) = queue_pending(&dir, op, cli_actor(), params, result) {
        eprintln!(
            "warning: audit queue_pending failed for {op} at {}: {e}",
            dir.display()
        );
    }
}

fn cli_actor() -> AuditActor {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    AuditActor::cli(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn audit_yaml_default_enabled_and_compress() {
        let y = AuditYaml::default();
        assert!(y.enabled);
        assert!(y.compress_rotated);
        assert!(y.dir.is_none());
    }

    #[test]
    fn load_audit_yaml_missing_file_returns_default() {
        let y = load_audit_yaml("/nonexistent/path/to/thurvtl.yaml");
        assert!(y.enabled);
        assert!(y.dir.is_none());
    }

    #[test]
    fn load_audit_yaml_parses_explicit_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        let mut f = std::fs::File::create(&cfg).expect("create cfg");
        writeln!(f, "audit:\n  enabled: false\n  dir: /var/log/thur-audit").expect("write cfg");
        let y = load_audit_yaml(cfg.to_str().expect("utf8 path"));
        assert!(!y.enabled);
        assert_eq!(y.dir.as_deref(), Some("/var/log/thur-audit"));
    }

    #[test]
    fn load_audit_yaml_missing_audit_block_uses_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        let mut f = std::fs::File::create(&cfg).expect("create cfg");
        writeln!(f, "data_dir: /srv/thur").expect("write cfg");
        let y = load_audit_yaml(cfg.to_str().expect("utf8 path"));
        assert!(y.enabled);
        assert!(y.dir.is_none());
    }

    #[test]
    fn load_audit_yaml_malformed_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        let mut f = std::fs::File::create(&cfg).expect("create cfg");
        writeln!(f, "audit: [this is not a map").expect("write cfg");
        let y = load_audit_yaml(cfg.to_str().expect("utf8 path"));
        assert!(y.enabled);
    }

    #[test]
    fn audit_dir_falls_back_to_data_dir_subdir() {
        let yaml = AuditYaml::default();
        let d = audit_dir("/srv/thur", &yaml);
        assert_eq!(d, PathBuf::from("/srv/thur/audit"));
    }

    #[test]
    fn audit_dir_honours_explicit_override() {
        let yaml = AuditYaml {
            enabled: true,
            dir: Some("/var/log/thur-audit".to_string()),
            compress_rotated: true,
        };
        let d = audit_dir("/srv/thur", &yaml);
        assert_eq!(d, PathBuf::from("/var/log/thur-audit"));
    }

    #[test]
    fn record_ok_noop_when_audit_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        let mut f = std::fs::File::create(&cfg).expect("create cfg");
        writeln!(f, "audit:\n  enabled: false").expect("write cfg");
        // Disabled audit must not create the pending queue dir.
        record_ok(
            dir.path().to_str().expect("utf8"),
            cfg.to_str().expect("utf8"),
            "library.partition.create",
            serde_json::json!({"name": "p0"}),
        );
        assert!(!dir.path().join("audit").join("pending").exists());
    }

    #[test]
    fn record_ok_queues_entry_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir data");
        let cfg = dir.path().join("thurvtl.yaml");
        std::fs::write(&cfg, "audit:\n  enabled: true\n").expect("write cfg");
        record_ok(
            data_dir.to_str().expect("utf8"),
            cfg.to_str().expect("utf8"),
            "library.partition.create",
            serde_json::json!({"name": "p0"}),
        );
        let pending = data_dir.join("audit").join("pending");
        assert!(pending.is_dir());
        let count = std::fs::read_dir(&pending).expect("read pending").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn record_result_passes_through_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        std::fs::write(&cfg, "audit:\n  enabled: false\n").expect("write cfg");
        let r: Result<i32> = record_result(
            dir.path().to_str().expect("utf8"),
            cfg.to_str().expect("utf8"),
            "library.restore",
            serde_json::json!({}),
            Ok(42),
        );
        assert_eq!(r.expect("ok value"), 42);
    }

    #[test]
    fn record_result_passes_through_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        std::fs::write(&cfg, "audit:\n  enabled: false\n").expect("write cfg");
        let r: Result<i32> = record_result(
            dir.path().to_str().expect("utf8"),
            cfg.to_str().expect("utf8"),
            "library.restore",
            serde_json::json!({}),
            Err(anyhow::anyhow!("boom")),
        );
        assert!(r.is_err());
    }
}
