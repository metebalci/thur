// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Sink trait + per-type impls.

pub mod email;
pub mod webhook;

use async_trait::async_trait;

use crate::alert::Alert;

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("template render: {0}")]
    Render(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("config: {0}")]
    Config(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkOutcome {
    Success,
    Failure,
}

/// One configured sink. Owned by [`crate::dispatcher::AlertingDispatcher`]
/// inside an `Arc<dyn AlertSink>` so the dispatcher can fan out
/// alerts to multiple sinks in parallel.
#[async_trait]
pub trait AlertSink: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, alert: &Alert, product: &str, version: &str) -> Result<(), SinkError>;
}
