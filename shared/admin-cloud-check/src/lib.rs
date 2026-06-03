// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product cloud-backend reachability checks.
//!
//! Two entry points, both driven by the parsed [`ObjectStoreConfig`]:
//!
//! - [`run_cloud_check`] — the `system.cloud_check` job handler. Both
//!   daemons route the job kind here (CLI verb: `system storage
//!   check`); it verifies reachability/auth on every configured
//!   backend, streams per-step progress through the [`JobEmitter`],
//!   and fires `backend_reachability` alerts on status transitions.
//! - [`run_reachability_ticker`] — the opt-in periodic ticker. Spawned
//!   by each daemon's main loop when `storage.check_interval_seconds`
//!   is non-zero; it runs the same per-backend probe on a timer so an
//!   overnight backend failure (revoked credential, quota, network
//!   partition) is caught without an operator at the console.
//!
//! Kept out of `shared-object-store` (which owns the underlying
//! [`validate_object_store_backend`] probe) so that lower-level crate
//! stays free of the `JobEmitter` job-protocol + `shared-alerting`
//! deps — the same split `shared-admin-audit` / `shared-admin-monitor`
//! use.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use shared_admin_server::{JobEmitter, JobEvent};
use shared_object_store::{ObjectStoreCheckStep, ObjectStoreConfig, validate_object_store_backend};

/// `system.cloud_check` job: verify reachability/auth on every
/// configured cloud backend.
///
/// Body is ignored (`{}`); the backends come from the daemon's already-
/// loaded `ObjectStoreConfig`, passed in as an `Arc` so the spawning
/// dispatch arm doesn't have to clone the whole config.
pub async fn run_cloud_check(emitter: JobEmitter, config: Arc<ObjectStoreConfig>) {
    let cfg = config.as_ref();
    let names = cfg.backend_names();

    emitter
        .info(format!(
            "Configured backends to check: {}",
            names.join(", ")
        ))
        .await;
    emitter.info("").await;

    let mut failed: Vec<String> = Vec::new();

    for name in &names {
        emitter.info(format!("=== Backend: {} ===", name)).await;
        if let Some(target) = cfg.target_label_named(name)
            && !target.is_empty()
        {
            emitter.info(format!("Target: {}", target)).await;
        }

        // Bridge the sync `validate_object_store_backend` step callback into
        // async event emission. The callback is sync; we collect into
        // a Vec then ship the lines after each backend completes.
        // (Cloud check steps are a handful per backend — buffering is
        // fine. If a future kind has thousands of sync callbacks
        // we'd want a sync->async bridge instead.)
        let mut steps: Vec<ObjectStoreCheckStep> = Vec::new();
        let result = validate_object_store_backend(cfg, name, |step| {
            steps.push(step);
        })
        .await;

        for s in &steps {
            emitter
                .info(format!("  [PASS] {:<6} {}", s.name, s.detail))
                .await;
        }

        match result {
            Ok(()) => {
                emitter.info("  Result: PASS").await;
                shared_alerting::record::backend_reachability(name, "recovery", None);
            }
            Err(e) => {
                let kind_label = e.kind().label();
                let step_label = e.step();
                shared_alerting::record::backend_reachability(
                    name,
                    "failure",
                    Some(&e.to_string()),
                );
                emitter
                    .info(format!("  [FAIL] {:<6} {}", step_label, kind_label))
                    .await;
                emitter
                    .info(format!("  Diagnosis: {}", e.kind().diagnosis()))
                    .await;
                emitter.info("  Hints to check:").await;
                for line in e.kind().hints().lines() {
                    emitter.info(format!("    {}", line.trim_start())).await;
                }
                emitter.info("  Raw error:").await;
                emitter.info(format!("    {}", e)).await;
                // Materialize the source chain into owned strings
                // before any await — `&dyn Error` is `!Send`, so we
                // can't hold one across an await boundary in a future
                // that tokio::spawn requires to be Send.
                let mut chain: Vec<String> = Vec::new();
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    chain.push(s.to_string());
                    src = s.source();
                }
                for line in chain {
                    emitter.info(format!("    caused by: {}", line)).await;
                }
                failed.push(name.clone());
            }
        }
        emitter.info("").await;
    }

    if failed.is_empty() {
        emitter
            .info(format!(
                "Cloud check passed for all configured backends ({}).",
                names.join(", ")
            ))
            .await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": names,
                "failed": Vec::<String>::new(),
            })))
            .await;
        emitter.emit(JobEvent::done(0)).await;
    } else {
        let summary = format!(
            "Cloud check FAILED for: {}  (passed: {}/{})",
            failed.join(", "),
            names.len() - failed.len(),
            names.len(),
        );
        emitter.error(summary.clone()).await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": names.iter().filter(|n| !failed.contains(n)).cloned().collect::<Vec<_>>(),
                "failed": failed,
            })))
            .await;
        emitter.emit(JobEvent::done_with_error(1, summary)).await;
    }
}

/// Periodic backend-reachability ticker. No-op (returns immediately)
/// when `interval_seconds` is 0 — the feature is opt-in via
/// `storage.check_interval_seconds`. Otherwise probes every configured
/// backend every `interval_seconds` and fires `backend_reachability`
/// failure/recovery transitions. Runs until the spawning task is
/// aborted at daemon shutdown.
///
/// The first interval tick (which `tokio::time::interval` fires
/// immediately) is consumed so the first probe lands one full interval
/// after boot — each daemon already validated its backends in its
/// startup path, so an at-boot re-probe would be redundant.
pub async fn run_reachability_ticker(config: Arc<ObjectStoreConfig>, interval_seconds: u64) {
    if interval_seconds == 0 {
        return;
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick
    tracing::info!(
        interval_seconds,
        backends = config.backend_names().len(),
        "backend-reachability ticker started"
    );
    loop {
        ticker.tick().await;
        probe_backends_once(config.as_ref()).await;
    }
}

/// Probe every configured backend once, firing `backend_reachability`
/// per backend. The dispatcher only emits an alert on a healthy<->failing
/// transition, so calling this every tick is cheap when nothing changes.
/// Each outcome is also logged (`warn!` on failure, `debug!` on success)
/// so a backend outage is visible in the daemon log even when no alert
/// sink is configured. `local` backends fast-path inside the probe
/// (construct-only — no network round-trip).
pub async fn probe_backends_once(config: &ObjectStoreConfig) {
    for name in config.backend_names() {
        match validate_object_store_backend(config, &name, |_step| {}).await {
            Ok(()) => {
                tracing::debug!(backend = %name, "backend-reachability probe ok");
                shared_alerting::record::backend_reachability(&name, "recovery", None);
            }
            Err(e) => {
                tracing::warn!(
                    backend = %name,
                    error = %e,
                    "backend-reachability probe FAILED"
                );
                shared_alerting::record::backend_reachability(
                    &name,
                    "failure",
                    Some(&e.to_string()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_admin_server::JobRegistry;

    #[tokio::test]
    async fn cloud_check_passes_with_no_backends() {
        // Empty config => zero backends => the all-passed branch, no
        // panic, reaches a terminal Done event.
        let config = Arc::new(ObjectStoreConfig::default());
        let registry = JobRegistry::new();
        let (_id, _started, emitter) = registry.create("system.cloud_check").await;
        run_cloud_check(emitter, config).await;
    }

    #[tokio::test]
    async fn ticker_returns_immediately_when_disabled() {
        // interval 0 = opt-in feature off. The call must return rather
        // than loop forever; a timeout proves it terminates.
        let config = Arc::new(ObjectStoreConfig::default());
        tokio::time::timeout(Duration::from_secs(1), run_reachability_ticker(config, 0))
            .await
            .expect("disabled ticker must return immediately");
    }

    #[tokio::test]
    async fn probe_no_backends_is_a_noop() {
        // No configured backends => the probe loop body never runs and
        // returns cleanly.
        let config = ObjectStoreConfig::default();
        probe_backends_once(&config).await;
    }
}
