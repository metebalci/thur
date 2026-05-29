//! Shared compression-tag codec for the on-disk index records.
//!
//! Both the chunk index (`chunk_index.rs`) and the block index
//! (`block_index.rs`) store a compression algorithm in the same 3-bit
//! sub-field of their flags byte (bits 4-6), with the same numeric tag
//! assignments. The enum + its conversions + the pack/unpack of that
//! sub-field used to be copy-pasted into both files; they live here so
//! the numeric assignments can't drift.
//!
//! The surrounding flag layout differs between the two records (chunk:
//! hash_present / uploaded / location; block: filemark / encryption),
//! so only the compression sub-field is shared — the other bits stay in
//! each codec.

use shared_object_store::compression::CompressionAlgo;

/// Mask for the 3-bit compression sub-field of a flags byte (bits 4-6).
pub(crate) const FLAG_COMP_MASK: u8 = 0b0111_0000;
/// Right-shift to move the compression sub-field down to bit 0.
pub(crate) const FLAG_COMP_SHIFT: u8 = 4;

/// Compression algorithm tag stored in the 3-bit compression field of
/// `flags`. Local to this codec so `compression::CompressionAlgo`
/// doesn't need a stable numeric tag of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CompressionTag {
    None = 0,
    Lz4 = 1,
    Zstd = 2,
    Sldc = 3,
    // 4..=7 reserved
}

impl CompressionTag {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            2 => Some(Self::Zstd),
            3 => Some(Self::Sldc),
            _ => None,
        }
    }
    fn from_algo(algo: Option<CompressionAlgo>) -> Self {
        match algo {
            None => Self::None,
            Some(CompressionAlgo::Lz4) => Self::Lz4,
            Some(CompressionAlgo::Zstd) => Self::Zstd,
            Some(CompressionAlgo::Sldc) => Self::Sldc,
        }
    }
    fn to_algo(self) -> Option<CompressionAlgo> {
        match self {
            Self::None => None,
            Self::Lz4 => Some(CompressionAlgo::Lz4),
            Self::Zstd => Some(CompressionAlgo::Zstd),
            Self::Sldc => Some(CompressionAlgo::Sldc),
        }
    }
}

/// Pack a compression algorithm into the compression sub-field, masked
/// and shifted ready to OR into a flags byte.
pub(crate) fn pack_compression(algo: Option<CompressionAlgo>) -> u8 {
    ((CompressionTag::from_algo(algo) as u8) << FLAG_COMP_SHIFT) & FLAG_COMP_MASK
}

/// Decode the compression sub-field of a flags byte. Returns `None`
/// when the tag is in the reserved 4..=7 range so the caller can raise
/// its own format error; otherwise `Some(algo)`.
pub(crate) fn unpack_compression(flags: u8) -> Option<Option<CompressionAlgo>> {
    let bits = (flags & FLAG_COMP_MASK) >> FLAG_COMP_SHIFT;
    CompressionTag::from_u8(bits).map(CompressionTag::to_algo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_every_algo() {
        for algo in [
            None,
            Some(CompressionAlgo::Lz4),
            Some(CompressionAlgo::Zstd),
            Some(CompressionAlgo::Sldc),
        ] {
            let packed = pack_compression(algo);
            assert_eq!(packed & !FLAG_COMP_MASK, 0, "packed bits stay in sub-field");
            assert_eq!(unpack_compression(packed), Some(algo));
        }
    }

    #[test]
    fn reserved_tag_is_rejected() {
        // Tag 4 (reserved) shifted into the sub-field.
        let flags = 4u8 << FLAG_COMP_SHIFT;
        assert_eq!(unpack_compression(flags), None);
    }
}
