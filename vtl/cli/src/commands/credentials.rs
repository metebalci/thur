// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl iscsi users` + `iscsi target` verbs. Thin trampoline
//! over the cross-product implementations in `shared_cli_iscsi` —
//! the shared crate holds the daemon-routed-only posture, wire
//! shapes, and audit op names so VTL and VSA stay in lockstep.

use anyhow::Result;

const PRODUCT: &shared_naming::ProductIdentity = &shared_naming::TAPE_LIBRARY;

pub async fn users_list(json: bool) -> Result<()> {
    shared_cli_iscsi::users_list(PRODUCT, json).await
}

pub async fn users_add(
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    mutual_chap: bool,
    partition: Option<&str>,
) -> Result<()> {
    shared_cli_iscsi::users_add(
        PRODUCT,
        name,
        password_arg,
        password_stdin,
        mutual_chap,
        partition,
    )
    .await
}

pub async fn users_remove(name: &str) -> Result<()> {
    shared_cli_iscsi::users_remove(PRODUCT, name).await
}

pub async fn users_set_disabled(name: &str, disabled: bool) -> Result<()> {
    shared_cli_iscsi::users_set_disabled(PRODUCT, name, disabled).await
}

pub async fn users_rotate(
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    grace: &str,
) -> Result<()> {
    shared_cli_iscsi::users_rotate(PRODUCT, name, password_arg, password_stdin, grace).await
}

pub async fn users_rotate_cancel(name: &str) -> Result<()> {
    shared_cli_iscsi::users_rotate_cancel(PRODUCT, name).await
}

pub async fn target_show(json: bool) -> Result<()> {
    shared_cli_iscsi::target_show(PRODUCT, json).await
}

pub async fn target_set(
    username: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
) -> Result<()> {
    shared_cli_iscsi::target_set(PRODUCT, username, password_arg, password_stdin).await
}

pub async fn target_clear() -> Result<()> {
    shared_cli_iscsi::target_clear(PRODUCT).await
}
