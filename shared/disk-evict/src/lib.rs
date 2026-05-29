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

use shared_object_store::ObjectStoreConfig;
use shared_pool::{DiskCacheBounds, DiskCacheSize, PoolBudget};

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
}
