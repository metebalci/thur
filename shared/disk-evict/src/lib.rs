//! Cross-product disk-cache eviction-worker primitives.
//!
//! The VTL and VSA daemons each run a periodic per-backend cache
//! eviction worker. The two workers were near-identical: the
//! `auto`-mode cap-resolution half was byte-for-byte the same, and the
//! per-backend "usage vs cap -> log or evict" decision shared the same
//! shape. They differed only in their wakeup source (VTL wakes on an
//! upload-completion `Notify` plus a backstop tick; VSA ticks on a
//! fixed interval) and in the actual evict call (VTL's
//! `evict_lru_chunks` is async and backs chunks up to cloud before
//! deleting; VSA's is a synchronous fs-only trim).
//!
//! This crate lifts the two pieces that are genuinely identical:
//! [`resolve_and_apply_caps`] (the `auto`-mode cap recompute) and
//! [`check_usage_or_alert`] (the within-budget log + soft-watermark
//! alert, returning whether eviction is needed). The wakeup loop and
//! the evict call stay per-product.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shared_object_store::ObjectStoreConfig;
use shared_pool::{DiskCacheBounds, DiskCacheSize, PoolBudget};

/// How often the eviction worker runs the budget divergence detector
/// ([`warn_on_budget_divergence`]). Far longer than the eviction tick so
/// the full pool walk it performs never reintroduces the per-tick rescan
/// that issue #49 removed — it is a slow safety reconcile, not the hot
/// path.
pub const BUDGET_RECONCILE_INTERVAL: Duration = Duration::from_secs(3600);

/// Coarse tolerance for the budget divergence detector. The per-tick
/// usage (sourced from the exact `PoolBudget`) is compared against a
/// fresh full-pool walk; small transient skew is expected and benign:
/// a chunk reserved-but-not-yet-on-disk (the window between
/// `try_reserve` and the pool insert) and, on the tape side, in-flight
/// `.staging/` bytes measured a moment apart from the walk. A genuine
/// un-instrumented mutation site leaks unboundedly over a daemon
/// lifetime and will blow past this; a daemon restart reseeds the
/// budget from disk to heal it.
pub const BUDGET_DIVERGENCE_TOLERANCE_BYTES: u64 = 256 * 1024 * 1024;

/// Warn if the budget-derived usage has drifted from a fresh on-disk
/// walk beyond [`BUDGET_DIVERGENCE_TOLERANCE_BYTES`]. Detection only —
/// does NOT mutate the budget (a daemon restart's startup reseed is the
/// authoritative heal). Now that every pool mutation site keeps the
/// budget exact (issue #49), this should never fire in steady state; if
/// it does, a new pool-byte mutation path was added without a paired
/// budget reserve/release.
pub fn warn_on_budget_divergence(backend: &str, budget_used: u64, on_disk_walk: u64) {
    let delta = budget_used.abs_diff(on_disk_walk);
    if delta > BUDGET_DIVERGENCE_TOLERANCE_BYTES {
        tracing::warn!(
            "disk-cache budget divergence on backend '{}': budget reports {} bytes, on-disk walk reports {} bytes (delta {} > tolerance {}). \
             Every pool mutation site should keep the budget exact; this indicates an un-instrumented mutation. A daemon restart reseeds the budget from disk.",
            backend,
            budget_used,
            on_disk_walk,
            delta,
            BUDGET_DIVERGENCE_TOLERANCE_BYTES,
        );
    } else if delta > 0 {
        tracing::debug!(
            "disk-cache budget reconcile on backend '{}': budget {} vs on-disk {} (delta {} within tolerance)",
            backend,
            budget_used,
            on_disk_walk,
            delta,
        );
    }
}

/// Recompute per-backend caps for `auto`-mode entries against current
/// free space, then push the new value into each backend's
/// [`PoolBudget`] so `try_reserve` immediately sees the updated
/// ceiling. External disk pressure shrinks the cap reactively;
/// recovery grows it. Explicit-mode entries are pinned and skip the
/// recompute. Auto-mode backends are counted first so the share
/// divisor is stable across the loop.
///
/// `default_size` is the daemon-wide `disk_cache.size_gb`; a backend
/// entry's `disk_cache_size_gb` override wins when present.
pub fn resolve_and_apply_caps(
    backend_names: &[String],
    pool_budgets: &HashMap<String, Arc<PoolBudget>>,
    storage_config: &ObjectStoreConfig,
    data_dir: &Path,
    default_size: DiskCacheSize,
    bounds: DiskCacheBounds,
) {
    let resolved_sizes: Vec<(String, DiskCacheSize)> = backend_names
        .iter()
        .map(|name| {
            let size = storage_config
                .backend_entry(name)
                .ok()
                .and_then(|e| e.disk_cache_size_gb())
                .unwrap_or(default_size);
            (name.clone(), size)
        })
        .collect();
    let auto_backends: u32 = resolved_sizes
        .iter()
        .filter(|(_, s)| s.is_auto())
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    for (name, size) in &resolved_sizes {
        let Some(budget) = pool_budgets.get(name) else {
            continue;
        };
        let new_cap = size.resolve_bytes(data_dir, bounds, auto_backends);
        if budget.cap_bytes() != new_cap {
            if size.is_auto() {
                tracing::debug!(
                    "disk-cache auto-resize backend '{}': {} -> {} bytes",
                    name,
                    budget.cap_bytes(),
                    new_cap,
                );
            }
            budget.set_cap_bytes(new_cap);
        }
    }
}

/// Decide whether `backend`'s pool needs eviction this pass.
///
/// Returns `true` when `used > cap` (the caller should evict). When the
/// pool is within budget, logs the utilization line and — if the
/// backend is above its soft watermark — fires the
/// `disk_cache_watermark` alert (per-backend dedup keeps that to one
/// emit per dedup window), then returns `false`.
pub fn check_usage_or_alert(backend: &str, used: u64, cap: u64, budget: &PoolBudget) -> bool {
    if used <= cap {
        let pct = if cap == 0 {
            0
        } else {
            used.saturating_mul(100).checked_div(cap).unwrap_or(0)
        };
        tracing::debug!(
            "disk-cache pool '{}' {} / {} bytes ({}%), no eviction",
            backend,
            used,
            cap,
            pct,
        );
        if budget.over_soft_watermark() {
            shared_alerting::record::disk_cache_watermark(backend, pct, cap);
        }
        return false;
    }
    tracing::info!(
        "disk-cache pool '{}' over budget ({} / {} bytes); LRU eviction",
        backend,
        used,
        cap,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_budget_returns_false() {
        let budget = PoolBudget::new(std::path::PathBuf::from("/tmp"), 1000, 0, 90);
        assert!(!check_usage_or_alert("b", 500, 1000, &budget));
    }

    #[test]
    fn over_budget_returns_true() {
        let budget = PoolBudget::new(std::path::PathBuf::from("/tmp"), 1000, 0, 90);
        assert!(check_usage_or_alert("b", 1500, 1000, &budget));
    }

    #[test]
    fn zero_cap_with_zero_used_is_within() {
        let budget = PoolBudget::new(std::path::PathBuf::from("/tmp"), 0, 0, 90);
        assert!(!check_usage_or_alert("b", 0, 0, &budget));
    }

    /// The divergence detector tolerates small transient skew but flags
    /// a gross mismatch. (We can't assert on the emitted log here, but
    /// the branch logic + arithmetic must not panic and the tolerance
    /// boundary must be respected — exercised for coverage.)
    #[test]
    fn divergence_detector_handles_match_skew_and_drift() {
        // Exact match — no-op.
        warn_on_budget_divergence("b", 1_000, 1_000);
        // Within tolerance — debug log only.
        warn_on_budget_divergence("b", 1_000, 1_000 + BUDGET_DIVERGENCE_TOLERANCE_BYTES);
        // Beyond tolerance, both directions — warn.
        warn_on_budget_divergence("b", 0, BUDGET_DIVERGENCE_TOLERANCE_BYTES + 1);
        warn_on_budget_divergence("b", BUDGET_DIVERGENCE_TOLERANCE_BYTES + 1, 0);
    }
}
