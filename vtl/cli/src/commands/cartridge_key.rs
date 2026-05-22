// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `cartridge key {migrate,show}` — daemon-down per-cartridge DEK
//! management for the appliance-side at-rest encryption layer. Mirrors
//! `vsa/cli/src/volume.rs` `cmd_key_migrate` / `cmd_key_show` so the
//! two products' keystore wiring stays in lockstep.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use core_mediachanger::{Cartridge, CartridgeEncryptionAlgorithm, CartridgeEncryptionMeta};
use shared_keystore::KeystoreYamlConfig;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// `thurvtl cartridge key migrate BARCODE --to NEW_BACKEND
/// [--purge-local]` — move a cartridge's DEK wrap-target from one
/// keystore backend to another.
///
/// Daemon must be stopped. The cartridge data is NOT re-encrypted —
/// the plaintext DEK is unwrapped from the current backend, re-wrapped
/// by the new one, and the manifest's `encryption.keystore_backend` +
/// `encryption.wrapped_dek` are updated atomically (tmp + rename).
/// Restart `thurvtld` after to pick up the new keystore binding.
pub async fn cmd_key_migrate(
    data_dir: &Path,
    config_path: &Path,
    barcode: &str,
    to: &str,
    purge_local: bool,
) -> Result<()> {
    let tapes_root = data_dir.join("tapes");
    let (uuid, current_meta) =
        Cartridge::read_manifest_identity(&tapes_root, barcode).map_err(|e| {
            anyhow!(
                "load manifest for cartridge '{barcode}' at {}: {e}",
                tapes_root.join(barcode).display()
            )
        })?;
    let Some(enc) = current_meta else {
        bail!(
            "cartridge '{barcode}' has no at-rest encryption; nothing to migrate. \
             Re-create the cartridge with --keystore NAME to bind it to a keystore."
        );
    };

    if enc.keystore_backend == to {
        bail!(
            "no-op: cartridge '{barcode}' is already bound to keystore '{to}'. Pass a \
             different --to <NAME>."
        );
    }

    // Resolve old + new backends from the YAML conffile's
    // `keystore.backends:` block.
    let file = KeystoreYamlConfig::load_from_conffile(config_path)
        .with_context(|| format!("loading keystore.backends from {}", config_path.display()))?;
    if file.backend_entry(to).is_err() {
        bail!(
            "keystore backend '{to}' not defined under `keystore.backends:` in {}. Backends defined: {}",
            config_path.display(),
            file.backend_names().join(", ")
        );
    }
    let old_backend = file
        .create_backend_named(&enc.keystore_backend, data_dir)
        .await
        .map_err(|e| anyhow!("instantiate old backend '{}': {e}", enc.keystore_backend))?;
    let new_backend = file
        .create_backend_named(to, data_dir)
        .await
        .map_err(|e| anyhow!("instantiate new backend '{to}': {e}"))?;

    // Wrap-target fingerprint check. Two differently-named entries
    // can resolve to the same external wrap target — two `local`
    // entries with the same `data_dir`, two `awskms` entries pointing
    // at the same key ARN, etc. Bail before unwrap/wrap so the
    // operator gets a clear signal the migrate would be a logical
    // no-op (and, for local-vs-local collisions, before the sidecar
    // write fails with "key file already exists").
    let from_fp = old_backend.wrap_target_fingerprint();
    let to_fp = new_backend.wrap_target_fingerprint();
    if from_fp == to_fp {
        bail!(
            "no-op: keystore '{}' and '{to}' resolve to the same wrap target ({from_fp}). \
             Migrate would not change custody of the DEK. Pick a --to that points at a \
             different keystore.",
            enc.keystore_backend
        );
    }

    // Unwrap from old. For `local` the wrapped blob in the manifest
    // is absent; the backend reads the sidecar.
    let wrapped_in: Vec<u8> = match enc.wrapped_dek.as_deref() {
        Some(b64) => B64
            .decode(b64.as_bytes())
            .with_context(|| "decoding manifest.encryption.wrapped_dek as base64")?,
        None => Vec::new(),
    };
    let plain_dek = old_backend
        .unwrap(&uuid, &wrapped_in)
        .await
        .map_err(|e| anyhow!("unwrap via old backend '{}': {e}", enc.keystore_backend))?;

    // Wrap into new.
    let wrapped_out = new_backend
        .wrap(&uuid, &plain_dek)
        .await
        .map_err(|e| anyhow!("wrap via new backend '{to}': {e}"))?;

    // Build the updated manifest entry. For backends that own their
    // sidecar (just `local` today) the wrapped blob is empty and
    // wrapped_dek stays `None`; for everything else we base64 it
    // back into the manifest.
    let new_wrapped_field = if new_backend.manages_local_blob() {
        None
    } else {
        Some(B64.encode(&wrapped_out))
    };

    let from_label = enc.keystore_backend.clone();
    let new_meta = CartridgeEncryptionMeta {
        algorithm: CartridgeEncryptionAlgorithm::Aes256Gcm,
        keystore_backend: to.to_string(),
        wrapped_dek: new_wrapped_field,
    };
    Cartridge::rewrite_manifest_encryption(&tapes_root, barcode, Some(new_meta))
        .map_err(|e| anyhow!("persist manifest for cartridge '{barcode}': {e}"))?;

    // Optionally purge the local sidecar after persisting the new
    // manifest. We do this last so a crash between persist and purge
    // leaves the sidecar present (recoverable rollback) rather than
    // a half-migrated cartridge with no key material reachable.
    let sidecar_warning = if purge_local && from_label == "local" {
        match old_backend.forget(&uuid).await {
            Ok(()) => Some(true),
            Err(e) => {
                eprintln!(
                    "warning: migration succeeded but sidecar purge failed: {e}. Remove \
                     <data_dir>/keys/{}.key manually if desired.",
                    hex::encode(uuid)
                );
                Some(false)
            }
        }
    } else {
        None
    };

    println!("OK: cartridge '{barcode}' key migrated: '{from_label}' -> '{to}'");
    if new_backend.manages_local_blob() {
        println!(
            "  Wrap target moved to the local-backend sidecar at \
             <data_dir>/keys/{}.key (mode 0600).",
            hex::encode(uuid)
        );
    } else {
        println!(
            "  Wrapped DEK now stored in manifest.encryption.wrapped_dek \
             (backend-ciphertext, base64)."
        );
    }
    println!("  Restart thurvtld to pick up the new keystore binding.");
    if from_label == "local" {
        match sidecar_warning {
            Some(true) => println!(
                "  Local sidecar at <data_dir>/keys/{}.key removed (--purge-local).",
                hex::encode(uuid)
            ),
            None => println!(
                "  Local sidecar at <data_dir>/keys/{}.key preserved; re-run with \
                 --purge-local once you've verified the new backend.",
                hex::encode(uuid)
            ),
            Some(false) => {}
        }
    }
    Ok(())
}

/// `thurvtl cartridge key show BARCODE` — print a cartridge's
/// at-rest encryption metadata. Read-only. Never prints DEK bytes.
pub async fn cmd_key_show(data_dir: &Path, barcode: &str) -> Result<()> {
    let tapes_root = data_dir.join("tapes");
    let (uuid, meta) = Cartridge::read_manifest_identity(&tapes_root, barcode)
        .map_err(|e| anyhow!("load manifest for cartridge '{barcode}': {e}"))?;
    println!("Cartridge: {barcode}");
    println!("UUID:      {}", hex::encode(uuid));
    match meta {
        None => {
            println!("At-rest encryption: disabled");
            println!(
                "  This cartridge stores plaintext chunks in the pool. Re-create with \
                 --keystore NAME for at-rest encryption."
            );
        }
        Some(m) => {
            println!("At-rest encryption: enabled");
            println!("  Algorithm:        {}", m.algorithm.as_str());
            println!("  Keystore backend: {}", m.keystore_backend);
            match m.wrapped_dek.as_deref() {
                Some(b64) => {
                    let raw = B64.decode(b64.as_bytes()).map(|v| v.len()).unwrap_or(0);
                    println!("  Wrapped DEK:      {raw} bytes (base64-encoded in manifest)");
                }
                None => {
                    println!(
                        "  Wrapped DEK:      stored in local sidecar at \
                         <data_dir>/keys/{}.key",
                        hex::encode(uuid)
                    );
                }
            }
        }
    }
    Ok(())
}
