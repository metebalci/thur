// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // Config infrastructure - some methods unused

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Unified thurvtl configuration
/// This structure matches the daemon's Config but only loads fields needed by iSCSI target
#[allow(dead_code)] // Method load() not used - config constructed inline in main.rs
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IscsiLibraryConfig {
    pub data_dir: String,
    #[serde(default)]
    pub library: Option<LibrarySettings>,
    #[serde(default)]
    pub iscsi: Option<IscsiSettings>,
}

/// Get library settings with defaults
impl IscsiLibraryConfig {
    pub fn library(&self) -> LibrarySettings {
        self.library.clone().unwrap_or_default()
    }

    pub fn iscsi(&self) -> IscsiSettings {
        self.iscsi.clone().unwrap_or_default()
    }
}

// Legacy compatibility wrapper
#[derive(Debug, Clone, Default)]
pub struct IscsiConfig {
    pub iscsi: IscsiSettings,
    pub library: LibrarySettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IscsiSettings {
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    #[serde(default = "default_target_iqn")]
    pub target_iqn: String,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    #[serde(default = "default_session_timeout")]
    pub session_timeout_seconds: u32,
    #[serde(default)]
    pub auth: AuthSettings,
    /// Algorithm the drive uses *when the host turns DCE on* via
    /// MODE SELECT page 0x0F. Sourced from
    /// `drive.compression.algorithm` in the YAML. There is
    /// deliberately no "DCE default" field — real LTO drives ship
    /// DCE off at every cartridge load and the host is the source
    /// of truth for whether a session compresses.
    #[serde(default = "default_drive_compression_algorithm")]
    pub drive_compression_algorithm: core_mediachanger::CompressionAlgo,
    /// Zstd level used when `drive_compression_algorithm == Zstd`.
    /// Ignored for LZ4 / SLDC. Sourced from
    /// `drive.compression.zstd_level` in the YAML.
    #[serde(default = "default_drive_compression_zstd_level")]
    pub drive_compression_zstd_level: i32,
}

fn default_drive_compression_algorithm() -> core_mediachanger::CompressionAlgo {
    core_mediachanger::CompressionAlgo::Lz4
}

fn default_drive_compression_zstd_level() -> i32 {
    core_mediachanger::ZSTD_DEFAULT_LEVEL
}

impl Default for IscsiSettings {
    fn default() -> Self {
        Self {
            listen_address: "0.0.0.0:3260".to_string(),
            target_iqn: "iqn.2025-10.com.metebalci:thurvtl".to_string(),
            max_sessions: 10,
            session_timeout_seconds: 300,
            auth: AuthSettings::default(),
            drive_compression_algorithm: core_mediachanger::CompressionAlgo::Lz4,
            drive_compression_zstd_level: core_mediachanger::ZSTD_DEFAULT_LEVEL,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthSettings {
    #[serde(default)]
    pub method: shared_iscsi::auth::AuthMethod,
    /// CHAP digest algorithms allowed by the target, in preference
    /// order (strongest first). Recognized values are "SHA-256",
    /// "SHA-1", and "MD5".
    #[serde(default = "default_chap_algorithms")]
    pub allowed_algorithms: Vec<String>,
}

fn default_chap_algorithms() -> Vec<String> {
    vec![
        "SHA3-256".to_string(),
        "SHA-256".to_string(),
        "SHA-1".to_string(),
        "MD5".to_string(),
    ]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibrarySettings {
    #[serde(default = "default_num_drives")]
    pub num_drives: u16,
    #[serde(default = "default_num_storage_slots")]
    pub num_storage_slots: u16,
    #[serde(default = "default_num_mail_slots")]
    pub num_mail_slots: u16,
    #[serde(default = "default_lto_generation")]
    pub lto_generation: u8,
}

fn default_num_drives() -> u16 {
    3
}
fn default_num_storage_slots() -> u16 {
    40
}
fn default_num_mail_slots() -> u16 {
    5
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            num_drives: 3,
            num_storage_slots: 40,
            num_mail_slots: 5,
            lto_generation: 8,
        }
    }
}

fn default_listen_address() -> String {
    "0.0.0.0:3260".to_string()
}

fn default_target_iqn() -> String {
    "iqn.2025-10.com.metebalci:thurvtl".to_string()
}

fn default_max_sessions() -> u32 {
    10
}

fn default_session_timeout() -> u32 {
    300
}

fn default_lto_generation() -> u8 {
    8
}

impl IscsiLibraryConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: IscsiLibraryConfig = serde_yaml::from_str(&contents)?;
        Ok(config)
    }

    /// Convert to IscsiConfig for backward compatibility
    pub fn to_iscsi_config(&self) -> IscsiConfig {
        let iscsi = self.iscsi();
        let library = self.library();
        IscsiConfig {
            iscsi: iscsi.clone(),
            library: library.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn iscsi_settings_default_values() {
        let s = IscsiSettings::default();
        assert_eq!(s.listen_address, "0.0.0.0:3260");
        assert_eq!(s.target_iqn, "iqn.2025-10.com.metebalci:thurvtl");
        assert_eq!(s.max_sessions, 10);
        assert_eq!(s.session_timeout_seconds, 300);
        assert_eq!(
            s.drive_compression_algorithm,
            core_mediachanger::CompressionAlgo::Lz4
        );
    }

    #[test]
    fn library_settings_default_values() {
        let l = LibrarySettings::default();
        assert_eq!(l.num_drives, 3);
        assert_eq!(l.num_storage_slots, 40);
        assert_eq!(l.num_mail_slots, 5);
        assert_eq!(l.lto_generation, 8);
    }

    #[test]
    fn default_chap_algorithms_preference_order() {
        let algs = default_chap_algorithms();
        assert_eq!(algs.first().map(String::as_str), Some("SHA3-256"));
        assert!(algs.contains(&"MD5".to_string()));
        assert_eq!(algs.len(), 4);
    }

    #[test]
    fn auth_settings_derived_default_is_empty_algorithms() {
        // The derived `Default` leaves `allowed_algorithms` empty;
        // the `#[serde(default = ...)]` fallback only applies when
        // deserializing an absent field, not to `Default::default()`.
        let a = AuthSettings::default();
        assert!(a.allowed_algorithms.is_empty());
    }

    #[test]
    fn auth_settings_deserialized_without_algorithms_uses_fallback() {
        let a: AuthSettings = serde_yaml::from_str("method: CHAP").expect("parse auth");
        assert_eq!(a.allowed_algorithms, default_chap_algorithms());
    }

    #[test]
    fn minimal_yaml_only_data_dir_falls_back_to_defaults() {
        let yaml = "data_dir: /srv/thur\n";
        let cfg: IscsiLibraryConfig = serde_yaml::from_str(yaml).expect("parse minimal yaml");
        assert_eq!(cfg.data_dir, "/srv/thur");
        assert!(cfg.library.is_none());
        assert!(cfg.iscsi.is_none());
        // Accessors substitute defaults for absent blocks.
        assert_eq!(cfg.library().num_drives, 3);
        assert_eq!(cfg.iscsi().max_sessions, 10);
    }

    #[test]
    fn partial_library_block_serde_defaults_fill_gaps() {
        let yaml = "data_dir: /srv/thur\nlibrary:\n  num_drives: 7\n";
        let cfg: IscsiLibraryConfig = serde_yaml::from_str(yaml).expect("parse partial yaml");
        let lib = cfg.library();
        assert_eq!(lib.num_drives, 7);
        // Untouched fields take the #[serde(default = ...)] fallback.
        assert_eq!(lib.num_storage_slots, 40);
        assert_eq!(lib.num_mail_slots, 5);
        assert_eq!(lib.lto_generation, 8);
    }

    #[test]
    fn iscsi_block_round_trips_through_yaml() {
        let original = IscsiSettings {
            listen_address: "10.0.0.1:3260".to_string(),
            target_iqn: "iqn.2025-10.com.example:vtl".to_string(),
            max_sessions: 64,
            session_timeout_seconds: 120,
            auth: AuthSettings::default(),
            drive_compression_algorithm: core_mediachanger::CompressionAlgo::Lz4,
            drive_compression_zstd_level: core_mediachanger::ZSTD_DEFAULT_LEVEL,
        };
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let back: IscsiSettings = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(back.listen_address, original.listen_address);
        assert_eq!(back.target_iqn, original.target_iqn);
        assert_eq!(back.max_sessions, original.max_sessions);
        assert_eq!(
            back.session_timeout_seconds,
            original.session_timeout_seconds
        );
    }

    #[test]
    fn config_round_trips_through_yaml() {
        let cfg = IscsiLibraryConfig {
            data_dir: "/srv/thur".to_string(),
            library: Some(LibrarySettings::default()),
            iscsi: Some(IscsiSettings::default()),
        };
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        let back: IscsiLibraryConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(back.data_dir, "/srv/thur");
        assert_eq!(back.library().num_drives, 3);
    }

    #[test]
    fn load_reads_config_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("thurvtl.yaml");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(
            f,
            "data_dir: /srv/thur\nlibrary:\n  num_storage_slots: 100\n  num_drives: 5"
        )
        .expect("write");
        let cfg = IscsiLibraryConfig::load(&path).expect("load config");
        assert_eq!(cfg.data_dir, "/srv/thur");
        assert_eq!(cfg.library().num_storage_slots, 100);
        assert_eq!(cfg.library().num_drives, 5);
    }

    #[test]
    fn load_missing_file_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.yaml");
        assert!(IscsiLibraryConfig::load(&path).is_err());
    }

    #[test]
    fn load_malformed_yaml_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "data_dir: [not a string").expect("write");
        assert!(IscsiLibraryConfig::load(&path).is_err());
    }

    #[test]
    fn to_iscsi_config_projects_both_sections() {
        let cfg = IscsiLibraryConfig {
            data_dir: "/srv/thur".to_string(),
            library: None,
            iscsi: None,
        };
        let projected = cfg.to_iscsi_config();
        assert_eq!(projected.iscsi.max_sessions, 10);
        assert_eq!(projected.library.num_drives, 3);
    }

    #[test]
    fn iscsi_config_default_is_all_defaults() {
        let c = IscsiConfig::default();
        assert_eq!(c.iscsi.listen_address, "0.0.0.0:3260");
        assert_eq!(c.library.lto_generation, 8);
    }
}
