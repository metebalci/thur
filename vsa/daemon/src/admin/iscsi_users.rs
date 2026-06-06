// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thin adapter wiring VSA's [`AdminState`] into the shared
//! `iscsi-users.json` admin handlers (`shared_admin_iscsi`). All
//! business logic, wire types, audit op names and the `ApiError`
//! type live in `shared-admin-iscsi`; this module just plumbs
//! `data_dir` + `audit` through the [`IscsiUsersState`] trait and
//! re-exports the handlers so the router wiring in
//! `crate::admin::mod` stays unchanged.
//!
//! The mutual-CHAP target verbs live in [`super::iscsi_target`] for
//! VSA (historical split); they call into the same shared
//! `users_path` helper so the wire shape stays in sync.

use std::path::Path;

use shared_admin_iscsi::IscsiUsersState;
use shared_audit::AuditChannel;

use super::handlers::AdminState;

impl IscsiUsersState for AdminState {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn audit_channel(&self) -> Option<&AuditChannel> {
        self.audit.as_ref()
    }

    /// VSA dynamic admission (issue #15): keep the in-memory
    /// [`shared_iscsi::AdmissionView`] in lockstep with the just-saved
    /// `iscsi-users.json` so a CHAP session that is already connected
    /// sees a grant / revoke take effect on its next command, then fan
    /// a REPORTED LUNS DATA HAS CHANGED Unit Attention to that user's
    /// live sessions so the initiator re-reads REPORT LUNS.
    ///
    /// `volumes = None` is the `remove` case: drop the user from the
    /// view (its sessions then see nothing) and skip the UA — there is
    /// no surviving LUN to raise it against.
    fn on_admission_changed(&self, username: &str, volumes: Option<&[String]>) {
        apply_admission_change(
            &self.admission,
            &self.sessions,
            self.ua_tracker.as_deref(),
            &self.registry,
            username,
            volumes,
        );
    }
}

/// Update the live admission view for `username` and, for a grant /
/// revoke on a CHAP user with connected sessions, fan a REPORTED LUNS
/// DATA HAS CHANGED Unit Attention to those sessions on the LUNs they
/// now see. Factored out of [`AdminState::on_admission_changed`] so it
/// can be unit-tested without standing up a full [`AdminState`].
///
/// `volumes = None` is the `remove` case: drop the user from the view
/// (its sessions then see nothing) and raise no UA — there is no
/// surviving LUN to raise it against.
fn apply_admission_change(
    admission: &shared_iscsi::AdmissionView,
    sessions: &shared_iscsi::session::SessionManager,
    ua: Option<&shared_iscsi::unit_attention::UnitAttentionTracker>,
    registry: &crate::registry::VolumeRegistry,
    username: &str,
    volumes: Option<&[String]>,
) {
    let Some(vols) = volumes else {
        admission.remove(username);
        return;
    };
    admission.set(username, vols.to_vec());

    // Raise the UA on the LUNs the user is admitted to after the change
    // — the LUNs the host issues commands to, which pop the UA on the
    // next op and trigger a re-enumeration. A grant's set is a superset
    // of the previously-visible LUNs, so this covers them; a revoke's
    // set is the still-visible remainder.
    let Some(ua) = ua else {
        return;
    };
    let tsihs = sessions.tsihs_for_user(username);
    if tsihs.is_empty() {
        return;
    }
    let luns: Vec<u8> = registry
        .entries()
        .into_iter()
        .filter(|(_, c)| vols.iter().any(|n| n == &c.manifest().name))
        .filter_map(|(lun, _)| u8::try_from(lun).ok())
        .collect();
    for tsih in tsihs {
        for &lun in &luns {
            ua.add_ua(
                tsih,
                lun,
                shared_iscsi::unit_attention::UnitAttentionCode::REPORTED_LUNS_DATA_CHANGED,
            );
        }
    }
}

pub use shared_admin_iscsi::{
    AddRequest, ApiError, GrantRequest, ListResponse, NameOnlyRequest, RevokeRequest,
    RotateRequest, UserRow,
};

pub async fn list(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
) -> Result<axum::Json<ListResponse>, ApiError> {
    shared_admin_iscsi::list::<AdminState>(state, peer).await
}

pub async fn add(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<AddRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    // VSA-mandatory: every CHAP user must declare an admission set.
    // Empty / missing `volumes` is refused at the wire — pairs with
    // clap's `required=true` on the CLI side and the daemon-startup
    // filter that drops legacy entries without volumes.
    let names = body
        .volumes
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("at least one --volume required"))?;
    if names.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_admitted_volumes(&state.registry, names)?;
    shared_admin_iscsi::add::<AdminState>(state, peer, body).await
}

pub async fn grant(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<GrantRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    validate_admitted_volumes(&state.registry, &body.volumes)?;
    shared_admin_iscsi::grant::<AdminState>(state, peer, body).await
}

pub async fn revoke(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<RevokeRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    // No volume-exists check on revoke — we want operators to be
    // able to revoke names of volumes that have since been destroyed
    // (dangling admission entries). The shared handler validates
    // that the resulting set is non-empty.
    shared_admin_iscsi::revoke::<AdminState>(state, peer, body).await
}

/// VSA-only pre-flight check: every volume name in the admission
/// list must currently resolve to a registered volume. Rejecting
/// unknown names at add / grant time keeps `iscsi-users.json` from
/// accumulating dead admission entries that point at typos. The
/// daemon's VolumeRegistry is the source of truth, so volumes
/// created later that match a previously-rejected name simply
/// require the operator to re-issue the verb.
fn validate_admitted_volumes(
    registry: &std::sync::Arc<crate::registry::VolumeRegistry>,
    names: &[String],
) -> Result<(), ApiError> {
    let mut unknown = Vec::new();
    for n in names {
        if registry.get_by_name(n).is_none() {
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

pub async fn remove(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::remove::<AdminState>(state, peer, body).await
}

pub async fn disable(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::disable::<AdminState>(state, peer, body).await
}

pub async fn enable(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::enable::<AdminState>(state, peer, body).await
}

pub async fn rotate(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<RotateRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    shared_admin_iscsi::rotate::<AdminState>(state, peer, body).await
}

pub async fn rotate_cancel(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::rotate_cancel::<AdminState>(state, peer, body).await
}

#[cfg(test)]
mod tests {
    use super::apply_admission_change;
    use crate::registry::VolumeRegistry;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_iscsi::AdmissionView;
    use shared_iscsi::session::SessionManager;
    use shared_iscsi::unit_attention::{UnitAttentionCode, UnitAttentionTracker};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn fixture_cache(name: &str, data_dir: &std::path::Path) -> Arc<PageCache> {
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            name.into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(data_dir, name, backend).unwrap());
        PageCache::new(writer)
    }

    #[tokio::test]
    async fn grant_updates_view_and_fans_reported_luns_ua_to_the_users_sessions() {
        let tmp = TempDir::new().unwrap();
        let registry = VolumeRegistry::new();
        registry.register(0, fixture_cache("vol1", tmp.path()).await);
        registry.register(1, fixture_cache("vol2", tmp.path()).await);

        let admission = AdmissionView::new();
        let sessions = SessionManager::new();
        let ua = UnitAttentionTracker::new();

        // alice has a live session; bob has one too (must NOT be touched).
        let t_alice = sessions.create_session([1u8; 6]);
        sessions.set_authenticated_user(t_alice, Some("alice".into()));
        let t_bob = sessions.create_session([2u8; 6]);
        sessions.set_authenticated_user(t_bob, Some("bob".into()));

        // Grant alice vol2 (LUN 1).
        apply_admission_change(
            &admission,
            &sessions,
            Some(&ua),
            &registry,
            "alice",
            Some(&["vol2".to_string()]),
        );

        // View reflects the grant.
        assert_eq!(
            admission.get("alice").as_deref().unwrap(),
            &["vol2".to_string()]
        );
        // UA queued on alice's session for LUN 1 (vol2), with the right code.
        assert_eq!(
            ua.check_and_pop_ua(t_alice, 1),
            Some(UnitAttentionCode::REPORTED_LUNS_DATA_CHANGED)
        );
        // Not on the un-granted LUN 0, nor on bob's session.
        assert!(ua.check_and_pop_ua(t_alice, 0).is_none());
        assert!(ua.check_and_pop_ua(t_bob, 1).is_none());
    }

    #[tokio::test]
    async fn grant_with_no_live_session_updates_view_without_a_ua() {
        let tmp = TempDir::new().unwrap();
        let registry = VolumeRegistry::new();
        registry.register(0, fixture_cache("vol1", tmp.path()).await);

        let admission = AdmissionView::new();
        let sessions = SessionManager::new(); // no sessions
        let ua = UnitAttentionTracker::new();

        apply_admission_change(
            &admission,
            &sessions,
            Some(&ua),
            &registry,
            "freshuser",
            Some(&["vol1".to_string()]),
        );
        // The new user's set is seeded so its first login sees the LUN…
        assert_eq!(
            admission.get("freshuser").as_deref().unwrap(),
            &["vol1".to_string()]
        );
        // …and no UA was raised (nobody connected yet).
        assert!(!ua.has_pending_ua(0, 0));
    }

    #[tokio::test]
    async fn remove_drops_the_user_from_the_view_and_raises_no_ua() {
        let tmp = TempDir::new().unwrap();
        let registry = VolumeRegistry::new();
        registry.register(0, fixture_cache("vol1", tmp.path()).await);

        let admission = AdmissionView::new();
        admission.set("alice", vec!["vol1".to_string()]);
        let sessions = SessionManager::new();
        let t = sessions.create_session([1u8; 6]);
        sessions.set_authenticated_user(t, Some("alice".into()));
        let ua = UnitAttentionTracker::new();

        // remove → volumes = None.
        apply_admission_change(&admission, &sessions, Some(&ua), &registry, "alice", None);
        assert!(admission.get("alice").is_none());
        // No surviving LUN → no UA.
        assert!(ua.check_and_pop_ua(t, 0).is_none());
    }
}
