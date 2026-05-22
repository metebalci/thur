// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Pluggable metrics hook.
//!
//! `shared-iscsi` doesn't own a meter provider — the consuming product
//! (thurvtld today, thurvsad next) wires its own OTel /
//! Prometheus stack and installs a sink here at boot. The session
//! manager calls `record::sessions_active(n)`; if no sink is installed
//! (CLI binaries, unit tests) the call is a no-op. Pattern mirrors
//! `core_mediachanger::metrics::record::*` so installing one is one
//! `From`-ish wrapper.

use std::sync::OnceLock;

/// Metrics sink installed by the consuming product.
pub trait MetricsSink: Send + Sync + 'static {
    /// Set the live-session gauge (mirrors
    /// `<product>_iscsi_sessions_active` where `<product>` is the
    /// host daemon's `shared_naming::PRODUCT.metric_prefix`). Called
    /// on every session create / remove / cleanup pass.
    fn sessions_active(&self, n: i64);
}

static SINK: OnceLock<Box<dyn MetricsSink>> = OnceLock::new();

/// Install the metrics sink. First call wins; subsequent calls
/// silently no-op (matches `core_mediachanger::metrics::install_global`).
/// Returns `true` if this call installed the sink.
pub fn install_sink(sink: Box<dyn MetricsSink>) -> bool {
    SINK.set(sink).is_ok()
}

/// Free-function recording helpers. Each looks up the global sink and
/// forwards if installed. Matches the
/// `core_mediachanger::metrics::record::*` ergonomics — call sites stay one
/// line, no `Option` handling required.
pub mod record {
    use super::SINK;

    pub fn sessions_active(n: i64) {
        if let Some(s) = SINK.get() {
            s.sessions_active(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Counting sink — captures the last value passed to
    /// `sessions_active` so the test can assert the forwarding path.
    struct CountingSink {
        last: Arc<AtomicI64>,
    }

    use std::sync::Arc;

    impl MetricsSink for CountingSink {
        fn sessions_active(&self, n: i64) {
            self.last.store(n, Ordering::SeqCst);
        }
    }

    #[test]
    fn record_no_sink_is_noop() {
        // With no sink installed (this runs before install_sink in
        // the single-binary test process if scheduled first, but the
        // call must never panic regardless of install order).
        record::sessions_active(7);
    }

    #[test]
    fn install_sink_then_record_forwards_and_first_call_wins() {
        let last = Arc::new(AtomicI64::new(-1));
        let installed = install_sink(Box::new(CountingSink {
            last: Arc::clone(&last),
        }));
        // `install_sink` returns true only when this call won the
        // OnceLock race. Either way the sink is now installed.
        let _ = installed;

        record::sessions_active(42);
        assert_eq!(last.load(Ordering::SeqCst), 42);

        record::sessions_active(0);
        assert_eq!(last.load(Ordering::SeqCst), 0);

        // A second install must not replace the live sink (first call
        // wins) — returns false.
        let other = Arc::new(AtomicI64::new(-99));
        let second = install_sink(Box::new(CountingSink {
            last: Arc::clone(&other),
        }));
        assert!(!second, "second install_sink must not win the OnceLock");

        // The original sink is still the one that receives values.
        record::sessions_active(5);
        assert_eq!(last.load(Ordering::SeqCst), 5);
        assert_eq!(other.load(Ordering::SeqCst), -99);
    }
}
