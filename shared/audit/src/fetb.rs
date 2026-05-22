// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Front-end TB (FETB) telemetry sampler, shared by `thurvtld`
//! and `thurvsad`.
//!
//! FETB is the sum of `host_bytes_written` across every cartridge /
//! volume — pre-dedup, pre-compression, exactly what the host shoved
//! at the daemon over each object's lifetime. The sampler is pure
//! telemetry: each tick it walks the per-object `runtime.json` files
//! for the raw byte count, emits one `fetb.sample` audit row, counts
//! how many samples sit in the trailing window, and publishes both
//! numbers as Prometheus gauges.
//!
//! The audit log is the only persistence. The sample-count gauge is
//! rebuilt from the trailing [`WINDOW_DAYS`] of `fetb.sample` rows on
//! every tick, so it survives daemon restarts without a separate
//! cache file — tampering is caught by the audit chain-verify step.
//!
//! Tunables are deliberately hardcoded (not config knobs): 6 h
//! cadence smooths spike loads without burning audit-log volume; the
//! 28-day window is 4 weeks exact so day-of-week backup cadence
//! cancels out.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{AuditActor, AuditChannel, AuditError, AuditResult, read_entries};

/// Cadence at which the daemon takes an FETB sample. One `fetb.sample`
/// audit event is emitted every interval. 6 hours = 4 samples/day.
pub const SAMPLE_INTERVAL_HOURS: u32 = 6;

/// Trailing window the sample-count gauge is computed over. 28 days =
/// 4 weeks exact. History older than this is ignored.
pub const WINDOW_DAYS: i64 = 28;

/// Audit-log `op` value the sampler emits. Verified by `verify_chain`
/// like every other entry.
pub const FETB_SAMPLE_OP: &str = "fetb.sample";

/// Lightweight view of `runtime.json` — only the field the FETB
/// sampler needs. Decoupled from either product's full Runtime
/// struct so the sampler doesn't drag in the open path.
#[derive(Debug, Deserialize)]
struct RuntimePeek {
    #[serde(default)]
    host_bytes_written: u64,
}

/// Walk every per-object directory under `objects_dir` and sum the
/// `host_bytes_written` field out of each object's `runtime.json`
/// (pre-dedup, pre-compression, pre-encryption — exactly what the
/// host shoved at the daemon across each object's lifetime). Both
/// VTL and VSA persist their FETB counter in `runtime.json`, so a
/// single reader serves both.
///
/// Subdirectories without `runtime.json` (or with an unparseable
/// one) contribute 0 — a best-effort number that's usually right is
/// preferred over "refuse to measure if anything is funny." Pass
/// `<data_dir>/tapes/` for VTL or `<data_dir>/volumes/` for VSA.
#[must_use]
pub fn take_sample(objects_dir: &Path) -> u64 {
    if !objects_dir.is_dir() {
        return 0;
    }
    let entries = match std::fs::read_dir(objects_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let obj_root = entry.path();
        if !obj_root.is_dir() {
            continue;
        }
        let path = obj_root.join("runtime.json");
        if !path.is_file() {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let peek: RuntimePeek = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(_) => continue,
        };
        total = total.saturating_add(peek.host_bytes_written);
    }
    total
}

/// Count the `fetb.sample` audit rows in the trailing [`WINDOW_DAYS`]
/// ending at `now`. Genuine I/O errors (audit dir missing, file
/// unreadable) propagate; malformed entries inside otherwise-valid
/// files are silently skipped, matching the rest of the audit-walk
/// code.
pub fn count_samples_in_window(audit_dir: &Path, now: DateTime<Utc>) -> Result<usize, AuditError> {
    let from = (now - ChronoDuration::days(WINDOW_DAYS + 1)).date_naive();
    let entries = read_entries(audit_dir, Some(from), None)?;
    let cutoff = now - ChronoDuration::days(WINDOW_DAYS);
    let count = entries
        .into_iter()
        .filter(|e| e.op == FETB_SAMPLE_OP && e.ts >= cutoff)
        .count();
    Ok(count)
}

/// Take one FETB sample, emit the `fetb.sample` audit event, recount
/// the rolling window, publish the two telemetry gauges. Run on every
/// sampler tick and once synchronously at daemon boot.
pub async fn record_fetb_sample(
    data_dir: &Path,
    audit_dir: &Path,
    subdir: &str,
    audit_log: Option<&AuditChannel>,
) {
    let now = Utc::now();
    let object_dir = data_dir.join(subdir);
    let fetb_bytes = take_sample(&object_dir);

    if let Some(log) = audit_log {
        let params = serde_json::json!({
            "ts": now.to_rfc3339(),
            "fetb_bytes": fetb_bytes,
        });
        log.try_append(
            FETB_SAMPLE_OP,
            AuditActor::daemon(),
            params,
            AuditResult::Ok,
        );
    }

    // Recount the window from the audit log so the gauge reflects
    // samples across daemon restarts.
    let sample_count = match count_samples_in_window(audit_dir, now) {
        Ok(c) => c,
        Err(e) => {
            warn!("fetb: window count failed at sample time: {}", e);
            return;
        }
    };

    // Telemetry — Prometheus gauges read by the operator + monitoring.
    // Process-global MeterProvider with per-product prefix
    // (`thurvtl_*` / `thurvsa_*`) attached at telemetry boot.
    shared_telemetry::record::fetb(fetb_bytes, sample_count as u64);
    info!(
        "fetb: latest={} bytes, {} sample(s) in {}-day window",
        fetb_bytes, sample_count, WINDOW_DAYS
    );
}

/// Periodic FETB sampler. Sleeps [`SAMPLE_INTERVAL_HOURS`] between
/// ticks; on each tick takes one sample and publishes telemetry.
pub async fn run_fetb_sampler(
    data_dir: PathBuf,
    audit_dir: PathBuf,
    subdir: &'static str,
    audit_log: Option<AuditChannel>,
) {
    let interval = Duration::from_secs(u64::from(SAMPLE_INTERVAL_HOURS) * 3600);
    info!(
        "fetb: sampler started (interval={}h)",
        SAMPLE_INTERVAL_HOURS
    );
    loop {
        tokio::time::sleep(interval).await;
        record_fetb_sample(&data_dir, &audit_dir, subdir, audit_log.as_ref()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_sample_on_empty_dir_returns_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Pointing at a non-existent subdir is fine — returns 0.
        assert_eq!(take_sample(&tmp.path().join("nope")), 0);
        // Pointing at an empty real dir also returns 0.
        assert_eq!(take_sample(tmp.path()), 0);
    }

    #[test]
    fn take_sample_sums_host_bytes_across_objects() {
        let tmp = tempfile::TempDir::new().unwrap();
        for (name, hbw) in [("obj1", 1_000u64), ("obj2", 2_500u64)] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let json = serde_json::json!({ "host_bytes_written": hbw });
            std::fs::write(dir.join("runtime.json"), json.to_string()).unwrap();
        }
        assert_eq!(take_sample(tmp.path()), 3_500);
    }

    #[test]
    fn take_sample_skips_malformed_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bad = tmp.path().join("borked");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("runtime.json"), b"{not valid json").unwrap();

        let good = tmp.path().join("good");
        std::fs::create_dir_all(&good).unwrap();
        let json = serde_json::json!({ "host_bytes_written": 42 });
        std::fs::write(good.join("runtime.json"), json.to_string()).unwrap();

        assert_eq!(take_sample(tmp.path()), 42);
    }

    #[test]
    fn take_sample_treats_missing_field_as_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({ "name": "legacy", "size_bytes": 1024 });
        std::fs::write(dir.join("runtime.json"), json.to_string()).unwrap();
        assert_eq!(take_sample(tmp.path()), 0);
    }
}
