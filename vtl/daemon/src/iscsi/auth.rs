// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CHAP authentication. Implementation lives in `shared-iscsi`; this
//! module re-exports the surface so existing call sites
//! (`crate::iscsi::auth::*`) keep working unchanged.

// `format_algorithm_list` / `select_algorithm` are consumed by
// `shared_iscsi::transport::handle_login_phase` after Step 3c
// phase 2 — no thurvtl-side caller anymore. `ChapAuthenticator`
// stays re-exported because `IscsiServer::new` constructs the
// authenticator.
pub use shared_iscsi::auth::ChapAuthenticator;
