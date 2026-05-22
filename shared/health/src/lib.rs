// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Axum handler for `GET /health` — unauthenticated liveness probe.
//!
//! Body shape is intentionally minimal: `{ status, daemon, version }`.
//! Per-product topology lives on `/info`. Mixing that into `/health`
//! conflates liveness with diagnostics — `/health` should answer
//! "is this process up?" without any auxiliary state to compute.
//!
//! Daemons mount this on their TCP HTTP server and supply `HealthMeta`
//! via `axum::extract::FromRef` from their composite state.

#![forbid(unsafe_code)]

use axum::{Json, extract::State, response::IntoResponse};
use shared_naming::ProductIdentity;

/// Per-daemon health metadata. Cheap to clone — two `'static` refs.
///
/// `version` is taken as `&'static str` so callers pass
/// `env!("CARGO_PKG_VERSION")` from their own crate (the macro must
/// resolve in the daemon, not in this shared crate).
#[derive(Clone, Copy)]
pub struct HealthMeta {
    pub product: &'static ProductIdentity,
    pub version: &'static str,
}

pub async fn health_handler(State(meta): State<HealthMeta>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "daemon": meta.product.name,
        "version": meta.version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use serde_json::Value;
    use shared_naming::{DISK, TAPE_LIBRARY};

    async fn body_for(product: &'static ProductIdentity) -> Value {
        let meta = HealthMeta {
            product,
            version: "9.9.9-test",
        };
        let resp = health_handler(State(meta)).await.into_response();
        let (_, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn vtl_shape() {
        let v = body_for(&TAPE_LIBRARY).await;
        assert_eq!(v["status"], "ok");
        assert_eq!(v["daemon"], TAPE_LIBRARY.name);
        assert_eq!(v["version"], "9.9.9-test");
        assert!(v.get("license").is_none());
        assert!(v.get("volume_count").is_none());
    }

    #[tokio::test]
    async fn vsa_shape() {
        let v = body_for(&DISK).await;
        assert_eq!(v["status"], "ok");
        assert_eq!(v["daemon"], DISK.name);
        assert_eq!(v["version"], "9.9.9-test");
    }
}
