// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared lifecycle for the two NVMe-TCP host-credential admin
//! surfaces — TLS-PSK ([`super::nvmetcp_psks`]) and DH-HMAC-CHAP
//! ([`super::nvmetcp_dhchap`]).
//!
//! Both files are the same rotatable per-host record (host NQN +
//! disabled flag + volume admission set + current secret + a
//! previous-secret rotation grace pair). This module owns one copy of
//! the `add` / `remove` / `disable` / `enable` / `rotate` /
//! `rotate cancel` / `grant` / `revoke` verbs, generic over the
//! [`Surface`] trait, so the two surfaces can't diverge on a future
//! fix (issue #70). Each surface supplies only its differences: the
//! on-disk file type, the response row, the secret validator, the
//! audit-op prefix, and a handful of noun strings.
//!
//! `list` and the DH-HMAC-CHAP-only `set-ctrl-key` / `clear-ctrl-key`
//! verbs stay in the per-surface modules — their wire shapes differ.
//!
//! VSA-only — VTL doesn't carry an NVMe-TCP transport.

use axum::{Json, extract::State, http::StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use nvme_tcp::identity::{HostCredentialEntry, HostCredentialFile};
use serde::Serialize;
use serde_json::json;
use shared_admin_server::PeerCred;
use shared_audit::{AuditActor, AuditResult};
use std::path::{Path, PathBuf};

use super::handlers::AdminState;
use super::iscsi_users::ApiError;

/// Process-global serialization for the read-modify-write on the
/// NVMe-TCP credential files (psks + dhchap). The admin server handles
/// connections concurrently (one task per accepted connection), so
/// without this two overlapping mutating verbs interleave as lost
/// updates: both `load()` the same file and each `save()`s its own copy,
/// last atomic-rename wins — a `revoke` racing a `grant`/`rotate` was
/// silently discarded, leaving a host admitted to a volume the operator
/// believed revoked (issue #223). Same fix as the `iscsi-users.json`
/// analog (issue #207). One global covers both surfaces and the
/// dhchap ctrl-key verbs; admin mutations are rare and human-driven, so
/// the coarse grain is fine. Held across the whole load→mutate→save.
pub(crate) static NVMETCP_HOST_WRITE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

// ---------- shared request types ----------
//
// The admin-socket request bodies are identical across the two
// surfaces (the secret is a neutral `key` field — the surface-specific
// on-disk field name `interchange_key` / `dhchap_key` is mapped in
// `Surface::build_entry`).

#[derive(Debug, serde::Deserialize)]
pub struct AddRequest {
    pub host_nqn: String,
    pub key: String,
    /// Optional controller secret enabling DH-HMAC-CHAP mutual auth
    /// from creation. Ignored by the TLS-PSK surface.
    #[serde(default)]
    pub ctrl_key: Option<String>,
    /// Volumes this host is admitted to. Mandatory (>= 1) — the
    /// handler rejects unknown names before persisting.
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct HostNqnOnlyRequest {
    pub host_nqn: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct RotateRequest {
    pub host_nqn: String,
    pub key: String,
    pub grace_seconds: u64,
}

#[derive(Debug, serde::Deserialize)]
pub struct GrantRequest {
    pub host_nqn: String,
    pub volumes: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct RevokeRequest {
    pub host_nqn: String,
    pub volumes: Vec<String>,
}

// ---------- the per-surface contract ----------

/// The differences between the TLS-PSK and DH-HMAC-CHAP surfaces. The
/// generic handlers below close over an implementor to do everything
/// the two have in common.
pub trait Surface: Send + Sync + 'static {
    /// On-disk file type (`NvmetcpPsksFile` / `NvmetcpDhchapFile`).
    type File: HostCredentialFile + Send;
    /// Response row returned by mutating verbs + `list`.
    type Row: Serialize + Send + 'static;

    /// Audit-op prefix, e.g. `"nvmetcp.psks"` — ops are
    /// `{PREFIX}.add`, `{PREFIX}.rotate.start`, etc.
    const AUDIT_PREFIX: &'static str;
    /// Human noun for not-found / conflict messages, e.g. `"PSK"` /
    /// `"DH-HMAC-CHAP entry"`.
    const ENTITY: &'static str;
    /// CLI verb name used in the revoke-would-empty hint: `"psks"` /
    /// `"dhchap"`.
    const VERB: &'static str;

    /// Resolved on-disk path of this surface's credential file.
    fn path(state: &AdminState) -> PathBuf;

    /// Validate a secret string (interchange key / DH-HMAC-CHAP
    /// secret), mapping a parse failure to a 400.
    fn validate_key(s: &str) -> Result<(), ApiError>;

    /// Validate the secret(s) carried by an `add` request, in the
    /// order the surface expects. The default validates only `key` —
    /// a surface with no controller secret ignores `ctrl_key`, exactly
    /// as it did before it shared the request type. The DH-HMAC-CHAP
    /// surface overrides this to also validate `ctrl_key` when present.
    fn validate_add(req: &AddRequest) -> Result<(), ApiError> {
        Self::validate_key(&req.key)
    }

    /// Build the response row for an entry.
    fn row(entry: &<Self::File as HostCredentialFile>::Entry) -> Self::Row;

    /// Construct a new entry from a validated add request. The key(s)
    /// have already passed [`Surface::validate_key`]; this just moves
    /// the strings into the surface-specific record.
    fn build_entry(req: AddRequest) -> <Self::File as HostCredentialFile>::Entry;

    /// Extra audit params for `add` beyond `{ "host_nqn": ... }` —
    /// the DH-HMAC-CHAP surface adds `"mutual"`.
    fn add_audit_params(entry: &<Self::File as HostCredentialFile>::Entry) -> serde_json::Value;
}

type EntryOf<S> = <<S as Surface>::File as HostCredentialFile>::Entry;

// ---------- generic handlers ----------

pub async fn add<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<AddRequest>,
) -> Result<(StatusCode, Json<S::Row>), ApiError> {
    validate_host_nqn(&body.host_nqn)?;
    S::validate_add(&body)?;
    // VSA-mandatory: every entry must declare an admission set.
    let names = body
        .volumes
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("at least one --volume required"))?;
    if names.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, names)?;

    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);

    if file.entries().iter().any(|e| e.host_nqn() == body.host_nqn) {
        return Err(ApiError::conflict(format!(
            "{} for host '{}' already exists",
            S::ENTITY,
            body.host_nqn
        )));
    }

    let entry = S::build_entry(body);
    let row = S::row(&entry);
    let audit_params = S::add_audit_params(&entry);
    file.entries_mut().push(entry);
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(&state, &peer, &op::<S>("add"), audit_params);
    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn remove<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let before = file.entries().len();
    file.entries_mut().retain(|e| e.host_nqn() != body.host_nqn);
    if file.entries().len() == before {
        return Err(not_found::<S>(&body.host_nqn));
    }
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>("remove"),
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled::<S>(state, peer, body.host_nqn, true).await
}

pub async fn enable<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled::<S>(state, peer, body.host_nqn, false).await
}

async fn toggle_disabled<S: Surface>(
    state: AdminState,
    peer: PeerCred,
    host_nqn: String,
    disabled: bool,
) -> Result<StatusCode, ApiError> {
    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let entry = find_mut::<S>(&mut file, &host_nqn)?;
    entry.set_disabled(disabled);
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>(if disabled { "disable" } else { "enable" }),
        json!({ "host_nqn": host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rotate<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RotateRequest>,
) -> Result<(StatusCode, Json<S::Row>), ApiError> {
    S::validate_key(&body.key)?;
    if body.grace_seconds == 0 {
        return Err(ApiError::bad_request(
            "grace_seconds must be > 0; use add + remove for an immediate cutover",
        ));
    }

    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let entry = find_mut::<S>(&mut file, &body.host_nqn)?;

    if entry.rotation_pending() {
        return Err(ApiError::conflict(format!(
            "{} for host '{}' already has a pending rotation; cancel it first",
            S::ENTITY,
            body.host_nqn
        )));
    }

    let expires = Utc::now() + ChronoDuration::seconds(body.grace_seconds as i64);
    entry.begin_rotation(body.key, expires);
    let row = S::row(entry);
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>("rotate.start"),
        json!({
            "host_nqn": body.host_nqn,
            "grace_seconds": body.grace_seconds,
            "previous_expires_at": expires,
        }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn rotate_cancel<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let entry = find_mut::<S>(&mut file, &body.host_nqn)?;
    if !entry.cancel_rotation() {
        return Err(ApiError::conflict(format!(
            "no rotation in progress for {} '{}'",
            S::ENTITY,
            body.host_nqn
        )));
    }
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>("rotate.cancel"),
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn grant<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<GrantRequest>,
) -> Result<(StatusCode, Json<S::Row>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, &body.volumes)?;
    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let entry = find_mut::<S>(&mut file, &body.host_nqn)?;

    let mut current = entry.volumes().cloned().unwrap_or_default();
    let added: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| !current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    current.extend(added.iter().cloned());
    entry.set_volumes(Some(current));

    let row = S::row(entry);
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>("grant"),
        json!({ "host_nqn": body.host_nqn, "volumes_added": added }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn revoke<S: Surface>(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RevokeRequest>,
) -> Result<(StatusCode, Json<S::Row>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    let _write_guard = NVMETCP_HOST_WRITE_LOCK.lock().await;
    let (path, mut file) = load::<S>(&state)?;
    let swept = sweep_all::<S>(&mut file);
    let entry = find_mut::<S>(&mut file, &body.host_nqn)?;

    let current = entry.volumes().cloned().unwrap_or_default();
    let removed: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    let next: Vec<String> = current
        .into_iter()
        .filter(|c| !body.volumes.iter().any(|v| v == c))
        .collect();
    if next.is_empty() {
        return Err(ApiError::bad_request(format!(
            "revoke would leave host '{}' with no volumes; use `nvmetcp {} disable` or `remove` instead",
            body.host_nqn,
            S::VERB
        )));
    }
    entry.set_volumes(Some(next));

    let row = S::row(entry);
    save::<S>(&path, &file)?;
    emit_sweep_audits::<S>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        &op::<S>("revoke"),
        json!({ "host_nqn": body.host_nqn, "volumes_removed": removed }),
    );
    Ok((StatusCode::OK, Json(row)))
}

/// Collect response rows for `list`. The per-surface `list` handler
/// wraps these in its own response envelope (`{ "psks": [...] }` /
/// `{ "dhchap": [...] }`).
pub fn collect_rows<S: Surface>(state: &AdminState) -> Result<Vec<S::Row>, ApiError> {
    let (_, file) = load::<S>(state)?;
    Ok(file.entries().iter().map(S::row).collect())
}

// ---------- shared helpers ----------

pub(crate) fn load<S: Surface>(state: &AdminState) -> Result<(PathBuf, S::File), ApiError> {
    let path = S::path(state);
    let file = S::File::from_path(&path)
        .map_err(|e| ApiError::internal(format!("loading {}: {}", path.display(), e)))?;
    Ok((path, file))
}

pub(crate) fn save<S: Surface>(path: &Path, file: &S::File) -> Result<(), ApiError> {
    file.to_path(path)
        .map_err(|e| ApiError::internal(format!("saving {}: {}", path.display(), e)))
}

fn find_mut<'f, S: Surface>(
    file: &'f mut S::File,
    host_nqn: &str,
) -> Result<&'f mut EntryOf<S>, ApiError> {
    file.entries_mut()
        .iter_mut()
        .find(|e| e.host_nqn() == host_nqn)
        .ok_or_else(|| not_found::<S>(host_nqn))
}

fn not_found<S: Surface>(host_nqn: &str) -> ApiError {
    ApiError::not_found(format!("{} for host '{}'", S::ENTITY, host_nqn))
}

/// `{PREFIX}.{suffix}` audit op name.
fn op<S: Surface>(suffix: &str) -> String {
    format!("{}.{}", S::AUDIT_PREFIX, suffix)
}

/// Clear every expired previous key, returning the swept host NQNs so
/// the caller can emit one `rotate.commit` audit row per host.
pub(crate) fn sweep_all<S: Surface>(file: &mut S::File) -> Vec<String> {
    let now = Utc::now();
    let mut swept = Vec::new();
    for e in file.entries_mut().iter_mut() {
        if e.sweep_expired(now) {
            swept.push(e.host_nqn().to_string());
        }
    }
    swept
}

pub(crate) fn emit_sweep_audits<S: Surface>(
    state: &AdminState,
    peer: &PeerCred,
    swept: Vec<String>,
) {
    for host_nqn in swept {
        audit(
            state,
            peer,
            &op::<S>("rotate.commit"),
            json!({ "host_nqn": host_nqn, "reason": "grace_expired" }),
        );
    }
}

pub(crate) fn audit(state: &AdminState, peer: &PeerCred, op: &str, params: serde_json::Value) {
    if let Some(chan) = state.audit.as_ref() {
        chan.try_append(
            op,
            AuditActor::cli(peer.audit_descriptor()),
            params,
            AuditResult::Ok,
        );
    }
}

fn validate_host_nqn(s: &str) -> Result<(), ApiError> {
    if s.is_empty() {
        return Err(ApiError::bad_request("host_nqn must not be empty"));
    }
    if s.len() > 223 {
        // NVMe Base §7.9: NQN field width.
        return Err(ApiError::bad_request(
            "host_nqn exceeds 223 bytes (NVMe §7.9)",
        ));
    }
    Ok(())
}

/// Reject unknown volume names at add / grant time so we don't
/// accumulate dangling admission entries.
pub(crate) fn validate_volumes_exist(state: &AdminState, names: &[String]) -> Result<(), ApiError> {
    let mut unknown = Vec::new();
    for n in names {
        if state.registry.get_by_name(n).is_none() {
            unknown.push(n.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::bad_request(format!(
            "unknown volume name(s): {}",
            unknown.join(", ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_host_nqn_rejects_empty_and_overlong() {
        assert!(validate_host_nqn("").is_err());
        assert!(validate_host_nqn(&"a".repeat(224)).is_err());
        assert!(validate_host_nqn("nqn.2025-01.example:host").is_ok());
    }

    #[test]
    fn add_request_omits_ctrl_key_by_default() {
        let req: AddRequest =
            serde_json::from_str(r#"{"host_nqn":"nqn.h","key":"k","volumes":["v"]}"#).unwrap();
        assert!(req.ctrl_key.is_none());
        assert_eq!(req.volumes.as_deref(), Some(&["v".to_string()][..]));
    }
}
