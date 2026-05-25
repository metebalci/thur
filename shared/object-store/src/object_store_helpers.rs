// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers used by every `ObjectStoreBackend` impl (S3 / GCS / Azure).
//!
//! Each of the three backend modules used to define near-identical
//! versions of these utilities side by side; pulling them into one
//! module removes the drift risk and shaves ~120 lines of duplication.
//!
//! The helpers are intentionally free functions, not trait methods —
//! they don't need backend state and the call sites are inside async
//! closures where adding a `self` argument adds noise without value.

use crate::Result;
use crate::object_store_config::{classify, is_retryable};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Initial backoff between retry attempts.
pub const INITIAL_BACKOFF_MS: u64 = 1000;
/// Cap for exponential backoff.
pub const MAX_BACKOFF_MS: u64 = 30_000;

/// Decorrelated full-jitter backoff: returns a random value in
/// `[INITIAL_BACKOFF_MS, base * 3)` clamped to `MAX_BACKOFF_MS`.
///
/// Why decorrelated jitter and not pure exponential: when many tapes
/// hit a transient backend error simultaneously (a brief 503 burst is
/// the canonical example), pure exponential synchronizes every
/// caller's retry wave at the same `2^attempt * base` mark — which is
/// exactly when the backend is most likely to still be recovering.
/// Decorrelated jitter spreads the retry wave across the whole
/// backoff window each round, so the herd thins itself out instead of
/// retrying in lockstep.
fn jittered_backoff_ms(base_ms: u64) -> u64 {
    use rand::Rng;
    let high = base_ms.saturating_mul(3).min(MAX_BACKOFF_MS);
    let low = INITIAL_BACKOFF_MS.min(high);
    if low >= high {
        return high;
    }
    rand::rng().random_range(low..high)
}

/// Concatenate the backend prefix with the per-object key. Empty
/// prefix → `key` is returned verbatim. Same shape for every backend.
pub fn full_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}{key}")
    }
}

/// Retry an async operation with exponential backoff.
///
/// * `max_retries` — total attempts after the initial; 0 disables retries.
/// * `operation_name` — used in log messages so an operator can grep
///   for which call is failing.
/// * `f` — closure producing a fresh future on each call.
///
/// **Permanent failures fail fast.** Each error goes through
/// [`crate::object_store_config::classify`] and [`crate::object_store_config::is_retryable`];
/// `Auth` / `Authz` / `NotFound` / `RegionMismatch` short-circuit out
/// without consuming the backoff budget. `Network` / `Timeout` /
/// `Other` (5xx, throttling, unclassified SDK noise) keep retrying.
pub async fn retry_async<F, Fut, T>(operation_name: &str, max_retries: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    loop {
        match f().await {
            Ok(result) => {
                if attempt > 0 {
                    debug!("{} succeeded after {} retries", operation_name, attempt);
                }
                return Ok(result);
            }
            Err(e) => {
                attempt += 1;
                let kind = classify(&e);
                if !is_retryable(kind) {
                    warn!(
                        "{} failed with permanent error ({}), not retrying: {:?}",
                        operation_name,
                        kind.label(),
                        e
                    );
                    return Err(e);
                }
                if attempt > max_retries {
                    warn!(
                        "{} failed after {} attempts: {:?}",
                        operation_name, max_retries, e
                    );
                    return Err(e);
                }
                let sleep_ms = jittered_backoff_ms(backoff_ms);
                warn!(
                    "{} failed (attempt {}/{}): {:?}, retrying in {}ms",
                    operation_name, attempt, max_retries, e, sleep_ms
                );
                sleep(Duration::from_millis(sleep_ms)).await;
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectStoreError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn retry_async_returns_immediately_on_ok() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<u32> = retry_async("test", 3, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_async_fails_fast_on_permanent_auth() {
        // ObjectStoreError::Other with an AccessDenied substring classifies
        // as Authz — permanent. retry_async must short-circuit on the
        // first attempt instead of consuming the retry budget.
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<()> = retry_async("test", 5, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::Other(
                    "AccessDenied: bucket is forbidden".to_string(),
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "permanent error must not retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_async_fails_fast_on_not_found() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<()> = retry_async("test", 5, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::Other(
                    "NoSuchBucket: my-bucket".to_string(),
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_async_exhausts_budget_on_retryable_other() {
        // Plain "Other" (unclassified, e.g. transient 5xx) is retryable —
        // retry_async should make max_retries+1 attempts and then return
        // the error. (3 retries = 4 attempts total: initial + 3 retries.)
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<()> = retry_async("test", 3, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::Other(
                    "500 internal server error".to_string(),
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_async_recovers_after_transient_failure() {
        // Fail twice with a retryable error, succeed on the 3rd attempt.
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<&'static str> = retry_async("test", 5, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(ObjectStoreError::Other("transient".to_string()))
                } else {
                    Ok("recovered")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "recovered");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_async_max_retries_zero_attempts_once() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_c = Arc::clone(&attempts);
        let result: Result<()> = retry_async("test", 0, move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::Other("transient".to_string()))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "max_retries=0 still tries once"
        );
    }
}
