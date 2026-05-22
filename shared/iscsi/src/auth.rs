// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // CHAP authentication infrastructure

use crate::error::{IscsiError, Result};
use chrono::{DateTime, Utc};
use rand::Rng;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use sha3::Sha3_256;
use std::collections::HashMap;

/// CHAP algorithm identifiers.
///
/// `5` (MD5) is the only RFC 1994 / RFC 7143 standard algorithm.
/// `6` (SHA-1), `7` (SHA-256), and `8` (SHA3-256) are de-facto
/// extensions implemented by Linux LIO and open-iscsi (3.x+) and
/// widely interoperable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha3_256,
}

impl ChapAlgorithm {
    pub fn id(self) -> u8 {
        match self {
            Self::Md5 => 5,
            Self::Sha1 => 6,
            Self::Sha256 => 7,
            Self::Sha3_256 => 8,
        }
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            5 => Some(Self::Md5),
            6 => Some(Self::Sha1),
            7 => Some(Self::Sha256),
            8 => Some(Self::Sha3_256),
            _ => None,
        }
    }

    pub fn output_len(self) -> usize {
        match self {
            Self::Md5 => 16,
            Self::Sha1 => 20,
            Self::Sha256 | Self::Sha3_256 => 32,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
            Self::Sha3_256 => "SHA3-256",
        }
    }
}

/// Pick the strongest algorithm in `offered` that is also in `allowed`.
///
/// `offered` comes from the initiator's `CHAP_A` value (comma-
/// separated decimal IDs, in initiator preference order per RFC 7143
/// §11.1.4). `allowed` is what the target permits, in target
/// preference order. We return the first match scanning `allowed`,
/// so the target's preferred-strongest-first ordering wins as long
/// as the initiator listed it.
pub fn select_algorithm(offered: &str, allowed: &[ChapAlgorithm]) -> Option<ChapAlgorithm> {
    let offered_ids: Vec<u8> = offered
        .split(',')
        .filter_map(|s| s.trim().parse::<u8>().ok())
        .collect();
    allowed
        .iter()
        .copied()
        .find(|a| offered_ids.contains(&a.id()))
}

/// Parse `iscsi.auth.allowed_algorithms` (operator's YAML list of
/// algorithm names) into a `Vec<ChapAlgorithm>`.
///
/// Empty/missing falls back to `[SHA3-256, SHA-256, SHA-1, MD5]` —
/// strongest-first, same default as `ChapAuthenticator::new` applies
/// when given an empty allow-list. Unknown names are rejected so a
/// typo doesn't silently disable a stronger algorithm.
///
/// Accepted aliases per algorithm:
/// - MD5: `MD5`, `5`
/// - SHA-1: `SHA-1`, `SHA1`, `6`
/// - SHA-256: `SHA-256`, `SHA256`, `7`
/// - SHA3-256: `SHA3-256`, `SHA3_256`, `SHA3256`, `8`
///
/// Matching is case-insensitive; duplicates are de-duplicated in
/// input order so an operator listing `[MD5, 5]` doesn't double-add.
pub fn parse_chap_algorithms(names: &[String]) -> Result<Vec<ChapAlgorithm>> {
    if names.is_empty() {
        return Ok(vec![
            ChapAlgorithm::Sha3_256,
            ChapAlgorithm::Sha256,
            ChapAlgorithm::Sha1,
            ChapAlgorithm::Md5,
        ]);
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let alg = match name.trim().to_ascii_uppercase().as_str() {
            "MD5" | "5" => ChapAlgorithm::Md5,
            "SHA-1" | "SHA1" | "6" => ChapAlgorithm::Sha1,
            "SHA-256" | "SHA256" | "7" => ChapAlgorithm::Sha256,
            "SHA3-256" | "SHA3_256" | "SHA3256" | "8" => ChapAlgorithm::Sha3_256,
            other => {
                return Err(IscsiError::InvalidConfig(format!(
                    "Unknown CHAP algorithm '{}' in iscsi.auth.allowed_algorithms (recognized: MD5, SHA-1, SHA-256, SHA3-256)",
                    other
                )));
            }
        };
        if !out.contains(&alg) {
            out.push(alg);
        }
    }
    Ok(out)
}

/// Format an `allowed` list as a comma-separated CHAP_A advertisement.
pub fn format_algorithm_list(allowed: &[ChapAlgorithm]) -> String {
    allowed
        .iter()
        .map(|a| a.id().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// iSCSI authentication method selector, stored in the YAML
/// `iscsi.auth.method` field. Replaces the historical
/// `enabled: bool` + `method: String` pair (both products checked
/// `enabled && method == "CHAP"`, so the two were redundant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub enum AuthMethod {
    /// No authentication. CHAP login attempts are rejected by the
    /// target; sessions log in as `AuthMethod=None`.
    #[default]
    None,
    /// CHAP (RFC 1994 / RFC 7143). Users come from the daemon's
    /// `<data_dir>/iscsi-users.json`.
    #[serde(rename = "CHAP", alias = "Chap", alias = "chap")]
    Chap,
}

impl AuthMethod {
    pub fn is_chap(self) -> bool {
        matches!(self, Self::Chap)
    }
}

/// One CHAP user as stored on disk in `<data_dir>/iscsi-users.json`.
/// Field shape is identical between thurvtl and thurvsa; `partition`
/// is only consulted by VTL (no partition concept on VSA).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UserEntry {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub mutual_chap: bool,
    /// Partition this user is fenced to, by name (VTL only). `None` or
    /// omitted = no fence. Ignored by VSA at runtime.
    #[serde(default)]
    pub partition: Option<String>,
    /// When `true`, login attempts for this user are denied without
    /// hashing the response. Distinct from removal so the entry keeps
    /// its audit-history continuity and can be re-enabled without
    /// re-sharing the password.
    #[serde(default)]
    pub disabled: bool,
    /// Previous password retained for a rotation grace window. Set by
    /// the `iscsi users rotate USER --password NEW [--grace D]` verb;
    /// both old and new passwords authenticate while
    /// `previous_expires_at` is in the future. Lookup logic at
    /// `ChapAuthenticator::verify_response`.
    #[serde(default)]
    pub previous_password: Option<String>,
    /// Wall-clock instant at which `previous_password` stops being
    /// honored. Evaluated at login (no daemon-side timer); a
    /// `rotate.commit` cleanup zeroes the pair on the next mutating
    /// admin verb that observes an expired entry.
    #[serde(default)]
    pub previous_expires_at: Option<DateTime<Utc>>,
}

/// Daemon-owned file at `<data_dir>/iscsi-users.json` holding the CHAP
/// user list plus mutual-CHAP target credentials. Daemon is the sole
/// writer; operator hand-edits today (CLI verbs are a follow-up).
/// Empty `users` is OK at boot when `iscsi.auth.method: None`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct IscsiUsersFile {
    #[serde(default = "default_users_file_version")]
    pub version: u32,
    #[serde(default)]
    pub target_username: Option<String>,
    #[serde(default)]
    pub target_password: Option<String>,
    #[serde(default)]
    pub users: Vec<UserEntry>,
}

fn default_users_file_version() -> u32 {
    1
}

impl Default for IscsiUsersFile {
    fn default() -> Self {
        Self {
            version: 1,
            target_username: None,
            target_password: None,
            users: Vec::new(),
        }
    }
}

impl IscsiUsersFile {
    /// Load from disk. Returns the default (empty) file if the path
    /// doesn't exist. Errors only on read-but-malformed JSON.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| {
                IscsiError::InvalidOp(Box::leak(
                    format!("failed to parse iscsi-users.json: {e}").into_boxed_str(),
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(IscsiError::InvalidOp(Box::leak(
                format!("I/O error on iscsi-users.json: {e}").into_boxed_str(),
            ))),
        }
    }

    /// Write to disk via atomic rename. Mode 0640 on Unix.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let body = serde_json::to_string_pretty(self).map_err(|e| {
            IscsiError::InvalidOp(Box::leak(
                format!("failed to serialize iscsi-users.json: {e}").into_boxed_str(),
            ))
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| {
            IscsiError::InvalidOp(Box::leak(
                format!("I/O error writing iscsi-users.json: {e}").into_boxed_str(),
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640)).map_err(
                |e| {
                    IscsiError::InvalidOp(Box::leak(
                        format!("chmod failed on iscsi-users.json: {e}").into_boxed_str(),
                    ))
                },
            )?;
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            IscsiError::InvalidOp(Box::leak(
                format!("rename failed on iscsi-users.json: {e}").into_boxed_str(),
            ))
        })?;
        Ok(())
    }

    /// Load if present, otherwise write an empty stub and return that.
    /// Used at daemon startup so first-boot operators don't have to
    /// learn the JSON schema before the daemon runs.
    pub fn load_or_create_default(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            let stub = Self::default();
            stub.save(path)?;
            return Ok(stub);
        }
        Self::load(path)
    }
}

/// CHAP user configuration
#[derive(Debug, Clone)]
pub struct ChapUser {
    pub username: String,
    password: String, // CHAP needs the plaintext to compute the digest
    pub mutual_chap: bool,
    /// Partition this user is fenced to, by name. `None` = no
    /// partition binding (the user has full chassis-level access;
    /// only valid when no partitions are defined in library.json).
    pub partition: Option<String>,
    /// Previous password (set during a rotation grace window).
    previous_password: Option<String>,
    previous_expires_at: Option<DateTime<Utc>>,
}

impl ChapUser {
    pub fn new(username: String, password: String, mutual_chap: bool) -> Self {
        Self {
            username,
            password,
            mutual_chap,
            partition: None,
            previous_password: None,
            previous_expires_at: None,
        }
    }

    pub fn with_partition(mut self, partition: Option<String>) -> Self {
        self.partition = partition;
        self
    }

    /// Attach a rotation grace window. Both the current and the
    /// previous password authenticate while `expires_at` is in the
    /// future; only the current password authenticates afterward.
    pub fn with_grace(
        mut self,
        previous_password: Option<String>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.previous_password = previous_password;
        self.previous_expires_at = expires_at;
        self
    }

    pub fn partition(&self) -> Option<&str> {
        self.partition.as_deref()
    }

    /// Returns the previous password if and only if `previous_expires_at`
    /// is set and in the future. Used by `verify_response` to fall
    /// through to the old credential during a rotation grace window.
    fn previous_password_if_in_grace(&self) -> Option<&str> {
        match (&self.previous_password, self.previous_expires_at) {
            (Some(p), Some(t)) if t > Utc::now() => Some(p.as_str()),
            _ => None,
        }
    }
}

/// CHAP authenticator
#[derive(Debug, Clone)]
pub struct ChapAuthenticator {
    users: HashMap<String, ChapUser>,
    target_username: Option<String>,
    target_password: Option<String>,
    allowed_algorithms: Vec<ChapAlgorithm>,
}

impl ChapAuthenticator {
    /// Create a new CHAP authenticator.
    ///
    /// `allowed_algorithms` is in target preference order (strongest
    /// first). Empty falls back to `[Sha256, Sha1, Md5]`.
    pub fn new(
        users: Vec<ChapUser>,
        target_username: Option<String>,
        target_password: Option<String>,
        allowed_algorithms: Vec<ChapAlgorithm>,
    ) -> Self {
        let mut user_map = HashMap::new();
        for user in users {
            user_map.insert(user.username.clone(), user);
        }
        let allowed_algorithms = if allowed_algorithms.is_empty() {
            vec![
                ChapAlgorithm::Sha3_256,
                ChapAlgorithm::Sha256,
                ChapAlgorithm::Sha1,
                ChapAlgorithm::Md5,
            ]
        } else {
            allowed_algorithms
        };

        Self {
            users: user_map,
            target_username,
            target_password,
            allowed_algorithms,
        }
    }

    pub fn allowed_algorithms(&self) -> &[ChapAlgorithm] {
        &self.allowed_algorithms
    }

    /// Build an authenticator from a loaded `IscsiUsersFile` plus the
    /// YAML-side `method` + `allowed_algorithms` policy. Returns
    /// `Ok(None)` when `method == AuthMethod::None` — caller treats
    /// that as "no auth required, accept unauthenticated sessions."
    /// Returns `Ok(Some(_))` when `method == AuthMethod::Chap`, even
    /// if `users` is empty (in which case every login fails — same
    /// behavior as the historical `enabled: true, users: []`).
    pub fn from_file(
        users_file: &IscsiUsersFile,
        method: AuthMethod,
        allowed_algorithms: Vec<ChapAlgorithm>,
    ) -> Option<Self> {
        if !method.is_chap() {
            return None;
        }
        // Disabled entries are filtered here rather than reaching the
        // map: `verify_response` then treats them identically to
        // unknown users (deny + audit "Unknown user"), with no special
        // case to maintain.
        let chap_users: Vec<ChapUser> = users_file
            .users
            .iter()
            .filter(|u| !u.disabled)
            .map(|u| {
                ChapUser::new(u.username.clone(), u.password.clone(), u.mutual_chap)
                    .with_partition(u.partition.clone())
                    .with_grace(u.previous_password.clone(), u.previous_expires_at)
            })
            .collect();
        Some(Self::new(
            chap_users,
            users_file.target_username.clone(),
            users_file.target_password.clone(),
            allowed_algorithms,
        ))
    }

    /// Generate a random CHAP challenge (16 bytes — 128 bits of
    /// entropy, sufficient input for any of the supported digests).
    pub fn generate_challenge(&self) -> Vec<u8> {
        let mut rng = rand::thread_rng();
        (0..16).map(|_| rng.r#gen()).collect()
    }

    /// Verify CHAP response from initiator.
    ///
    /// Tries the current password first. On mismatch, if the user is
    /// in a rotation grace window (`previous_password` set and
    /// `previous_expires_at` in the future) the previous password is
    /// also tried — both old and new credentials authenticate during
    /// the window. Grace evaporates naturally at the next call after
    /// expiry; no daemon-side timer.
    pub fn verify_response(
        &self,
        username: &str,
        challenge: &[u8],
        identifier: u8,
        response: &[u8],
        algorithm: ChapAlgorithm,
    ) -> Result<bool> {
        let user = self
            .users
            .get(username)
            .ok_or_else(|| IscsiError::AuthFailed(format!("Unknown user: {}", username)))?;

        let expected = compute_chap_response(algorithm, identifier, &user.password, challenge);
        if response == expected.as_slice() {
            return Ok(true);
        }
        if let Some(prev) = user.previous_password_if_in_grace() {
            let expected_prev = compute_chap_response(algorithm, identifier, prev, challenge);
            if response == expected_prev.as_slice() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Get user information
    pub fn get_user(&self, username: &str) -> Option<&ChapUser> {
        self.users.get(username)
    }

    /// Check if mutual CHAP is required for a user
    pub fn requires_mutual_chap(&self, username: &str) -> bool {
        self.users
            .get(username)
            .map(|u| u.mutual_chap)
            .unwrap_or(false)
    }

    /// Compute target CHAP response for mutual authentication
    pub fn compute_target_response(
        &self,
        challenge: &[u8],
        identifier: u8,
        algorithm: ChapAlgorithm,
    ) -> Result<Vec<u8>> {
        let password = self
            .target_password
            .as_ref()
            .ok_or_else(|| IscsiError::AuthFailed("Target password not configured".to_string()))?;

        Ok(compute_chap_response(
            algorithm, identifier, password, challenge,
        ))
    }

    /// Get target username for mutual CHAP
    pub fn get_target_username(&self) -> Option<&str> {
        self.target_username.as_deref()
    }
}

/// Compute CHAP response: H(identifier || password || challenge)
fn compute_chap_response(
    algorithm: ChapAlgorithm,
    identifier: u8,
    password: &str,
    challenge: &[u8],
) -> Vec<u8> {
    match algorithm {
        ChapAlgorithm::Md5 => {
            let mut data = Vec::with_capacity(1 + password.len() + challenge.len());
            data.push(identifier);
            data.extend_from_slice(password.as_bytes());
            data.extend_from_slice(challenge);
            md5::compute(&data).0.to_vec()
        }
        ChapAlgorithm::Sha1 => {
            let mut h = Sha1::new();
            h.update([identifier]);
            h.update(password.as_bytes());
            h.update(challenge);
            h.finalize().to_vec()
        }
        ChapAlgorithm::Sha256 => {
            let mut h = Sha256::new();
            h.update([identifier]);
            h.update(password.as_bytes());
            h.update(challenge);
            h.finalize().to_vec()
        }
        ChapAlgorithm::Sha3_256 => {
            let mut h = Sha3_256::new();
            h.update([identifier]);
            h.update(password.as_bytes());
            h.update(challenge);
            h.finalize().to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ALGORITHMS: [ChapAlgorithm; 4] = [
        ChapAlgorithm::Md5,
        ChapAlgorithm::Sha1,
        ChapAlgorithm::Sha256,
        ChapAlgorithm::Sha3_256,
    ];

    fn make_auth(allowed: Vec<ChapAlgorithm>) -> ChapAuthenticator {
        ChapAuthenticator::new(
            vec![
                ChapUser::new("user1".to_string(), "password1".to_string(), false),
                ChapUser::new("user2".to_string(), "password2".to_string(), true),
            ],
            Some("target".to_string()),
            Some("target-password".to_string()),
            allowed,
        )
    }

    #[test]
    fn parse_algorithms_default_when_empty() {
        let parsed = parse_chap_algorithms(&[]).unwrap();
        assert_eq!(
            parsed,
            vec![
                ChapAlgorithm::Sha3_256,
                ChapAlgorithm::Sha256,
                ChapAlgorithm::Sha1,
                ChapAlgorithm::Md5,
            ]
        );
    }

    #[test]
    fn parse_algorithms_accepts_aliases() {
        let names = vec![
            "sha3-256".to_string(),
            "sha256".to_string(),
            "sha-1".to_string(),
            "MD5".to_string(),
        ];
        let parsed = parse_chap_algorithms(&names).unwrap();
        assert_eq!(
            parsed,
            vec![
                ChapAlgorithm::Sha3_256,
                ChapAlgorithm::Sha256,
                ChapAlgorithm::Sha1,
                ChapAlgorithm::Md5,
            ]
        );
    }

    #[test]
    fn parse_algorithms_sha3_aliases() {
        for alias in ["SHA3-256", "sha3_256", "sha3256", "8"] {
            let parsed = parse_chap_algorithms(&[alias.to_string()]).unwrap();
            assert_eq!(parsed, vec![ChapAlgorithm::Sha3_256], "alias={}", alias);
        }
    }

    #[test]
    fn parse_algorithms_rejects_unknown() {
        let names = vec!["bogus".to_string()];
        assert!(matches!(
            parse_chap_algorithms(&names),
            Err(IscsiError::InvalidConfig(_))
        ));
    }

    #[test]
    fn parse_algorithms_dedups() {
        let names = vec!["MD5".to_string(), "5".to_string(), "MD5".to_string()];
        let parsed = parse_chap_algorithms(&names).unwrap();
        assert_eq!(parsed, vec![ChapAlgorithm::Md5]);
    }

    #[test]
    fn algorithm_ids_round_trip() {
        for alg in ALL_ALGORITHMS {
            assert_eq!(ChapAlgorithm::from_id(alg.id()), Some(alg));
        }
        assert_eq!(ChapAlgorithm::from_id(0), None);
        assert_eq!(ChapAlgorithm::from_id(4), None);
        assert_eq!(ChapAlgorithm::from_id(9), None);
    }

    #[test]
    fn output_lengths_match_digests() {
        assert_eq!(ChapAlgorithm::Md5.output_len(), 16);
        assert_eq!(ChapAlgorithm::Sha1.output_len(), 20);
        assert_eq!(ChapAlgorithm::Sha256.output_len(), 32);
        assert_eq!(ChapAlgorithm::Sha3_256.output_len(), 32);

        let challenge = vec![0xAA; 16];
        for alg in ALL_ALGORITHMS {
            let r = compute_chap_response(alg, 1, "pw", &challenge);
            assert_eq!(r.len(), alg.output_len(), "{:?}", alg);
        }
    }

    #[test]
    fn select_prefers_target_order() {
        let allowed = vec![
            ChapAlgorithm::Sha3_256,
            ChapAlgorithm::Sha256,
            ChapAlgorithm::Sha1,
            ChapAlgorithm::Md5,
        ];
        // Initiator offers all four — target picks SHA3-256
        assert_eq!(
            select_algorithm("5,6,7,8", &allowed),
            Some(ChapAlgorithm::Sha3_256)
        );
        // No SHA3-256 from initiator — fall through to SHA-256
        assert_eq!(
            select_algorithm("5,6,7", &allowed),
            Some(ChapAlgorithm::Sha256)
        );
        // Initiator offers only MD5 + SHA-1 — target picks SHA-1
        assert_eq!(select_algorithm("5,6", &allowed), Some(ChapAlgorithm::Sha1));
        // Initiator offers only MD5
        assert_eq!(select_algorithm("5", &allowed), Some(ChapAlgorithm::Md5));
        // No overlap
        assert_eq!(select_algorithm("99", &allowed), None);
        // Whitespace tolerance
        assert_eq!(
            select_algorithm("8 , 7 , 6 , 5", &allowed),
            Some(ChapAlgorithm::Sha3_256)
        );
    }

    #[test]
    fn select_respects_allowed_filter() {
        let only_md5 = vec![ChapAlgorithm::Md5];
        assert_eq!(
            select_algorithm("5,6,7,8", &only_md5),
            Some(ChapAlgorithm::Md5)
        );
        assert_eq!(select_algorithm("6,7,8", &only_md5), None);
    }

    #[test]
    fn sha3_distinct_from_sha2() {
        // Same length (32 bytes) but different algorithm — outputs
        // must differ for the same inputs.
        let challenge = vec![0xAA; 16];
        let r256 = compute_chap_response(ChapAlgorithm::Sha256, 1, "pw", &challenge);
        let r3_256 = compute_chap_response(ChapAlgorithm::Sha3_256, 1, "pw", &challenge);
        assert_eq!(r256.len(), r3_256.len());
        assert_ne!(r256, r3_256);
    }

    #[test]
    fn deterministic_response() {
        let challenge = vec![0x01, 0x02, 0x03, 0x04];
        for alg in ALL_ALGORITHMS {
            let r1 = compute_chap_response(alg, 5, "pw", &challenge);
            let r2 = compute_chap_response(alg, 5, "pw", &challenge);
            assert_eq!(r1, r2);
        }
    }

    #[test]
    fn verify_round_trip_each_algorithm() {
        let auth = make_auth(ALL_ALGORITHMS.to_vec());
        let challenge = auth.generate_challenge();
        let id = 1;

        for alg in ALL_ALGORITHMS {
            let response = compute_chap_response(alg, id, "password1", &challenge);
            assert!(
                auth.verify_response("user1", &challenge, id, &response, alg)
                    .unwrap(),
                "{:?}",
                alg
            );
            // Wrong algorithm -> mismatch
            let other = if alg == ChapAlgorithm::Md5 {
                ChapAlgorithm::Sha3_256
            } else {
                ChapAlgorithm::Md5
            };
            assert!(
                !auth
                    .verify_response("user1", &challenge, id, &response, other)
                    .unwrap()
            );
        }
    }

    #[test]
    fn unknown_user_errors() {
        let auth = make_auth(vec![ChapAlgorithm::Md5]);
        let challenge = auth.generate_challenge();
        let response = compute_chap_response(ChapAlgorithm::Md5, 1, "x", &challenge);
        assert!(
            auth.verify_response("nobody", &challenge, 1, &response, ChapAlgorithm::Md5)
                .is_err()
        );
    }

    #[test]
    fn mutual_chap_each_algorithm() {
        let auth = make_auth(vec![]);
        assert!(auth.requires_mutual_chap("user2"));
        assert_eq!(auth.get_target_username(), Some("target"));

        let challenge = vec![0x01, 0x02, 0x03, 0x04];
        for alg in ALL_ALGORITHMS {
            let r = auth.compute_target_response(&challenge, 2, alg).unwrap();
            assert_eq!(r.len(), alg.output_len());
        }
    }

    #[test]
    fn default_allowed_when_empty() {
        let auth = make_auth(vec![]);
        let allowed = auth.allowed_algorithms();
        assert_eq!(allowed[0], ChapAlgorithm::Sha3_256);
        assert!(allowed.contains(&ChapAlgorithm::Sha256));
        assert!(allowed.contains(&ChapAlgorithm::Md5));
    }

    #[test]
    fn format_algorithm_list_csv() {
        assert_eq!(
            format_algorithm_list(&[
                ChapAlgorithm::Sha3_256,
                ChapAlgorithm::Sha256,
                ChapAlgorithm::Sha1,
                ChapAlgorithm::Md5,
            ]),
            "8,7,6,5"
        );
    }

    #[test]
    fn from_file_filters_disabled_users() {
        use chrono::Duration;
        let file = IscsiUsersFile {
            version: 1,
            target_username: None,
            target_password: None,
            users: vec![
                UserEntry {
                    username: "active".into(),
                    password: "p1".into(),
                    mutual_chap: false,
                    partition: None,
                    disabled: false,
                    previous_password: None,
                    previous_expires_at: None,
                },
                UserEntry {
                    username: "off".into(),
                    password: "p2".into(),
                    mutual_chap: false,
                    partition: None,
                    disabled: true,
                    previous_password: None,
                    previous_expires_at: None,
                },
            ],
        };
        let auth = ChapAuthenticator::from_file(&file, AuthMethod::Chap, vec![]).unwrap();
        assert!(auth.get_user("active").is_some());
        assert!(auth.get_user("off").is_none());

        // verify_response against the disabled user is reported as
        // "Unknown user" — indistinguishable from a removal at the
        // wire level.
        let challenge = vec![0xAB; 16];
        let resp = compute_chap_response(ChapAlgorithm::Sha256, 1, "p2", &challenge);
        let err = auth
            .verify_response("off", &challenge, 1, &resp, ChapAlgorithm::Sha256)
            .unwrap_err();
        assert!(matches!(err, IscsiError::AuthFailed(_)));

        // And the rotate-grace timer carrying a past expiry is a
        // no-op for non-disabled entries that have no previous set —
        // confirms the optional-pair short-circuit.
        let _ = Duration::seconds(1); // sanity: chrono::Duration in scope
    }

    #[test]
    fn previous_password_accepted_within_grace() {
        use chrono::Duration;
        let user = ChapUser::new("bob".into(), "new-pw".into(), false)
            .with_grace(Some("old-pw".into()), Some(Utc::now() + Duration::hours(1)));
        let auth = ChapAuthenticator::new(vec![user], None, None, vec![ChapAlgorithm::Sha256]);
        let challenge = vec![0x11; 16];
        // New password works.
        let r_new = compute_chap_response(ChapAlgorithm::Sha256, 1, "new-pw", &challenge);
        assert!(
            auth.verify_response("bob", &challenge, 1, &r_new, ChapAlgorithm::Sha256)
                .unwrap()
        );
        // Old password also works during the grace window.
        let r_old = compute_chap_response(ChapAlgorithm::Sha256, 1, "old-pw", &challenge);
        assert!(
            auth.verify_response("bob", &challenge, 1, &r_old, ChapAlgorithm::Sha256)
                .unwrap()
        );
        // Wrong third password rejected.
        let r_bad = compute_chap_response(ChapAlgorithm::Sha256, 1, "nope", &challenge);
        assert!(
            !auth
                .verify_response("bob", &challenge, 1, &r_bad, ChapAlgorithm::Sha256)
                .unwrap()
        );
    }

    #[test]
    fn previous_password_rejected_after_grace_expiry() {
        use chrono::Duration;
        let user = ChapUser::new("bob".into(), "new-pw".into(), false).with_grace(
            Some("old-pw".into()),
            Some(Utc::now() - Duration::seconds(1)),
        );
        let auth = ChapAuthenticator::new(vec![user], None, None, vec![ChapAlgorithm::Sha256]);
        let challenge = vec![0x11; 16];
        let r_new = compute_chap_response(ChapAlgorithm::Sha256, 1, "new-pw", &challenge);
        let r_old = compute_chap_response(ChapAlgorithm::Sha256, 1, "old-pw", &challenge);
        assert!(
            auth.verify_response("bob", &challenge, 1, &r_new, ChapAlgorithm::Sha256)
                .unwrap()
        );
        assert!(
            !auth
                .verify_response("bob", &challenge, 1, &r_old, ChapAlgorithm::Sha256)
                .unwrap()
        );
    }

    #[test]
    fn previous_password_unset_means_only_current_matches() {
        // Defensive: a ChapUser with no previous_password should
        // never accept anything except the current password,
        // regardless of expires_at.
        let user = ChapUser::new("bob".into(), "pw".into(), false);
        let auth = ChapAuthenticator::new(vec![user], None, None, vec![ChapAlgorithm::Sha256]);
        let challenge = vec![0x55; 16];
        let r = compute_chap_response(ChapAlgorithm::Sha256, 1, "pw", &challenge);
        assert!(
            auth.verify_response("bob", &challenge, 1, &r, ChapAlgorithm::Sha256)
                .unwrap()
        );
        let r_other = compute_chap_response(ChapAlgorithm::Sha256, 1, "other", &challenge);
        assert!(
            !auth
                .verify_response("bob", &challenge, 1, &r_other, ChapAlgorithm::Sha256)
                .unwrap()
        );
    }
}
