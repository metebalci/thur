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
