// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CLI-side audit log helper for the two daemon-down commands —
//! `library init` and `library modify`. Each writes a
//! `PendingAuditEntry` JSON file under `<audit_dir>/pending/` via
//! [`core_mediachanger::queue_pending`]; the daemon picks them up at
//! next startup, replays them through the live chain, and removes
//! the source files.
//!
//! Routing CLI audit writes through the queue keeps the daemon as
//! the single chain writer — no cross-process file lock required.
//! Helpers are no-ops when `audit.enabled: false` is set in the
//! config.

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
