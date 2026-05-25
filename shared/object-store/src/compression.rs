// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Pluggable lossless compression for the two Thur VTL compression
//! layers:
//!   1. **Drive-side** (per-block, host-visible via Mode Page 0x0F DCE).
//!      Default: LZ4. The closest off-the-shelf algorithm to what real
//!      LTO drives do (SLDC: streaming sliding-window LZ77, no entropy
//!      coding) — minimal per-block setup overhead, ~3 GiB/s/core, no
//!      compression-level knob to tune.
//!   2. **Cloud-side** (per-chunk, on upload). Default: Zstd. Chunks
//!      are 1-128 MiB so the heavier algorithm pays for itself.
//!
//! Both layers are pluggable. The chosen algorithm's name is recorded
//! in the manifest (`BlockIndex.compression`, `ChunkMeta.compression`)
//! so we can change defaults — or add new algorithms like SLDC — without
//! breaking existing tapes. `None` (the variant) marks an uncompressed
//! block / chunk.
//!
//! Compress-then-encrypt order matters: encryption per-block uses a
//! per-block IV, which yields high-entropy ciphertext that doesn't
//! compress; compression must therefore run *first*. See
//! `cartridge.rs::write_data`.

use crate::{ObjectStoreError, Result};
use serde::{Deserialize, Serialize};

/// Algorithm code reported in Mode Page 0x0F (Data Compression). 0x00 is
/// "no algorithm / default"; we keep it because SLDC, LZ4, and zstd all
/// lack SCSI-registered algorithm codes. Real LTO drives report 0x00,
/// 0x01 (DCLZ), or 0x40 (LTO-DC). Reporting 0x00 (vendor-specific) is
/// the honest answer for an emulated VTL.
pub const COMPRESSION_ALGORITHM_DEFAULT: u32 = 0x0000_0000;

/// Default zstd level used by cloud-side compression when zstd is
/// selected. zstd ranges 1..=22; level 3 is the broadly-balanced
/// default. LZ4 has no equivalent knob.
pub const ZSTD_DEFAULT_LEVEL: i32 = 3;

/// Lossless compression algorithm. Recorded in manifest metadata so
/// readers know how to undo it without consulting the daemon's current
/// config.
///
/// Serialized as a lowercase string (`"lz4"`, `"zstd"`, `"sldc"`) so
/// adding a new algorithm is forward-compatible — readers that don't
/// know the name reject the block / chunk loudly rather than silently
/// mis-decompressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgo {
    /// LZ4 (frame format). Drive-side default.
    Lz4,
    /// Zstandard. Cloud-side default; also available drive-side for
    /// callers that want maximum ratio over speed.
    Zstd,
    /// SLDC — ECMA-321 Streaming Lossless Data Compression, the
    /// algorithm real LTO-4+ drives implement in dedicated silicon
    /// (sliding-window LZ77, 1 KiB history, no entropy coding,
    /// inline literal/match opcodes). Reserved in the enum for
    /// forward compatibility — selecting it today raises a
    /// `CompressionError` because we don't yet ship an implementation.
    /// Plumbed through the schema so a future codec can land without
    /// migrating any existing manifests.
    Sldc,
}

impl CompressionAlgo {
    /// Lowercase name as it appears in manifests and config. Useful
    /// for log lines and the `--encoded` CLI output.
    pub fn as_str(&self) -> &'static str {
        match self {
            CompressionAlgo::Lz4 => "lz4",
            CompressionAlgo::Zstd => "zstd",
            CompressionAlgo::Sldc => "sldc",
        }
    }
}

impl std::fmt::Display for CompressionAlgo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// In-memory drive compression state — runtime, drive-side. Mirrors the
/// `DriveEncryptionState` pattern: lives for as long as the cartridge is
/// loaded (cleared on UNLOAD via `Cartridge::Drop`), set/cleared by MODE
/// SELECT page 0x0F at runtime.
///
/// Real LTO drives carry compression state in volatile drive RAM that
/// resets on power loss; we mirror that by resetting on cartridge
/// unload, which is the closest analogue Thur VTL has.
///
/// `dce` (Data Compression Enable) controls compression on writes;
/// decompression on reads is per-block (driven by the manifest's
/// `compression` field), independent of the current `dce`. That matches
/// real drives — toggling DCE off doesn't make existing compressed
/// blocks unreadable.
#[derive(Debug, Clone, Copy)]
pub struct DriveCompressionState {
    /// DCE bit from Mode Page 0x0F. True = compress new writes.
    pub dce: bool,
    /// Algorithm used for new writes when `dce` is true. Per-block
    /// `BlockIndex.compression` is what the read path consults; this
    /// field only governs *future* writes.
    pub algorithm: CompressionAlgo,
    /// zstd level — only consulted when `algorithm == Zstd`. LZ4 has
    /// no level knob.
    pub level: i32,
}

impl DriveCompressionState {
    /// Build a state with DCE on at the default algorithm (LZ4).
    pub fn enabled() -> Self {
        Self {
            dce: true,
            algorithm: CompressionAlgo::Lz4,
            level: ZSTD_DEFAULT_LEVEL,
        }
    }

    /// Build a state with DCE on with an explicit algorithm.
    pub fn enabled_with(algorithm: CompressionAlgo) -> Self {
        Self::enabled_with_level(algorithm, ZSTD_DEFAULT_LEVEL)
    }

    /// Build a state with DCE on, an explicit algorithm, and an
    /// explicit zstd level. The level is only consulted when
    /// `algorithm == Zstd`; LZ4 and SLDC ignore it.
    pub fn enabled_with_level(algorithm: CompressionAlgo, level: i32) -> Self {
        Self {
            dce: true,
            algorithm,
            level,
        }
    }

    /// Build a state with DCE off (decompression on read still works
    /// for blocks already on the medium — that's per-block).
    pub fn disabled() -> Self {
        Self {
            dce: false,
            algorithm: CompressionAlgo::Lz4,
            level: ZSTD_DEFAULT_LEVEL,
        }
    }

    /// True iff outgoing writes should be compressed by the drive.
    pub fn compress_on_write(&self) -> bool {
        self.dce
    }
}

impl Default for DriveCompressionState {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Cloud-side compression configuration (per-chunk on upload). When
/// `algorithm` is `None`, chunks are uploaded uncompressed.
#[derive(Debug, Clone, Copy)]
pub struct CompressionConfig {
    pub algorithm: Option<CompressionAlgo>,
    /// zstd level — only consulted when `algorithm == Some(Zstd)`.
    pub level: i32,
}

impl CompressionConfig {
    pub fn new(algorithm: Option<CompressionAlgo>, level: i32) -> Self {
        Self { algorithm, level }
    }

    /// Cloud compression off (uncompressed PUTs).
    pub fn disabled() -> Self {
        Self {
            algorithm: None,
            level: ZSTD_DEFAULT_LEVEL,
        }
    }

    /// True iff PUTs should be compressed.
    pub fn enabled(&self) -> bool {
        self.algorithm.is_some()
    }
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            algorithm: Some(CompressionAlgo::Zstd),
            level: ZSTD_DEFAULT_LEVEL,
        }
    }
}

/// Compress `data` using `algo`. `level` only applies to
/// `CompressionAlgo::Zstd`. Selecting `Sldc` returns
/// `ObjectStoreError::Compression` — the variant is reserved in the
/// enum but we don't yet ship an implementation.
pub fn compress_data(algo: CompressionAlgo, data: &[u8], level: i32) -> Result<Vec<u8>> {
    match algo {
        CompressionAlgo::Lz4 => Ok(lz4_flex::frame::FrameEncoder::new(Vec::new()).pipe(
            |mut enc| {
                use std::io::Write;
                enc.write_all(data)
                    .map_err(|e| ObjectStoreError::Compression(format!("lz4 write: {}", e)))?;
                enc.finish()
                    .map_err(|e| ObjectStoreError::Compression(format!("lz4 finish: {}", e)))
            },
        )?),
        CompressionAlgo::Zstd => zstd::encode_all(data, level)
            .map_err(|e| ObjectStoreError::Compression(format!("zstd encode: {}", e))),
        CompressionAlgo::Sldc => Err(ObjectStoreError::Compression(
            "sldc compression not yet implemented (ECMA-321 codec stub — \
             reserved in the schema, no encoder shipped). Pick lz4 or \
             zstd instead, or wait for the SLDC codec to land."
                .to_string(),
        )),
    }
}

/// Decompress bytes produced by `algo`. Caller must know the algorithm
/// from the manifest (`BlockIndex.compression` / `ChunkMeta.compression`).
/// Selecting `Sldc` returns `ObjectStoreError::Compression` for the same
/// reason as `compress_data`.
pub fn decompress_data(algo: CompressionAlgo, data: &[u8]) -> Result<Vec<u8>> {
    match algo {
        CompressionAlgo::Lz4 => {
            use std::io::Read;
            let mut dec = lz4_flex::frame::FrameDecoder::new(data);
            let mut out = Vec::new();
            dec.read_to_end(&mut out)
                .map_err(|e| ObjectStoreError::Compression(format!("lz4 decode: {}", e)))?;
            Ok(out)
        }
        CompressionAlgo::Zstd => zstd::decode_all(data)
            .map_err(|e| ObjectStoreError::Compression(format!("zstd decode: {}", e))),
        CompressionAlgo::Sldc => Err(ObjectStoreError::Compression(
            "sldc decompression not yet implemented (ECMA-321 codec stub). \
             Any chunks tagged sldc in the manifest cannot be read with \
             this build."
                .to_string(),
        )),
    }
}

/// Tiny helper so the `compress_data` LZ4 arm reads top-down without a
/// `let`-then-`Ok(...)` dance. Local to this file — not a general utility.
trait Pipe: Sized {
    fn pipe<R, F: FnOnce(Self) -> R>(self, f: F) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        b"Hello, World! This is test data that should compress well. ".repeat(100)
    }

    #[test]
    fn lz4_roundtrip() {
        let data = fixture();
        let c = compress_data(CompressionAlgo::Lz4, &data, 0).unwrap();
        assert!(c.len() < data.len(), "lz4 should shrink redundant text");
        let d = decompress_data(CompressionAlgo::Lz4, &c).unwrap();
        assert_eq!(d, data);
    }

    #[test]
    fn zstd_roundtrip() {
        let data = fixture();
        let c = compress_data(CompressionAlgo::Zstd, &data, 3).unwrap();
        assert!(c.len() < data.len(), "zstd should shrink redundant text");
        let d = decompress_data(CompressionAlgo::Zstd, &c).unwrap();
        assert_eq!(d, data);
    }

    #[test]
    fn algos_are_distinct_byte_streams() {
        let data = fixture();
        let lz4 = compress_data(CompressionAlgo::Lz4, &data, 0).unwrap();
        let zstd = compress_data(CompressionAlgo::Zstd, &data, 3).unwrap();
        assert_ne!(lz4, zstd);
    }

    #[test]
    fn config_default_is_zstd_for_cloud() {
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.algorithm, Some(CompressionAlgo::Zstd));
        assert!(cfg.enabled());

        let off = CompressionConfig::disabled();
        assert_eq!(off.algorithm, None);
        assert!(!off.enabled());
    }

    #[test]
    fn drive_default_is_disabled() {
        let st = DriveCompressionState::default();
        assert!(!st.dce);
        assert!(!st.compress_on_write());
    }

    #[test]
    fn drive_enabled_default_algo_is_lz4() {
        let st = DriveCompressionState::enabled();
        assert!(st.dce);
        assert_eq!(st.algorithm, CompressionAlgo::Lz4);
    }

    #[test]
    fn algo_serialize_roundtrip_yaml() {
        let s = serde_yaml::to_string(&CompressionAlgo::Lz4).unwrap();
        assert!(s.trim() == "lz4");
        let back: CompressionAlgo = serde_yaml::from_str("lz4").unwrap();
        assert_eq!(back, CompressionAlgo::Lz4);
        let z: CompressionAlgo = serde_yaml::from_str("zstd").unwrap();
        assert_eq!(z, CompressionAlgo::Zstd);
    }

    #[test]
    fn algo_as_str_and_display_cover_every_variant() {
        assert_eq!(CompressionAlgo::Lz4.as_str(), "lz4");
        assert_eq!(CompressionAlgo::Zstd.as_str(), "zstd");
        assert_eq!(CompressionAlgo::Sldc.as_str(), "sldc");
        assert_eq!(format!("{}", CompressionAlgo::Lz4), "lz4");
        assert_eq!(format!("{}", CompressionAlgo::Zstd), "zstd");
        assert_eq!(format!("{}", CompressionAlgo::Sldc), "sldc");
    }

    #[test]
    fn sldc_compress_and_decompress_are_unimplemented() {
        let c = compress_data(CompressionAlgo::Sldc, b"data", 3);
        assert!(matches!(c, Err(ObjectStoreError::Compression(_))));
        let d = decompress_data(CompressionAlgo::Sldc, b"data");
        assert!(matches!(d, Err(ObjectStoreError::Compression(_))));
    }

    #[test]
    fn compression_config_new_carries_fields() {
        let cfg = CompressionConfig::new(Some(CompressionAlgo::Zstd), 9);
        assert_eq!(cfg.algorithm, Some(CompressionAlgo::Zstd));
        assert_eq!(cfg.level, 9);
        assert!(cfg.enabled());

        let off = CompressionConfig::new(None, 3);
        assert!(!off.enabled());
    }

    #[test]
    fn drive_compression_state_constructors() {
        let with_algo = DriveCompressionState::enabled_with(CompressionAlgo::Zstd);
        assert!(with_algo.dce);
        assert_eq!(with_algo.algorithm, CompressionAlgo::Zstd);
        assert_eq!(with_algo.level, ZSTD_DEFAULT_LEVEL);

        let with_level = DriveCompressionState::enabled_with_level(CompressionAlgo::Zstd, 19);
        assert!(with_level.dce);
        assert_eq!(with_level.algorithm, CompressionAlgo::Zstd);
        assert_eq!(with_level.level, 19);
        assert!(with_level.compress_on_write());

        let off = DriveCompressionState::disabled();
        assert!(!off.dce);
        assert_eq!(off.algorithm, CompressionAlgo::Lz4);
        assert!(!off.compress_on_write());
    }
}
