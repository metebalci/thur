// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! On-disk admin-password store: `<data_dir>/admin-password.json`,
//! sibling of `iscsi-users.json` / `nvmetcp-psks.json`. Holds only the
//! self-describing Argon2id PHC hash — never the plaintext. Written via
//! atomic rename at mode 0640 (daemon writes it `<product>:<product>`,
//! group-readable, non-world-readable), mirroring the iscsi-users idiom.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Parsed `admin-password.json`. The PHC string is self-describing
/// (algorithm + version + params + salt + hash), so there is no
/// separate salt / params / username field to persist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminPasswordFile {
    /// Argon2id PHC string: `$argon2id$v=19$m=...$<salt>$<hash>`.
    pub phc: String,
    /// When the password was last set (operator-facing context only;
    /// not security-relevant).
    pub updated_at: DateTime<Utc>,
}

/// Canonical on-disk path of the admin-password file for a `data_dir`.
pub fn admin_password_path(data_dir: &Path) -> PathBuf {
    data_dir.join("admin-password.json")
}

impl AdminPasswordFile {
    /// Load from disk. `Ok(None)` when the file is absent (no password
    /// configured — the gate fails closed); `Err` only when the file is
    /// present but unreadable / malformed.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| format!("parsing {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// Write via atomic rename, mode 0640 on Unix.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serializing admin-password.json: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))
                .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_password_path_joins_the_data_dir() {
        let p = admin_password_path(Path::new("/var/lib/thurvtl"));
        assert_eq!(p, Path::new("/var/lib/thurvtl/admin-password.json"));
    }

    #[test]
    fn load_on_a_missing_file_is_none_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        assert!(AdminPasswordFile::load(&path).expect("ok").is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        let file = AdminPasswordFile {
            phc: "$argon2id$v=19$m=19456,t=2,p=1$abc$def".to_string(),
            updated_at: Utc::now(),
        };
        file.save(&path).expect("save");
        let loaded = AdminPasswordFile::load(&path).expect("ok").expect("some");
        assert_eq!(loaded.phc, file.phc);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_mode_0640() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        AdminPasswordFile {
            phc: "$argon2id$v=19$x".to_string(),
            updated_at: Utc::now(),
        }
        .save(&path)
        .expect("save");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn load_on_malformed_json_surfaces_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        std::fs::write(&path, "{not json").unwrap();
        assert!(AdminPasswordFile::load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_overwrites_an_existing_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        AdminPasswordFile {
            phc: "first".to_string(),
            updated_at: Utc::now(),
        }
        .save(&path)
        .expect("first save");
        AdminPasswordFile {
            phc: "second".to_string(),
            updated_at: Utc::now(),
        }
        .save(&path)
        .expect("second save");
        let loaded = AdminPasswordFile::load(&path).expect("ok").expect("some");
        assert_eq!(loaded.phc, "second");
        // No stale temp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
