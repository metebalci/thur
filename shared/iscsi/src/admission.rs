// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Live per-CHAP-user volume admission (VSA).
//!
//! VSA fences each CHAP session to the volume set its user is admitted
//! to (`iscsi-users.json`'s `volumes:` array). Historically that set was
//! snapshotted at login and frozen for the session's lifetime, so an
//! `iscsi users grant USER --volume V` only reached sessions that logged
//! in *after* the grant. That breaks the Kubernetes CSI per-node CHAP
//! model: all VSA volumes share one target IQN, so a node holds a single
//! iSCSI session and every volume it mounts must appear on that one
//! session — including volumes granted while the session is already up.
//!
//! [`AdmissionView`] decouples the visible-LUN set from the login
//! snapshot. It is a small in-memory map (`username` → admitted volume
//! names) shared between two sides:
//!
//! - the iSCSI FFP loop reads the *current* set for a session's
//!   authenticated user on every command (cheap: an [`RwLock`] read plus
//!   an [`Arc`] clone — no per-command file I/O), and
//! - the admin `iscsi users {add,grant,revoke,remove}` handlers mutate it
//!   in lockstep with the on-disk `iscsi-users.json`.
//!
//! The daemon seeds it from `iscsi-users.json` at boot and the admin
//! handlers keep it current, so the map always covers every user that
//! could authenticate. VTL has no admission concept and never constructs
//! one; the [`crate::ScsiHandler::live_admission`] hook defaults to
//! `None`, leaving the login snapshot in force.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Shared, mutable map of CHAP username → admitted volume names. Cheap
/// to clone (it's an `Arc` internally via the handles that hold it) and
/// safe to read on the SCSI hot path.
#[derive(Default)]
pub struct AdmissionView {
    inner: RwLock<HashMap<String, Arc<Vec<String>>>>,
}

impl AdmissionView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed (or wholesale replace) the map from an iterator of
    /// `(username, volumes)` pairs — the daemon's boot-time load of
    /// `iscsi-users.json`.
    pub fn seed(&self, users: impl IntoIterator<Item = (String, Vec<String>)>) {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.clear();
        for (user, volumes) in users {
            map.insert(user, Arc::new(volumes));
        }
    }

    /// Set one user's admitted-volume set (used by `add` / `grant` /
    /// `revoke`, which always persist a non-empty set).
    pub fn set(&self, username: &str, volumes: Vec<String>) {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.insert(username.to_string(), Arc::new(volumes));
    }

    /// Drop a user entirely (used by `remove`). A live session of a
    /// dropped user then resolves to the empty set and sees no LUNs.
    pub fn remove(&self, username: &str) {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.remove(username);
    }

    /// Current admitted-volume set for `username`. `None` when the user
    /// is unknown — the caller decides whether that means "see nothing"
    /// (a live CHAP session whose user vanished) or a fall-through.
    pub fn get(&self, username: &str) -> Option<Arc<Vec<String>>> {
        let map = self.inner.read().unwrap_or_else(|p| p.into_inner());
        map.get(username).map(Arc::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_then_get_returns_the_admitted_set() {
        let view = AdmissionView::new();
        view.seed([
            (
                "alice".to_string(),
                vec!["v1".to_string(), "v2".to_string()],
            ),
            ("bob".to_string(), vec!["v3".to_string()]),
        ]);
        assert_eq!(
            view.get("alice").as_deref().map(|v| v.as_slice()),
            Some(&["v1".to_string(), "v2".to_string()][..])
        );
        assert_eq!(
            view.get("bob").as_deref().map(|v| v.as_slice()),
            Some(&["v3".to_string()][..])
        );
        assert!(view.get("carol").is_none());
    }

    #[test]
    fn set_overwrites_and_grant_revoke_are_visible_immediately() {
        let view = AdmissionView::new();
        view.set("alice", vec!["v1".to_string()]);
        // grant adds v2
        view.set("alice", vec!["v1".to_string(), "v2".to_string()]);
        assert_eq!(view.get("alice").unwrap().len(), 2);
        // revoke drops back to v2
        view.set("alice", vec!["v2".to_string()]);
        assert_eq!(view.get("alice").as_deref().unwrap(), &["v2".to_string()]);
    }

    #[test]
    fn remove_drops_the_user() {
        let view = AdmissionView::new();
        view.set("alice", vec!["v1".to_string()]);
        view.remove("alice");
        assert!(view.get("alice").is_none());
    }

    #[test]
    fn seed_replaces_the_whole_map() {
        let view = AdmissionView::new();
        view.set("stale", vec!["old".to_string()]);
        view.seed([("fresh".to_string(), vec!["new".to_string()])]);
        assert!(view.get("stale").is_none());
        assert_eq!(view.get("fresh").as_deref().unwrap(), &["new".to_string()]);
    }
}
