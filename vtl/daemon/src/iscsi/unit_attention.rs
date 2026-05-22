// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-(TSIH, LUN) Unit Attention queue. Implementation lives in
//! `shared-iscsi`; this module re-exports the surface so existing
//! call sites (`crate::iscsi::unit_attention::*`) keep working
//! unchanged.

pub use shared_iscsi::unit_attention::{UnitAttentionCode, UnitAttentionTracker};
