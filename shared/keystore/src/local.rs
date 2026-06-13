// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Daemon-local on-disk keystore.
//!
//! Layout: `<data_dir>/keys/<wrap_context_hex>.key`, one file per
//! wrapped secret. For per-volume DEKs the context is the volume's
//! own UUID. File contents are 64 hex chars + newline (matches
//! `openssl rand -hex 32`) so operators can inspect, back up, and
//! restore keys with standard text tools. Mode 0600 — only the
//! daemon user can read them.
//!
//! Storing nothing else in the manifest that identifies the key —
//! no path, no key id — means a stolen manifest carries no extra
//! attack surface: the attacker would already need the keystore on
//! disk to do anything with it.
//!
//! Threat model documented in `docs/admin/ENCRYPTION.md` § VSA volume
//! encryption. In short: protects ciphertext in storage buckets +
//! local pool against a bucket leak / cold-disk theft; does **not**
//! protect against a compromised thurvsad host (the daemon
//! has to be able to read the key to write encrypted volumes). KMS /
//! Vault backends in sibling modules address that gap.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use shared_crypto::{KEY_LEN, OsRng, RngCore};

use crate::error::KeyStoreError;
use crate::keystore_backend::{DekSource, KeyStoreBackend, SecretBytes};

/// Subdirectory under `<data_dir>/` that holds every key file.
pub const KEYS_SUBDIR: &str = "keys";

/// Required file mode for keystore entries (owner read+write only).
const KEY_FILE_MODE: u32 = 0o600;

/// Daemon-local on-disk keystore — DEK lives in
/// `<data_dir>/keys/<uuid>.key`, plaintext. Wrap is identity (the
/// returned ciphertext is empty); the manifest's `wrapped_dek` field
/// stays `None` for volumes bound to this backend (see
/// [`KeyStoreBackend::manages_local_blob`]).
#[derive(Debug, Clone)]
pub struct LocalBackend {
    data_dir: Arc<PathBuf>,
}

impl LocalBackend {
    /// Construct a local-backend handle anchored at `<data_dir>`.
    /// The keys subdirectory is created lazily on the first
    /// `generate_and_wrap` / `wrap` call so a daemon that never
    /// creates an encrypted volume doesn't churn the disk.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir: Arc::new(data_dir),
        }
    }

    pub fn data_dir(&self) -> &Path {
        self.data_dir.as_path()
    }

    fn dir(&self) -> PathBuf {
        self.data_dir.join(KEYS_SUBDIR)
    }

    fn path_for(&self, wrap_context: &[u8; 16]) -> PathBuf {
        self.dir()
            .join(format!("{}.key", hex::encode(wrap_context)))
    }

    /// Refuses to overwrite an existing key file — symmetric to
    /// `VolumeManifest::create`'s no-clobber posture.
    fn write_key_file(
        &self,
        wrap_context: &[u8; 16],
        key: &[u8; KEY_LEN],
    ) -> Result<PathBuf, KeyStoreError> {
        let dir = self.dir();
        fs::create_dir_all(&dir)?;
        // Tighten the directory perms — 0700, owner-only.
        let mut dir_perms = fs::metadata(&dir)?.permissions();
        dir_perms.set_mode(0o700);
        fs::set_permissions(&dir, dir_perms)?;

        let final_path = self.path_for(wrap_context);
        if final_path.exists() {
            return Err(KeyStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("key file already exists: {}", final_path.display()),
            )));
        }
        let tmp = dir.join(format!("{}.key.tmp", hex::encode(wrap_context)));
        // A crash between temp-create and rename leaves a stale .tmp at
        // this deterministic name; without clearing it, every later
        // wrap/import for the same context fails AlreadyExists under
        // create_new. Remove it first (best-effort) so create_new still
        // detects a genuine concurrent writer but recovers from crash
        // leftovers (issue #197).
        let _ = fs::remove_file(&tmp);
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(KEY_FILE_MODE)
                .open(&tmp)?;
            // 64 hex chars + newline. Newline keeps `cat` / `less`
            // output tidy; load strips it. `encode_to_slice` only
            // errors on a size mismatch (output buffer ≠ 2*input.len());
            // sized exactly, so a real failure would mean the hex
            // crate broke its contract — surface as an I/O error
            // rather than silent truncation.
            let mut hex_buf = [0u8; KEY_LEN * 2];
            hex::encode_to_slice(key, &mut hex_buf).map_err(|e| {
                KeyStoreError::Io(std::io::Error::other(format!(
                    "hex::encode_to_slice for AES-256 key: {e}"
                )))
            })?;
            f.write_all(&hex_buf)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &final_path)?;
        // The rename's directory entry is not durable until the directory
        // itself is fsynced. For the local backend the sidecar is the
        // ONLY copy of the DEK, so a power loss in the journal-commit
        // window after `volume create` returned could otherwise leave the
        // key — and every byte the initiator already wrote — permanently
        // unreadable (issue #197).
        if let Ok(dir_handle) = fs::File::open(&dir) {
            let _ = dir_handle.sync_all();
        }
        Ok(final_path)
    }

    /// Mode-0o600 verification + parse. Refuses if an operator
    /// chmod'd the file world-readable after the fact — their backup
    /// tool might do this and we'd rather refuse than silently expose.
    fn read_key_file(&self, wrap_context: &[u8; 16]) -> Result<[u8; KEY_LEN], KeyStoreError> {
        let path = self.path_for(wrap_context);
        let meta = fs::metadata(&path)?;
        let mode = meta.permissions().mode() & 0o7777;
        if mode != KEY_FILE_MODE {
            return Err(KeyStoreError::BadPermissions {
                path,
                mode,
                expected: KEY_FILE_MODE,
            });
        }
        let raw = fs::read_to_string(&path)?;
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        if trimmed.len() != KEY_LEN * 2 {
            return Err(KeyStoreError::Malformed {
                path,
                got: trimmed.len(),
            });
        }
        let mut out = [0u8; KEY_LEN];
        hex::decode_to_slice(trimmed, &mut out).map_err(|e| KeyStoreError::InvalidHex(path, e))?;
        Ok(out)
    }

    fn delete_key_file(&self, wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        let path = self.path_for(wrap_context);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl KeyStoreBackend for LocalBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        _source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        // `local` ignores `source` — it has no remote RNG to call.
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key);
        self.write_key_file(wrap_context, &key)?;
        Ok((SecretBytes::new(key), Vec::new()))
    }

    async fn wrap(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &SecretBytes,
    ) -> Result<Vec<u8>, KeyStoreError> {
        self.write_key_file(wrap_context, plaintext.as_bytes())?;
        Ok(Vec::new())
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        _wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let bytes = self.read_key_file(wrap_context)?;
        Ok(SecretBytes::new(bytes))
    }

    async fn forget(&self, wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        self.delete_key_file(wrap_context)
    }

    fn backend_type(&self) -> &'static str {
        "local"
    }

    fn manages_local_blob(&self) -> bool {
        true
    }

    fn wrap_target_fingerprint(&self) -> String {
        // Sidecar lives at `<data_dir>/keys/<uuid>.key`, so the
        // data_dir is the wrap target. Canonicalize when the path
        // exists so `/var/lib/thurvsa` and `/var/lib/thurvsa/` fold
        // together; fall back to the raw path for a brand-new
        // daemon where the directory hasn't been created yet.
        let canonical = std::fs::canonicalize(self.data_dir.as_path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.data_dir.to_string_lossy().into_owned());
        format!("local:{canonical}")
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        // Best-effort: the keys dir might not exist yet (no
        // encrypted volume has been created). Confirm we can create
        // it with the expected perms — the same shape the data path
        // would attempt.
        let dir = self.dir();
        match fs::metadata(&dir) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o7777;
                if mode != 0o700 {
                    // Not fatal — `wrap` re-applies 0700 every time
                    // it writes. Surface as Other so operators see
                    // the drift in startup logs.
                    return Err(KeyStoreError::Other(format!(
                        "keys directory '{}' has mode {:o}, expected 0700",
                        dir.display(),
                        mode
                    )));
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(KeyStoreError::Io(e)),
        }
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_uuid() -> [u8; 16] {
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ]
    }

    fn fixture_key() -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[tokio::test]
    async fn wrap_then_unwrap_round_trips() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        let key = fixture_key();
        let wrapped = backend
            .wrap(&uuid, &SecretBytes::new(key))
            .await
            .expect("wrap");
        assert!(wrapped.is_empty(), "local wrap returns an empty blob");
        let loaded = backend.unwrap(&uuid, &[]).await.expect("unwrap");
        assert_eq!(loaded.as_bytes(), &key);
    }

    #[tokio::test]
    async fn wrap_sets_mode_0600() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        backend
            .wrap(&uuid, &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        let path = backend.path_for(&uuid);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, KEY_FILE_MODE);
    }

    #[tokio::test]
    async fn wrap_refuses_overwrite() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        let key = fixture_key();
        backend
            .wrap(&uuid, &SecretBytes::new(key))
            .await
            .expect("first wrap");
        let err = backend
            .wrap(&uuid, &SecretBytes::new(key))
            .await
            .expect_err("second wrap must fail");
        assert!(matches!(err, KeyStoreError::Io(_)));
    }

    #[tokio::test]
    async fn unwrap_refuses_bad_permissions() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        backend
            .wrap(&uuid, &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        let path = backend.path_for(&uuid);
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&path, p).unwrap();

        let err = backend
            .unwrap(&uuid, &[])
            .await
            .expect_err("unwrap must fail");
        match err {
            KeyStoreError::BadPermissions { mode, expected, .. } => {
                assert_eq!(mode, 0o644);
                assert_eq!(expected, KEY_FILE_MODE);
            }
            other => panic!("expected BadPermissions, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unwrap_refuses_malformed_hex() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        let kdir = backend.dir();
        fs::create_dir_all(&kdir).unwrap();
        let path = backend.path_for(&uuid);
        fs::write(&path, "0".repeat(63)).unwrap();
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(KEY_FILE_MODE);
        fs::set_permissions(&path, p).unwrap();
        let err = backend.unwrap(&uuid, &[]).await.expect_err("must fail");
        assert!(matches!(err, KeyStoreError::Malformed { .. }));
    }

    #[tokio::test]
    async fn unwrap_refuses_non_hex() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        let kdir = backend.dir();
        fs::create_dir_all(&kdir).unwrap();
        let path = backend.path_for(&uuid);
        fs::write(&path, "z".repeat(64)).unwrap();
        let mut p = fs::metadata(&path).unwrap().permissions();
        p.set_mode(KEY_FILE_MODE);
        fs::set_permissions(&path, p).unwrap();
        let err = backend.unwrap(&uuid, &[]).await.expect_err("must fail");
        assert!(matches!(err, KeyStoreError::InvalidHex(_, _)));
    }

    #[tokio::test]
    async fn forget_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        // Missing -> OK.
        backend.forget(&uuid).await.expect("forget missing");
        // After wrap -> OK.
        backend
            .wrap(&uuid, &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        backend.forget(&uuid).await.expect("forget existing");
        // Idempotent second forget -> OK.
        backend.forget(&uuid).await.expect("forget twice");
        let err = backend.unwrap(&uuid, &[]).await.expect_err("must fail");
        assert!(matches!(err, KeyStoreError::Io(_)));
    }

    #[test]
    fn wrap_target_fingerprint_uses_data_dir() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let fp = backend.wrap_target_fingerprint();
        assert!(fp.starts_with("local:"), "got {fp}");
        // Canonical path (TempDir resolves to /tmp/... which already
        // exists, so canonicalize succeeds).
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(fp, format!("local:{}", canonical.display()));
    }

    #[test]
    fn wrap_target_fingerprint_distinguishes_data_dirs() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let ba = LocalBackend::new(a.path().to_path_buf());
        let bb = LocalBackend::new(b.path().to_path_buf());
        assert_ne!(ba.wrap_target_fingerprint(), bb.wrap_target_fingerprint());
    }

    #[test]
    fn wrap_target_fingerprint_folds_equivalent_paths() {
        let dir = TempDir::new().unwrap();
        // Same directory referenced two ways (trailing slash).
        let p1 = dir.path().to_path_buf();
        let p2 = dir.path().join(""); // adds trailing separator
        let f1 = LocalBackend::new(p1).wrap_target_fingerprint();
        let f2 = LocalBackend::new(p2).wrap_target_fingerprint();
        assert_eq!(f1, f2, "trailing-slash variants must fold");
    }

    #[test]
    fn data_dir_returns_anchor_path() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        assert_eq!(backend.data_dir(), dir.path());
    }

    #[tokio::test]
    async fn health_check_ok_on_missing_keys_dir() {
        // Brand-new daemon: no encrypted volume yet, so `keys/` does
        // not exist. health_check treats that as healthy.
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        backend.health_check().await.expect("missing dir is OK");
    }

    #[tokio::test]
    async fn health_check_ok_after_wrap() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        // `wrap` created `keys/` at mode 0700 — health_check passes.
        backend.health_check().await.expect("0700 keys dir is OK");
    }

    #[tokio::test]
    async fn health_check_flags_keys_dir_mode_drift() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        // Operator (or a backup tool) loosened the keys dir perms.
        let kdir = backend.dir();
        let mut p = fs::metadata(&kdir).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(&kdir, p).unwrap();
        let err = backend.health_check().await.expect_err("drift must fail");
        match err {
            KeyStoreError::Other(msg) => assert!(msg.contains("755")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_and_wrap_persists_a_loadable_key() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid = fixture_uuid();
        // `local` ignores DekSource::Backend (no remote RNG).
        let (minted, wrapped) = backend
            .generate_and_wrap(&uuid, DekSource::Backend)
            .await
            .expect("generate");
        assert!(wrapped.is_empty(), "local wrap blob is always empty");
        let loaded = backend.unwrap(&uuid, &[]).await.expect("unwrap");
        assert_eq!(loaded.as_bytes(), minted.as_bytes());
    }

    #[tokio::test]
    async fn backend_type_and_manages_local_blob() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        assert_eq!(backend.backend_type(), "local");
        assert!(backend.manages_local_blob());
    }

    #[tokio::test]
    async fn clone_box_yields_equivalent_backend() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let boxed = backend.clone_box();
        assert_eq!(boxed.backend_type(), "local");
        assert_eq!(
            boxed.wrap_target_fingerprint(),
            backend.wrap_target_fingerprint()
        );
    }

    #[tokio::test]
    async fn unwrap_missing_key_file_is_io_error() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let err = backend
            .unwrap(&fixture_uuid(), &[])
            .await
            .expect_err("no key file");
        assert!(matches!(err, KeyStoreError::Io(_)));
    }

    #[tokio::test]
    async fn generate_returns_unique_keys() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path().to_path_buf());
        let uuid_a = [0x01u8; 16];
        let uuid_b = [0x02u8; 16];
        let (a, _) = backend
            .generate_and_wrap(&uuid_a, DekSource::Daemon)
            .await
            .expect("a");
        let (b, _) = backend
            .generate_and_wrap(&uuid_b, DekSource::Daemon)
            .await
            .expect("b");
        // Probability of collision is 2^-256; if this fires you've
        // either won the lottery or broken the CSPRNG.
        assert_ne!(a.as_bytes(), b.as_bytes());
    }
}
