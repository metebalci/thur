// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

/// Errors surfaced by the Thur VTL core. `#[non_exhaustive]` so adding
/// a new variant in a future revision is not a breaking change for
/// downstream consumers — the iSCSI layer's `error_to_sense` already
/// matches with sensible fallbacks.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SmcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("invalid operation: {0}")]
    InvalidOp(&'static str),

    #[error("verification failed at LBA {lba}: expected {expected}, got {actual}")]
    VerifyFailed {
        lba: u64,
        expected: String,
        actual: String,
    },

    /// Chunk-level integrity failure: bytes fetched from the storage
    /// (refetch on cache miss, prefetcher, or thurvsa page read
    /// fallback) didn't hash to the content-address the caller asked
    /// for. Surfaced by `ChunkPool::insert_verified_bytes`. Distinct
    /// from `VerifyFailed` (LBA-keyed, block-level VERIFY opcode);
    /// this one is chunk-keyed, surfaces a corrupted storage object.
    /// Mapped at the iSCSI layer to CHECK CONDITION + MEDIUM ERROR
    /// (0x03) + ASC/ASCQ 0x11/0x00 ("UNRECOVERED READ ERROR") so
    /// backup software treats it as a per-block read failure rather
    /// than a cartridge-wide write-protect.
    #[error("content hash mismatch: expected {expected}, got {actual}")]
    ContentHashMismatch { expected: String, actual: String },

    /// An index-sidecar record (`blocks-p<N>.idx` / `chunks.idx`)
    /// failed to decode: reserved encryption / compression / location
    /// tag bits, i.e. on-disk corruption of the index file. Distinct
    /// from `InvalidOp` (genuinely illegal requests and internal
    /// invariants) so the SCSI layer can tell the truth: mapped at the
    /// iSCSI layer to CHECK CONDITION + MEDIUM ERROR (0x03) + ASC/ASCQ
    /// 0x11/0x00 ("UNRECOVERED READ ERROR") — the same sense
    /// `ContentHashMismatch` produces for corrupt chunk payloads — so
    /// backup software treats it as a per-block read failure (log,
    /// skip, or fail the cartridge) rather than an unsupported command
    /// (issue #105). Both the READ data path and the SPACE walks
    /// (issue #104) surface it.
    #[error("index corrupt: {0}")]
    IndexCorrupt(&'static str),

    /// A sealed chunk's payload failed codec decode on the read path:
    /// the lz4/zstd frame check caught on-disk bit rot of a cached
    /// chunk file (or of a refetched storage object, one layer before
    /// the BLAKE3 verify). The codec-detected sibling of
    /// `ContentHashMismatch` — the same physical fault, a rotted
    /// chunk payload, detected by a different layer — so it maps to
    /// the same CHECK CONDITION + MEDIUM ERROR (0x03) + ASC/ASCQ
    /// 0x11/0x00 ("UNRECOVERED READ ERROR") at the iSCSI layer
    /// (issue #108). Write-side codec failures keep
    /// `CompressionError` → HARDWARE ERROR: there the codec itself
    /// failed, not the medium.
    #[error("chunk payload corrupt: {0}")]
    ChunkPayloadCorrupt(String),

    #[error("storage error: {0}")]
    ObjectStoreError(String),

    /// HTTP 412 Precondition Failed from a storage op. On the legal-hold
    /// path this is "your AAD identity has the right role but the
    /// container's immutability policy disallows the requested
    /// operation" — distinct from `StorageConflict` (a racing concurrent
    /// change) and from the generic `ObjectStoreError` catch-all (5xx /
    /// throttling / unclassified). Carries the provider's response
    /// body so the operator can read the actual policy decision
    /// without diving into a server log.
    #[error("storage precondition failed (HTTP 412): {0}")]
    StoragePreconditionFailed(String),

    /// HTTP 409 Conflict from a storage op. On the legal-hold path this
    /// usually means an idempotent retry raced another writer (a
    /// concurrent `Set Blob Legal Hold`, container being recreated,
    /// container locked while we PUT). Distinct from
    /// `StoragePreconditionFailed` so an operator can tell "policy says
    /// no" from "try again later."
    #[error("storage conflict (HTTP 409): {0}")]
    StorageConflict(String),

    #[error("invalid session TSIH: {0}")]
    InvalidSession(u16),

    #[error("drive {0} is reserved by another session")]
    DriveReserved(usize),

    #[error("no cartridge loaded in drive {0}")]
    NoCartridgeLoaded(usize),

    #[error("invalid drive ID: {0}")]
    InvalidDrive(usize),

    #[error("cartridge not found: {0}")]
    CartridgeNotFound(String),

    // Tape-specific errors for SCSI sense data mapping
    #[error("end of data reached")]
    EndOfData,

    #[error("beginning of tape (BOT)")]
    BeginningOfTape,

    #[error("filemark detected")]
    FilemarkDetected,

    #[error("end of medium (EOM)")]
    EndOfMedium,

    /// Early-warning latch fired: a successful WRITE / WRITE FILEMARKS
    /// committed at or past the 95% capacity threshold for the first
    /// time on this load. Mapped at the iSCSI layer to CHECK CONDITION
    /// with NoSense, EOM bit, and ASC/ASCQ 0x00/0x02 — the SCSI
    /// convention for "data is on the medium, but we're nearing EOM,
    /// finish this pass and unload." Cleared on rewind, locate to BOM,
    /// erase, or SET CAPACITY.
    #[error("early warning: cartridge approaching end of medium")]
    EarlyWarning,

    #[error("write protected")]
    WriteProtected,

    #[error("invalid field in CDB")]
    InvalidField,

    #[error("invalid command opcode")]
    InvalidCommand,

    #[error("LBA out of range")]
    LbaOutOfRange,

    #[error("invalid element type")]
    InvalidElementType,

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("compression error: {0}")]
    CompressionError(String),

    #[error("configuration error: {0}")]
    ConfigError(String),

    #[error("daemon is running (PID: {0}). Stop the daemon before modifying the library.")]
    DaemonRunning(i32),

    #[error("library configuration error: {0}")]
    LibraryConfig(String),

    // Drive-level encryption errors (LTO Application-Managed Encryption)
    #[error("data decryption error: {0}")]
    DataDecryptionError(&'static str),

    #[error("encryption error: {0}")]
    EncryptionError(String),

    /// WORM cartridge: write attempted at a non-EOD LBA, or
    /// FORMAT MEDIUM / ERASE / ALLOW OVERWRITE attempted. Mapped to
    /// SCSI CHECK CONDITION + DATA PROTECT (key 0x07) + ASC/ASCQ
    /// 0x30/0x0C ("WRITE PROTECTED — WORM MEDIUM") at the iSCSI layer.
    #[error("write protected — WORM medium")]
    WormViolation,

    /// Cartridge under legal hold: any host write opcode is refused.
    /// The held flag is read from the storage sentinel
    /// (`manifests/<barcode>/manifest-latest.json`) once at drive load
    /// and pinned for the duration of the load. Mapped at the iSCSI
    /// layer to CHECK CONDITION + DATA PROTECT (key 0x07) + ASC/ASCQ
    /// 0x27/0x00 ("WRITE PROTECTED") — plain code, not the WORM-specific
    /// 0x30/0x0C, since this is operator-applied preservation rather
    /// than sticky-at-create write-once semantics.
    #[error("write protected — cartridge under legal hold")]
    LegalHoldViolation,

    /// LTO-8+ Encrypt-Only mode (Mode Page 0x10/0x01 WRE bit) is set
    /// and the host attempted a WRITE / WRITE FILEMARKS without an
    /// active drive encryption key (no SECURITY PROTOCOL OUT 0x20 /
    /// SPSP 0x0010 SET DATA ENCRYPTION). Mapped at the iSCSI layer
    /// to CHECK CONDITION + DATA PROTECT (key 0x07) + ASC/ASCQ
    /// 0x74/0x0C ("ENCRYPTION KEY ABSENT"). Recoverable: install a
    /// key via SECURITY PROTOCOL OUT and retry.
    #[error("write protected — encrypt-only mode active without an encryption key")]
    EncryptOnlyKeyAbsent,

    /// LTO-7+ Append-Only / Random/Sequential mode (Mode Page
    /// 0x10/0x01 WRITE MODE = 1) is set and the host attempted a
    /// WRITE / WRITE FILEMARKS at a position other than EOD. Mapped
    /// at the iSCSI layer to CHECK CONDITION + DATA PROTECT (key
    /// 0x07) + ASC/ASCQ 0x27/0x06 ("CONDITIONAL WRITE PROTECT").
    /// Recoverable: SPACE/LOCATE to EOD and retry.
    #[error("write protected — append-only mode requires writes to extend EOD")]
    AppendOnlyMustExtendEod,

    /// Cartridge LTO generation is newer than the drive can read.
    /// Mapped at the iSCSI layer to CHECK CONDITION + ILLEGAL REQUEST
    /// (key 0x05) + ASC/ASCQ 0x30/0x00 ("INCOMPATIBLE MEDIUM
    /// INSTALLED"). Move the tape to an LTO-`cart_gen` (or newer)
    /// drive — the changer keeps the slot intact.
    #[error(
        "incompatible medium: cartridge LTO-{cart_gen} requires an LTO-{cart_gen}+ drive (this drive is LTO-{drive_gen})"
    )]
    IncompatibleMedium { drive_gen: u8, cart_gen: u8 },

    /// Backend does not implement a requested provider-native operation
    /// (today: legal hold against the local backend, or against a
    /// container/bucket that the provider hasn't enabled the relevant
    /// feature on). Surfaced to the operator with a clear refusal.
    #[error("not supported by backend: {0}")]
    NotSupported(String),

    /// Upload backpressure timed out: a chunk-seal would have pushed
    /// the local pool past its hard cap (or under
    /// `disk_cache.disk_free_min_gb`), and waiting on
    /// `upload.backpressure_max_wait_seconds` did not free enough
    /// headroom. Mapped at the SCSI layer to NOT READY +
    /// ASC/ASCQ 0x04/0x07 ("LOGICAL UNIT NOT READY, OPERATION IN
    /// PROGRESS"); backup software (tar/mt, NetBackup, Veeam,
    /// Bacula) treats that as transient and retries.
    ///
    /// The payload lives in `shared_pool::BackpressureError` so the
    /// block side (VSA) can wrap the same struct in its own error
    /// enum without duplicating the format string.
    #[error("{0}")]
    Backpressured(#[from] shared_pool::BackpressureError),

    /// `cartridge migrate --mode=rebind` (verify mode) found objects
    /// missing on the target backend that the cartridge references.
    /// Operator's bucket-replication tool is behind, or the target
    /// backend is the wrong one. List is capped (first 16) to keep
    /// the error payload bounded.
    #[error("rebind target missing {} object(s): {}", keys.len(), keys.join(", "))]
    RebindTargetMissing { keys: Vec<String> },
}

pub type Result<T> = std::result::Result<T, SmcError>;

/// Bridge `shared_object_store::ObjectStoreError` into `SmcError`. Keeps every
/// existing `?` propagation working unchanged after the storage layer
/// was lifted out of core-mediachanger. Mapping is one-to-one against the
/// pre-extraction variants:
///
/// - `ObjectStoreError::Other(msg)` → `SmcError::ObjectStoreError(msg)`
/// - the six retry-classification variants (`Auth` / `Authz` / `NotFound`
///   / `RegionMismatch` / `Network` / `Timeout`) also → `SmcError::ObjectStoreError(msg)`:
///   they exist to drive the object-store retry loop's fail-fast decision,
///   not to carry a distinct SCSI-sense meaning, so they collapse to the
///   generic message variant once they cross into core-mediachanger.
/// - `ObjectStoreError::PreconditionFailed(msg)` → `SmcError::StoragePreconditionFailed(msg)`
/// - `ObjectStoreError::Conflict(msg)` → `SmcError::StorageConflict(msg)`
/// - `ObjectStoreError::NotSupported(msg)` → `SmcError::NotSupported(msg)`
/// - `ObjectStoreError::Compression(msg)` → `SmcError::CompressionError(msg)`
/// - `ObjectStoreError::Io(e)` → `SmcError::Io(e)`
impl From<shared_object_store::ObjectStoreError> for SmcError {
    fn from(e: shared_object_store::ObjectStoreError) -> Self {
        use shared_object_store::ObjectStoreError as O;
        match e {
            O::Other(s)
            | O::Auth(s)
            | O::Authz(s)
            | O::NotFound(s)
            | O::RegionMismatch(s)
            | O::Network(s)
            | O::Timeout(s) => Self::ObjectStoreError(s),
            O::PreconditionFailed(s) => Self::StoragePreconditionFailed(s),
            O::Conflict(s) => Self::StorageConflict(s),
            O::NotSupported(s) => Self::NotSupported(s),
            O::Compression(s) => Self::CompressionError(s),
            O::Io(e) => Self::Io(e),
        }
    }
}

/// Bridge `shared_pool::ChunkPoolError` into `SmcError`. Keeps the
/// existing `ChunkStore::*` `?` propagation working unchanged after
/// the pool primitives were lifted out of core-mediachanger in Step 5
/// Milestone 5.A.3. `HashMismatch` carries chunk-level integrity
/// information — surfaced as `ContentHashMismatch` so the iSCSI
/// sense mapper can map it to MEDIUM ERROR 0x11/0x00.
impl From<shared_pool::ChunkPoolError> for SmcError {
    fn from(e: shared_pool::ChunkPoolError) -> Self {
        match e {
            shared_pool::ChunkPoolError::Io(io) => Self::Io(io),
            shared_pool::ChunkPoolError::HashMismatch { expected, actual } => {
                Self::ContentHashMismatch { expected, actual }
            }
        }
    }
}

/// Bridge `shared_upload_worker::UploadInertError` into `SmcError`.
/// Keeps `Cartridge::upload_chunk_to_storage`'s `?` propagation working
/// after `upload_chunk_inert` moved into the shared crate. The shared
/// error carries either a `ObjectStoreError` or a local IO failure
/// (file read of the chunk's pool path); both fan out to the existing
/// `SmcError` variants.
impl From<shared_upload_worker::UploadInertError> for SmcError {
    fn from(e: shared_upload_worker::UploadInertError) -> Self {
        match e {
            shared_upload_worker::UploadInertError::ObjectStore(c) => c.into(),
            shared_upload_worker::UploadInertError::Io { source, .. } => Self::Io(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_error_routes_to_the_matching_smc_variant() {
        let cases: Vec<(shared_object_store::ObjectStoreError, SmcError)> = vec![
            (
                shared_object_store::ObjectStoreError::Other("x".into()),
                SmcError::ObjectStoreError("x".into()),
            ),
            (
                shared_object_store::ObjectStoreError::PreconditionFailed("p".into()),
                SmcError::StoragePreconditionFailed("p".into()),
            ),
            (
                shared_object_store::ObjectStoreError::Conflict("c".into()),
                SmcError::StorageConflict("c".into()),
            ),
            (
                shared_object_store::ObjectStoreError::NotSupported("n".into()),
                SmcError::NotSupported("n".into()),
            ),
            (
                shared_object_store::ObjectStoreError::Compression("z".into()),
                SmcError::CompressionError("z".into()),
            ),
        ];
        for (input, expected) in cases {
            let got: SmcError = input.into();
            assert_eq!(got.to_string(), expected.to_string());
        }

        // The six retry-classification variants all fold to the generic
        // SmcError::ObjectStoreError, carrying the inner message verbatim —
        // they drive the object-store retry loop, not SCSI sense, so the
        // distinction is intentionally dropped at this boundary.
        for input in [
            shared_object_store::ObjectStoreError::Auth("a".into()),
            shared_object_store::ObjectStoreError::Authz("z".into()),
            shared_object_store::ObjectStoreError::NotFound("nf".into()),
            shared_object_store::ObjectStoreError::RegionMismatch("rm".into()),
            shared_object_store::ObjectStoreError::Network("net".into()),
            shared_object_store::ObjectStoreError::Timeout("to".into()),
        ] {
            // Each variant's inner string is the last `: `-separated field
            // of its Display (e.g. "object store auth: a" -> "a").
            let inner = input.to_string().rsplit(": ").next().unwrap().to_string();
            match SmcError::from(input) {
                SmcError::ObjectStoreError(s) => assert_eq!(s, inner),
                other => unreachable!("expected ObjectStoreError, got {other:?}"),
            }
        }

        let io = shared_object_store::ObjectStoreError::Io(std::io::Error::other("boom"));
        assert!(matches!(SmcError::from(io), SmcError::Io(_)));
    }

    #[test]
    fn chunk_pool_error_io_maps_to_io_variant() {
        let e = shared_pool::ChunkPoolError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing",
        ));
        assert!(matches!(SmcError::from(e), SmcError::Io(_)));
    }

    #[test]
    fn chunk_pool_error_hash_mismatch_maps_to_content_hash_mismatch() {
        let e = shared_pool::ChunkPoolError::HashMismatch {
            expected: "abc".into(),
            actual: "def".into(),
        };
        let got: SmcError = e.into();
        match got {
            SmcError::ContentHashMismatch { expected, actual } => {
                assert_eq!(expected, "abc");
                assert_eq!(actual, "def");
            }
            other => unreachable!("expected ContentHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn upload_inert_error_routes_storage_and_io_arms() {
        let storage = shared_upload_worker::UploadInertError::ObjectStore(
            shared_object_store::ObjectStoreError::Other("svc".into()),
        );
        assert!(matches!(
            SmcError::from(storage),
            SmcError::ObjectStoreError(_)
        ));

        let io = shared_upload_worker::UploadInertError::Io {
            path: "/tmp/chunk.dat".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        assert!(matches!(SmcError::from(io), SmcError::Io(_)));
    }
}
