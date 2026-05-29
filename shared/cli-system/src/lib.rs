// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product `system <verb>` CLI commands. Each helper is generic
//! over `shared_naming::ProductIdentity` so a single implementation
//! serves both `thurvtl` and `thurvsa`.

#![forbid(unsafe_code)]

pub mod audit;
pub mod daemon_health;
pub mod fmt;
pub mod gc;
pub mod monitor;
pub mod regenerate_cert;
pub mod secrets_io;

pub use daemon_health::cmd_daemon_health;
pub use gc::cmd_gc;
pub use monitor::cmd_monitor;
pub use regenerate_cert::cmd_regenerate_cert;
