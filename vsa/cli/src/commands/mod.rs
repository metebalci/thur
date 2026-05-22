// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa` subcommand backends. Each module here is a small
//! adapter — the `Cli` struct in `cli.rs` matches the user's
//! input and dispatches into one of these.

pub mod generate_config;
