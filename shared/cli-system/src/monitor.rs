// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `<product> system monitor` — interactive live activity screen.
//!
//! Holds and redraws ~1× per second; exits cleanly on Ctrl-C. The
//! daemon does the data-collection (one `MonitorSnapshot` JSON per
//! tick over the `system.monitor` job stream); this module renders
//! and maintains the short ring buffer the screen needs to compute
//! the 60-second cloud window and the 5-minute audit window.
//!
//! No new TUI deps: clear-screen + cursor-home is two ANSI escapes.
//! SIGINT handling is implicit — the user's Ctrl-C drops the job
//! stream, `AdminClient::run_job` returns, the function exits.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use shared_admin_client::AdminClient;
use shared_admin_monitor::{CloudEntry, MonitorSnapshot, PoolEntry, ProductSnapshot};
use shared_admin_proto::JobEvent;
use shared_naming::ProductIdentity;

const WINDOW_60S: usize = 60;
/// Ring-buffer capacity — 5 minutes' worth of 1 Hz ticks, which is the
/// widest window the screen needs (audit events over 5 m).
const RING_CAP: usize = 300;

pub async fn cmd_monitor(product: &'static ProductIdentity) -> Result<u8> {
    let client = AdminClient::auto_discover(product);
    let history: Arc<Mutex<VecDeque<MonitorSnapshot>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
    let history_cb = Arc::clone(&history);

    // Print one stub header before the first tick so an operator
    // staring at a terminal sees something — the first daemon tick
    // takes up to ~1s.
    print!("\x1B[2J\x1B[H"); // clear + cursor home
    println!("waiting for first tick from {} …", product.name);

    let result = client
        .run_job("system.monitor", &serde_json::json!({}), move |ev| {
            if let JobEvent::Log { message, .. } = ev {
                match serde_json::from_str::<MonitorSnapshot>(&message) {
                    Ok(snap) => {
                        let mut h = history_cb.lock().expect("monitor history mutex poisoned");
                        if h.len() == RING_CAP {
                            h.pop_front();
                        }
                        h.push_back(snap);
                        let frame = render(&h);
                        // Stdout flush is implicit on newline; render
                        // ends with one.
                        print!("{frame}");
                    }
                    Err(e) => {
                        eprintln!("monitor: skipping malformed tick: {e}");
                    }
                }
            }
        })
        .await;

    // Re-show the cursor and drop to a fresh line so the shell prompt
    // lands cleanly. ANSI sequence is harmless on terminals that don't
    // grok it.
    println!("\x1B[?25h");

    match result {
        Ok(code) => Ok(u8::try_from(code.max(0)).unwrap_or(2)),
        Err(e) => Err(e).context("monitor stream"),
    }
}

fn render(history: &VecDeque<MonitorSnapshot>) -> String {
    let current = match history.back() {
        Some(s) => s,
        None => return String::new(),
    };
    let mut out = String::new();
    out.push_str("\x1B[2J\x1B[H"); // clear + cursor home

    // Header row — daemon name, uptime, version.
    let uptime = (current.ts_unix - current.started_at_unix).max(0);
    out.push_str(&format!(
        "{}  up {}   v{}\n\n",
        current.daemon,
        format_uptime(uptime),
        current.version,
    ));

    // Per-product summary lines.
    match &current.product {
        ProductSnapshot::Vsa {
            volumes_online,
            sessions_active,
        } => {
            out.push_str(&format!("Volumes:   {} online\n", volumes_online));
            out.push_str(&format!("Sessions:  {} active\n\n", sessions_active));
        }
        ProductSnapshot::Vtl {
            cartridges_loaded,
            cartridges_total,
            drives_busy,
            drives_total,
            sessions_active,
        } => {
            out.push_str(&format!(
                "Cartridges: {} loaded / {}\n",
                cartridges_loaded, cartridges_total
            ));
            out.push_str(&format!(
                "Drives:     {} busy / {}\n",
                drives_busy, drives_total
            ));
            out.push_str(&format!("Sessions:   {} active\n\n", sessions_active));
        }
    }

    // Pool block — one row per (backend, namespace). Backend-wide
    // columns (cap, waiters, waits-since-boot) are repeated across
    // rows sharing a backend; the renderer prints them only on the
    // first row of each backend group and leaves blanks on the
    // continuations so the columns stay readable.
    out.push_str(
        "Pool                          used / cap                waiters / waits-since-boot\n",
    );
    if current.pool.is_empty() {
        out.push_str("  (no backends configured)\n");
    } else {
        let mut prev_backend: Option<&str> = None;
        for p in &current.pool {
            let first_in_group = prev_backend != Some(p.backend.as_str());
            out.push_str(&format!("  {}\n", render_pool_row(p, first_in_group)));
            prev_backend = Some(p.backend.as_str());
        }
    }
    out.push('\n');

    // Cloud block — diff against the snapshot from ≥60 s ago.
    let baseline_60s = baseline_at_least(history, WINDOW_60S);
    let history_secs = history.len().saturating_sub(1);
    let history_note = if history_secs < WINDOW_60S {
        format!(" ({}/{}s of history)", history_secs, WINDOW_60S)
    } else {
        String::new()
    };
    out.push_str(&format!("Cloud (last 60s){}\n", history_note));
    if current.cloud.is_empty() {
        out.push_str("  (no cloud activity since boot)\n");
    } else {
        for c in &current.cloud {
            out.push_str(&format!("  {}\n", render_cloud_row(c, baseline_60s)));
        }
    }
    out.push('\n');

    // Audit line — 5 minutes of cumulative entries.
    let baseline_5m = baseline_at_least(history, 300);
    let audit_delta = match baseline_5m {
        Some(base) => current
            .audit
            .entries_total
            .saturating_sub(base.audit.entries_total),
        None => current.audit.entries_total, // pre-window: show cumulative
    };
    out.push_str(&format!("Audit (last 5m): {} events\n", audit_delta));

    out
}

/// Find a snapshot at least `secs` seconds older than the latest.
/// Returns `None` if the ring buffer doesn't yet cover the window —
/// the caller falls back to a cumulative-since-boot reading and the
/// header annotates "(N/Ms of history)".
fn baseline_at_least(
    history: &VecDeque<MonitorSnapshot>,
    secs: usize,
) -> Option<&MonitorSnapshot> {
    let latest_ts = history.back()?.ts_unix;
    history
        .iter()
        .find(|s| latest_ts - s.ts_unix >= secs as i64)
}

fn render_pool_row(p: &PoolEntry, first_in_group: bool) -> String {
    let used = shared_cli::fmt::format_bytes(p.used_bytes);
    let label = p
        .label
        .clone()
        .or_else(|| p.namespace.clone())
        .unwrap_or_else(|| "global".to_string());
    let row_label = format!("{}/{}", p.backend, label);
    if first_in_group {
        let cap = shared_cli::fmt::format_bytes(p.cap_bytes);
        let pct = if p.cap_bytes > 0 {
            (p.used_bytes as f64 / p.cap_bytes as f64 * 100.0).round() as u64
        } else {
            0
        };
        format!(
            "{:<26}  {:>9} / {:>9} {:>3}%   waiters: {}  waits: {}",
            row_label, used, cap, pct, p.waiters_now, p.backpressure_waits_total,
        )
    } else {
        // Continuation row: same backend → same cap + backpressure
        // counters; show only used.
        format!("{:<26}  {:>9}", row_label, used)
    }
}

fn render_cloud_row(c: &CloudEntry, baseline: Option<&MonitorSnapshot>) -> String {
    // Pull the per-backend baseline entry; if the backend appeared
    // after the baseline tick, we have no prior reading and the row
    // reports the cumulative-since-boot delta (effectively
    // "everything we've seen").
    let base = baseline
        .and_then(|s| s.cloud.iter().find(|b| b.backend == c.backend))
        .cloned()
        .unwrap_or_else(|| CloudEntry {
            backend: c.backend.clone(),
            put_ops_total: 0,
            get_ops_total: 0,
            put_bytes_total: 0,
            get_bytes_total: 0,
            errors_total: 0,
        });

    let put_ops = c.put_ops_total.saturating_sub(base.put_ops_total);
    let get_ops = c.get_ops_total.saturating_sub(base.get_ops_total);
    let put_bytes = c.put_bytes_total.saturating_sub(base.put_bytes_total);
    let errors = c.errors_total.saturating_sub(base.errors_total);
    format!(
        "{:<12}  PUT {:>4} ops  {:>9}    GET {:>4} ops  errors: {}",
        c.backend,
        put_ops,
        shared_cli::fmt::format_bytes(put_bytes),
        get_ops,
        errors,
    )
}

/// `1d 2h 34m` / `2h 34m` / `34m 12s` / `12s`. Drops larger units that
/// are zero. Suits the header row; not for general use.
fn format_uptime(seconds: i64) -> String {
    if seconds < 60 {
        return format!("{}s", seconds.max(0));
    }
    let mut s = seconds.max(0) as u64;
    let d = s / 86_400;
    s %= 86_400;
    let h = s / 3_600;
    s %= 3_600;
    let m = s / 60;
    let sec = s % 60;
    if d > 0 {
        format!("{}d {}h {}m", d, h, m)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m {}s", m, sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_admin_monitor::AuditEntry;

    fn snap(ts: i64, audit_total: u64) -> MonitorSnapshot {
        MonitorSnapshot {
            ts_unix: ts,
            daemon: "thurvsad".into(),
            version: "test".into(),
            started_at_unix: 0,
            product: ProductSnapshot::Vsa {
                volumes_online: 1,
                sessions_active: 0,
            },
            pool: vec![],
            cloud: vec![],
            audit: AuditEntry {
                entries_total: audit_total,
            },
        }
    }

    #[test]
    fn baseline_picks_first_old_enough_entry() {
        let mut h: VecDeque<MonitorSnapshot> = VecDeque::new();
        for ts in 0..120i64 {
            h.push_back(snap(ts, ts as u64));
        }
        // Latest ts=119; baseline for 60s window is the first snapshot
        // where 119 - ts >= 60, i.e. ts<=59.
        let base = baseline_at_least(&h, 60).unwrap();
        assert!(base.ts_unix <= 59);
        // Newer than the oldest possible — i.e. the search finds the
        // *first* old-enough entry (FIFO order).
        assert_eq!(base.ts_unix, 0);
    }

    #[test]
    fn baseline_returns_none_when_window_uncovered() {
        let mut h: VecDeque<MonitorSnapshot> = VecDeque::new();
        for ts in 0..30i64 {
            h.push_back(snap(ts, 0));
        }
        // Only 30 s of history; the 60 s window is uncovered.
        assert!(baseline_at_least(&h, 60).is_none());
    }

    #[test]
    fn render_does_not_crash_on_empty_history() {
        let h: VecDeque<MonitorSnapshot> = VecDeque::new();
        assert_eq!(render(&h), "");
    }

    #[test]
    fn format_uptime_units_drop_zeros() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(125), "2m 5s");
        assert_eq!(format_uptime(3700), "1h 1m");
        assert_eq!(format_uptime(90_000), "1d 1h 0m");
    }

    fn pool_entry(backend: &str, ns: Option<&str>, label: Option<&str>, used: u64) -> PoolEntry {
        PoolEntry {
            backend: backend.into(),
            namespace: ns.map(String::from),
            label: label.map(String::from),
            used_bytes: used,
            cap_bytes: 1024 * 1024 * 1024,
            waiters_now: 0,
            backpressure_waits_total: 7,
        }
    }

    /// Multi-namespace render: backend header row carries cap +
    /// backpressure counters; continuation rows show only the
    /// per-namespace used bytes.
    #[test]
    fn render_pool_row_suppresses_cap_on_continuation_rows() {
        let head = render_pool_row(&pool_entry("primary", None, None, 100), true);
        let cont = render_pool_row(
            &pool_entry("primary", Some("uuid-a"), Some("vol-a"), 200),
            false,
        );
        assert!(head.contains("primary/global"));
        assert!(head.contains("waits: 7"));
        assert!(cont.contains("primary/vol-a"));
        assert!(!cont.contains("waits:"));
        assert!(!cont.contains('%'));
    }

    /// Namespace without a resolved label falls back to the raw
    /// namespace string ("primary/<ns>"). This is the VSA path when
    /// a volume has been destroyed mid-tick or the registry hasn't
    /// caught up.
    #[test]
    fn render_pool_row_falls_back_to_namespace_when_label_missing() {
        let row = render_pool_row(
            &pool_entry("primary", Some("uuid-orphan"), None, 50),
            true,
        );
        assert!(row.contains("primary/uuid-orphan"));
    }
}
