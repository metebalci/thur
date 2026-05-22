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
