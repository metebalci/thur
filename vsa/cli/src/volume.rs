// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use core_block::{SyncAfter, VolumeManifest, VolumeRuntime, parse_size, volume::parse_dedup_scope};
use serde::Deserialize;
use shared_keystore::{KeystoreYamlConfig, SecretBytes, passphrase_envelope};

use shared_admin_client::{AdminClient, urlencode};
use shared_cli::{format_bytes, with_host_ratio};

/// `volume create` — daemon-routed only. The daemon serializes the
/// create through its writer task and emits a `volume.create` audit
/// row; `--backend` is resolved daemon-side (inferred when exactly
/// one backend is configured). Refuses with a clear message when the
/// admin socket is unreachable.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_create(
    name: &str,
    size_str: &str,
    backend_arg: Option<&str>,
    page_size_str: &str,
    dedup_arg: &str,
    worm: bool,
    encrypt: bool,
    key_file: Option<&Path>,
    keystore: Option<&str>,
    dek_source: Option<&str>,
    sync_after_arg: &str,
    lun: Option<u64>,
) -> Result<()> {
    let size_bytes =
        parse_size(size_str).with_context(|| format!("parsing --size '{size_str}'"))?;
    let page_size_bytes_u64 = parse_size(page_size_str)
        .with_context(|| format!("parsing --page-size '{page_size_str}'"))?;
    let page_size_bytes = u32::try_from(page_size_bytes_u64)
        .map_err(|_| anyhow!("--page-size {page_size_str} too large (must fit in u32)"))?;
    let _ =
        parse_dedup_scope(dedup_arg).map_err(|e| anyhow!("invalid --dedup '{dedup_arg}': {e}"))?;
    let sync_after: SyncAfter = sync_after_arg
        .parse()
        .map_err(|e| anyhow!("invalid --sync-after '{sync_after_arg}': {e}"))?;

    // Resolve `--key-file` CLI-side so a missing / malformed file
    // surfaces here, not after the daemon has already created
    // half a volume. clap guarantees `--key-file` / `--keystore` /
    // `--dek-source` only ever appear alongside `--encrypt`.
    let key_hex: Option<String> = match key_file {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading --key-file {}", path.display()))?;
            let trimmed = raw.trim().trim_end_matches(['\n', '\r']).to_string();
            if trimmed.len() != shared_crypto::KEY_LEN * 2 {
                bail!(
                    "--key-file {} must contain {} hex chars (AES-256), got {}",
                    path.display(),
                    shared_crypto::KEY_LEN * 2,
                    trimmed.len(),
                );
            }
            // Validate it parses as hex before we send it on the
            // wire — the daemon validates too, but a clean
            // client-side error is friendlier.
            let mut bytes = [0u8; shared_crypto::KEY_LEN];
            hex::decode_to_slice(&trimmed, &mut bytes)
                .with_context(|| format!("--key-file {} is not valid hex", path.display()))?;
            Some(trimmed)
        }
        None => None,
    };
    let encryption_on = encrypt;

    let admin = AdminClient::auto_discover(&shared_naming::DISK);
    if !admin.ping().await {
        bail!(
            "thurvsad admin socket unreachable at {} — `volume create` \
             needs the daemon running so the create is serialized through the \
             writer task and audited. Start the daemon and retry.",
            admin.socket_path().display()
        );
    }
    create_via_socket(
        &admin,
        name,
        size_bytes,
        page_size_bytes,
        backend_arg,
        dedup_arg,
        worm,
        encryption_on,
        key_hex,
        keystore,
        dek_source,
        sync_after,
        lun,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_via_socket(
    admin: &AdminClient,
    name: &str,
    size_bytes: u64,
    page_size_bytes: u32,
    backend: Option<&str>,
    dedup: &str,
    worm: bool,
    encrypt: bool,
    key_hex: Option<String>,
    keystore: Option<&str>,
    dek_source: Option<&str>,
    sync_after: SyncAfter,
    lun: Option<u64>,
) -> Result<()> {
    let mut body = serde_json::json!({
        "name": name,
        "size_bytes": size_bytes,
        "page_size_bytes": page_size_bytes,
        "dedup": dedup,
        "worm": worm,
        "sync_after": sync_after.as_str(),
    });
    if let Some(b) = backend {
        body["backend"] = serde_json::Value::String(b.to_string());
    }
    if encrypt {
        body["encrypt"] = serde_json::Value::Bool(true);
    }
    if let Some(k) = key_hex {
        body["key_hex"] = serde_json::Value::String(k);
    }
    if let Some(k) = keystore {
        body["keystore"] = serde_json::Value::String(k.to_string());
    }
    if let Some(s) = dek_source {
        body["dek_source"] = serde_json::Value::String(s.to_string());
    }
    if let Some(n) = lun {
        body["lun"] = serde_json::Value::Number(n.into());
    }
    let row: VolumeRow = admin.post_json("/api/v1/volumes", &body).await?;
    println!("OK: volume '{}' created (LUN {})", row.name, row.lun);
    println!("  UUID:        {}", row.uuid);
    println!(
        "  Size:        {} ({})",
        format_bytes(row.size_bytes),
        row.size_bytes
    );
    println!("  Sector size: {} B", row.sector_bytes);
    println!(
        "  Page size:   {} ({})",
        format_bytes(u64::from(row.page_size_bytes)),
        row.page_size_bytes
    );
    println!("  Backend:     {}", row.backend);
    println!("  Dedup scope: {}", row.dedup_scope);
    println!("  WORM:        {}", row.worm);
    println!("  Sync after:  {}", sync_after);
    if encrypt {
        let keystore_label = keystore.unwrap_or("(default)");
        println!(
            "  Encryption:  AES-256-GCM at rest (keystore='{}')",
            keystore_label,
        );
    }
    Ok(())
}

/// `volume list` — daemon-routed only; reports the live LUN map
/// from the running daemon. Refuses when the admin socket is
/// unreachable.
pub async fn cmd_list(json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(&shared_naming::DISK);
    if !admin.ping().await {
        bail!(
            "thurvsad admin socket unreachable at {} — `volume list` \
             reports the live LUN map from the running daemon. Start the \
             daemon and retry.",
            admin.socket_path().display()
        );
    }
    let listing: VolumeListing = admin.get_json("/api/v1/volumes").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&listing.volumes)?);
        return Ok(());
    }
    if listing.volumes.is_empty() {
        println!("(no volumes)");
        return Ok(());
    }
    println!(
        "{:>4}  {:<24}  {:>10}  {:>10}  {:<14}  {:<7}  {:<5}",
        "LUN", "NAME", "SIZE", "PAGE", "BACKEND", "DEDUP", "WORM"
    );
    for r in &listing.volumes {
        println!(
            "{:>4}  {:<24}  {:>10}  {:>10}  {:<14}  {:<7}  {:<5}",
            r.lun,
            r.name,
            format_bytes(r.size_bytes),
            format_bytes(u64::from(r.page_size_bytes)),
            r.backend,
            r.dedup_scope,
            if r.worm { "yes" } else { "no" },
        );
    }
    Ok(())
}

/// `volume info NAME` — daemon-routed only; reports live volume
/// state from the running daemon. Refuses when the admin socket is
/// unreachable. `data_dir` is used only to render the volume's
/// on-disk path in the human-readable output.
pub async fn cmd_info(name: &str, json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(&shared_naming::DISK);
    if !admin.ping().await {
        bail!(
            "thurvsad admin socket unreachable at {} — `volume info` \
             reports live volume state from the running daemon. Start the \
             daemon and retry.",
            admin.socket_path().display()
        );
    }
    let value: serde_json::Value = admin
        .get_json(&format!("/api/v1/volumes/{}", urlencode(name)))
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let manifest: VolumeManifest = serde_json::from_value(value.clone())
        .with_context(|| "decoding volume manifest from admin socket")?;
    let lun = value.get("lun").and_then(|v| v.as_u64());
    let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let runtime: Option<VolumeRuntime> = value
        .get("runtime")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok());
    let allocated_pages = value.get("allocated_pages").and_then(|v| v.as_u64());
    print_manifest(path, &manifest, runtime.as_ref(), lun, allocated_pages);
    Ok(())
}

/// `volume destroy NAME`. Daemon-routed only — destruction needs
/// to coordinate with the live registry, and we don't want to race
/// the dispatcher by touching the manifest directly behind its
/// back. Refuses with "is the daemon running?" when the socket is
/// unreachable.
/// `volume modify NAME --sync-after <MODE>` — flip the volume's
/// SCSI SYNCHRONIZE CACHE durability tier at runtime. Daemon-
/// routed only; the daemon updates the live atomic + rewrites
/// runtime.json. Effective on the next host SYNC.
pub async fn cmd_modify_sync_after(name: &str, mode: &str) -> Result<()> {
    let parsed: SyncAfter = mode
        .parse()
        .map_err(|e| anyhow!("invalid --sync-after '{mode}': {e}"))?;
    let admin = AdminClient::auto_discover(&shared_naming::DISK);
    if !admin.ping().await {
        bail!(
            "thurvsad admin socket unreachable at {} — `volume modify --sync-after` \
             needs the daemon running so the live atomic + runtime.json stay in sync. \
             Start the daemon and retry.",
            admin.socket_path().display()
        );
    }
    let body = serde_json::json!({ "mode": parsed.as_str() });
    let resp: serde_json::Value = admin
        .post_json(
            &format!("/api/v1/volumes/{}/sync-after", urlencode(name)),
            &body,
        )
        .await?;
    let prev = resp.get("previous").and_then(|v| v.as_str()).unwrap_or("?");
    let now = resp
        .get("sync_after")
        .and_then(|v| v.as_str())
        .unwrap_or(parsed.as_str());
    println!("OK: volume '{name}' sync_after: {prev} -> {now}");
    if parsed == SyncAfter::Memory {
        eprintln!(
            "warning: volume '{name}' is now in `memory` sync mode — host fsync(2) returns \
             immediately; bytes the host believes are persisted are lost on the next crash. \
             The SCSI initiator is NOT signalled about this contract change."
        );
    } else if parsed == SyncAfter::Disk {
        eprintln!(
            "warning: volume '{name}' is now in `disk` sync mode — host fsync(2) settles to \
             the local pool only; bytes are lost if the daemon-host disk fails before the \
             upload worker drains. The SCSI initiator is NOT signalled about this contract \
             change."
        );
    }
    Ok(())
}

pub async fn cmd_destroy(name: &str, force: bool) -> Result<()> {
    if !force {
        eprintln!(
            "warning: destroying volume '{name}' will remove its manifest, \
             page index, and unregister it from the live LUN map. Per-volume \
             chunks remain in the pool until the next GC sweep. Re-run with \
             --force to confirm."
        );
        bail!("refusing to destroy without --force");
    }
    let admin = AdminClient::auto_discover(&shared_naming::DISK);
    if !admin.ping().await {
        bail!(
            "thurvsad admin socket unreachable at {} — `volume destroy` \
             needs the daemon running so the LUN comes out of the live map \
             cleanly. Start the daemon and retry.",
            admin.socket_path().display()
        );
    }
    let resp: serde_json::Value = admin
        .delete_json::<(), _>(&format!("/api/v1/volumes/{}", urlencode(name)), None)
        .await?;
    match resp.get("lun").and_then(|v| v.as_u64()) {
        Some(lun) => println!("OK: volume '{}' destroyed (was LUN {})", name, lun),
        None => println!("OK: volume '{}' destroyed", name),
    }
    Ok(())
}

/// `volume key migrate NAME --to NEW_BACKEND [--purge-local]` — move
/// a volume's DEK wrap-target from one keystore backend to another.
///
/// Safe to run with the daemon up. `manifest.json` is creation-
/// frozen — the daemon's hot path only mutates `runtime.json` — so
/// rewriting the manifest's `encryption.*` block out-of-band can't
/// race the live `VolumeWriter`'s flush.
///
/// Volume data is NOT rewritten — the plaintext DEK is unwrapped
/// from the current backend, rewrapped by the new one, and the
/// manifest's `encryption.keystore_backend` + `encryption.wrapped_dek`
/// are updated atomically (tmp + fsync + rename). Restart
/// thurvsad after to pick up the new keystore binding.
pub async fn cmd_key_migrate(
    data_dir: &Path,
    config_path: &Path,
    name: &str,
    to: &str,
    purge_local: bool,
) -> Result<()> {
    // Load the on-disk manifest. NotFound surfaces as the daemon-down
    // counterpart of `volume info NAME` failing — same error shape.
    let mut manifest = VolumeManifest::load(data_dir, name).map_err(|e| anyhow!(e))?;
    let Some(enc) = manifest.encryption.clone() else {
        bail!(
            "volume '{name}' is not encrypted; nothing to migrate. Re-create the volume \
             with --encrypt + --keystore NAME to bind it to a keystore."
        );
    };

    if enc.keystore_backend == to {
        bail!(
            "no-op: volume '{name}' is already bound to keystore '{to}'. Pass a \
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
    // entries with the same data_dir, two `awskms` entries pointing
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
             different keystore (different data_dir for local, different key for KMS / \
             Vault / KV / KMIP).",
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
    let plain_dek: SecretBytes = old_backend
        .unwrap(&manifest.uuid, &wrapped_in)
        .await
        .map_err(|e| anyhow!("unwrap via old backend '{}': {e}", enc.keystore_backend))?;

    // Wrap into new.
    let wrapped_out = new_backend
        .wrap(&manifest.uuid, &plain_dek)
        .await
        .map_err(|e| anyhow!("wrap via new backend '{to}': {e}"))?;

    // Build the updated manifest. For backends that own their
    // sidecar (just `local` today) the wrapped blob is empty and
    // wrapped_dek stays `None`; for everything else we base64 it
    // back into the manifest.
    let new_wrapped_field = if new_backend.manages_local_blob() {
        None
    } else {
        Some(B64.encode(&wrapped_out))
    };

    let from_label = enc.keystore_backend.clone();
    let new_meta = core_block::volume::VolumeEncryptionMeta {
        algorithm: enc.algorithm,
        keystore_backend: to.to_string(),
        wrapped_dek: new_wrapped_field,
    };
    manifest.encryption = Some(new_meta);

    let vol_dir = VolumeManifest::dir_for(data_dir, name);
    manifest.persist(&vol_dir).map_err(|e| anyhow!(e))?;

    // Optionally purge the local sidecar after persisting the new
    // manifest. We do this last so a crash between persist and purge
    // leaves the sidecar present (recoverable rollback) rather than
    // a half-migrated volume with no key material reachable.
    let sidecar_warning = if purge_local && from_label == "local" {
        match old_backend.forget(&manifest.uuid).await {
            Ok(()) => Some(true),
            Err(e) => {
                eprintln!(
                    "warning: migration succeeded but sidecar purge failed: {e}. Remove \
                     <data_dir>/keys/{}.key manually if desired.",
                    hex::encode(manifest.uuid)
                );
                Some(false)
            }
        }
    } else {
        None
    };

    println!("OK: volume '{name}' key migrated: '{from_label}' -> '{to}'");
    if new_backend.manages_local_blob() {
        println!(
            "  Wrap target moved to the local-backend sidecar at \
             <data_dir>/keys/{}.key (mode 0600).",
            hex::encode(manifest.uuid)
        );
    } else {
        println!(
            "  Wrapped DEK now stored in manifest.encryption.wrapped_dek \
             (backend-ciphertext, base64)."
        );
    }
    println!("  Restart thurvsad to pick up the new keystore binding.");
    if from_label == "local" {
        match sidecar_warning {
            Some(true) => println!(
                "  Local sidecar at <data_dir>/keys/{}.key removed (--purge-local).",
                hex::encode(manifest.uuid)
            ),
            None => println!(
                "  Local sidecar at <data_dir>/keys/{}.key preserved; re-run with \
                 --purge-local once you've verified the new backend.",
                hex::encode(manifest.uuid)
            ),
            Some(false) => {}
        }
    }
    Ok(())
}

/// `volume key export NAME --to PATH` — passphrase-seal a volume's
/// DEK in a JWE/PBES2 envelope for cross-region DR or audit-compliant
/// key custody.
///
/// Daemon-down only (mirrors `volume key migrate`'s constraint). The
/// envelope is one base64 JWE Compact string — storable in any
/// escrow vehicle and cryptographically bound to the volume UUID via
/// the GCM AAD. Passphrase comes from the tty (no flag — shell
/// history leak); the `THURVSA_PASSPHRASE` env var bypasses the
/// prompt for automation / tests.
pub async fn cmd_key_export(
    data_dir: &Path,
    config_path: &Path,
    name: &str,
    to: &Path,
    iter: u32,
) -> Result<()> {
    let manifest = VolumeManifest::load(data_dir, name).map_err(|e| anyhow!(e))?;
    let Some(enc) = manifest.encryption.clone() else {
        bail!(
            "volume '{name}' is not encrypted; nothing to export. Re-create the volume \
             with --encrypt + --keystore NAME to bind it to a keystore."
        );
    };

    if to.exists() {
        bail!(
            "output file '{}' already exists. Refusing to overwrite — escrow artifacts \
             are too easy to lose by accident. Move or delete it first, or pick a different --to.",
            to.display()
        );
    }

    let file = KeystoreYamlConfig::load_from_conffile(config_path)
        .with_context(|| format!("loading keystore.backends from {}", config_path.display()))?;
    let backend = file
        .create_backend_named(&enc.keystore_backend, data_dir)
        .await
        .map_err(|e| anyhow!("instantiate backend '{}': {e}", enc.keystore_backend))?;

    let wrapped_in: Vec<u8> = match enc.wrapped_dek.as_deref() {
        Some(b64) => B64
            .decode(b64.as_bytes())
            .with_context(|| "decoding manifest.encryption.wrapped_dek as base64")?,
        None => Vec::new(),
    };
    let plain_dek: SecretBytes = backend
        .unwrap(&manifest.uuid, &wrapped_in)
        .await
        .map_err(|e| anyhow!("unwrap via backend '{}': {e}", enc.keystore_backend))?;

    let passphrase = shared_cli_system::secrets_io::prompt_passphrase(&shared_naming::DISK, true)?;

    // Payload: JSON object with the DEK + algorithm + version tag.
    // Versioned so we can rev the payload schema later without
    // touching the JWE envelope shape.
    let payload = serde_json::json!({
        "dek": B64.encode(plain_dek.as_bytes()),
        "alg": "AES-256-GCM",
        "v": 1,
    });
    let payload_bytes =
        serde_json::to_vec(&payload).map_err(|e| anyhow!("serialize payload: {e}"))?;

    let mut extras = std::collections::BTreeMap::new();
    extras.insert(
        "cty".to_string(),
        "application/vnd.thur.vsa.dek+json".to_string(),
    );
    extras.insert("thur_purpose".to_string(), "vsa_volume_dek".to_string());
    extras.insert("thur_volume_uuid".to_string(), hex::encode(manifest.uuid));

    let jwe = passphrase_envelope::encode(&payload_bytes, &passphrase, iter, &extras)
        .map_err(|e| anyhow!("build JWE envelope: {e}"))?;

    shared_cli_system::secrets_io::write_mode_0600(to, jwe.as_bytes())
        .with_context(|| format!("writing envelope to {}", to.display()))?;

    println!("OK: volume '{name}' DEK exported to {}", to.display());
    println!("  UUID bound:    {}", hex::encode(manifest.uuid));
    println!("  Source backend: {}", enc.keystore_backend);
    println!("  Envelope:      JWE Compact (alg=PBES2-HS512+A256KW, enc=A256GCM, p2c={iter})");
    println!("  File mode:     0600");
    println!();
    println!("Store the file and its passphrase separately. Both are required");
    println!("to recover the DEK; either alone is useless.");
    Ok(())
}

/// `volume key import NAME --from PATH [--keystore NAME]` — unwrap a
/// JWE/PBES2 envelope and rewrap the DEK into the named keystore
/// backend (or the single inferred one).
///
/// Refuses if the volume's current keystore already holds an
/// unwrappable DEK — import is for the "keystore lost / wrong
/// backend after DR" case, not for in-place wrap-target swaps (use
/// `volume key migrate` for that). Daemon-down only.
pub async fn cmd_key_import(
    data_dir: &Path,
    config_path: &Path,
    name: &str,
    from: &Path,
    target_keystore: Option<&str>,
) -> Result<()> {
    let mut manifest = VolumeManifest::load(data_dir, name).map_err(|e| anyhow!(e))?;
    let Some(enc) = manifest.encryption.clone() else {
        bail!(
            "volume '{name}' is not encrypted in its manifest. Import only restores a \
             DEK for a volume that was created with --encrypt; create the volume first \
             (or check you're targeting the right volume name)."
        );
    };

    let jwe = std::fs::read_to_string(from)
        .with_context(|| format!("reading envelope from {}", from.display()))?;

    let passphrase = shared_cli_system::secrets_io::prompt_passphrase(&shared_naming::DISK, false)?;

    let (payload_bytes, header) = passphrase_envelope::decode(&jwe, &passphrase)
        .map_err(|e| anyhow!("decode envelope: {e}"))?;

    // Header sanity. We accept any extras but require the
    // purpose / cty / uuid binding the export verb stamped.
    let cty = header.get("cty").and_then(|v| v.as_str()).unwrap_or("");
    let purpose = header
        .get("thur_purpose")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bound_uuid = header
        .get("thur_volume_uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if cty != "application/vnd.thur.vsa.dek+json" || purpose != "vsa_volume_dek" {
        bail!(
            "envelope header doesn't look like a thurvsa DEK export \
             (cty='{cty}', thur_purpose='{purpose}'). Refusing import — \
             pass an envelope produced by `thurvsa volume key export`."
        );
    }
    let manifest_uuid_hex = hex::encode(manifest.uuid);
    if bound_uuid != manifest_uuid_hex {
        bail!(
            "envelope is bound to volume UUID {bound_uuid}, but volume '{name}' has \
             UUID {manifest_uuid_hex}. Cross-volume binding is rejected — the \
             envelope was made for a different volume."
        );
    }

    // Payload shape check + DEK extraction.
    #[derive(Deserialize)]
    struct DekPayload {
        dek: String,
        #[serde(default)]
        alg: String,
        #[serde(default)]
        v: u32,
    }
    let payload: DekPayload =
        serde_json::from_slice(&payload_bytes).map_err(|e| anyhow!("parse payload: {e}"))?;
    if payload.v != 1 {
        bail!(
            "envelope payload schema version {} is unsupported (this binary handles v=1).",
            payload.v
        );
    }
    if payload.alg != "AES-256-GCM" {
        bail!(
            "envelope payload algorithm '{}' is unsupported (this binary handles AES-256-GCM).",
            payload.alg
        );
    }
    let dek_bytes = B64
        .decode(payload.dek.as_bytes())
        .context("decoding payload.dek as base64")?;
    if dek_bytes.len() != shared_crypto::KEY_LEN {
        bail!(
            "payload DEK must be {} bytes (AES-256), got {}",
            shared_crypto::KEY_LEN,
            dek_bytes.len()
        );
    }
    let mut dek_arr = [0u8; shared_crypto::KEY_LEN];
    dek_arr.copy_from_slice(&dek_bytes);
    let plain_dek = SecretBytes::new(dek_arr);

    // Resolve target backend. CLI arg wins; otherwise infer from the
    // YAML conffile's `keystore.backends:` block (single entry =
    // obvious pick) or fail with a clear ask.
    let file = KeystoreYamlConfig::load_from_conffile(config_path)
        .with_context(|| format!("loading keystore.backends from {}", config_path.display()))?;
    let target_name = match target_keystore {
        Some(n) => {
            if file.backend_entry(n).is_err() {
                bail!(
                    "keystore backend '{n}' not defined under `keystore.backends:` in {}. Backends defined: {}",
                    config_path.display(),
                    file.backend_names().join(", ")
                );
            }
            n.to_string()
        }
        None => {
            let names = file.backend_names();
            if names.len() == 1 {
                names[0].clone()
            } else {
                bail!(
                    "--keystore NAME required: `keystore.backends:` in {} has {} entries ({}). \
                     Pick one explicitly.",
                    config_path.display(),
                    names.len(),
                    names.join(", ")
                );
            }
        }
    };

    // Refuse if the volume's current keystore already holds an
    // unwrappable DEK. We probe the *target* backend (not the
    // manifest's named backend) because that's what import would
    // overwrite. If the target unwraps successfully against this
    // volume's UUID, there's already a working key there — use
    // `volume key migrate` instead.
    let target_backend = file
        .create_backend_named(&target_name, data_dir)
        .await
        .map_err(|e| anyhow!("instantiate target backend '{target_name}': {e}"))?;
    let probe_wrapped: Vec<u8> = if target_name == enc.keystore_backend {
        // Same backend as the manifest names — probe with whatever
        // the manifest holds (or empty for local).
        match enc.wrapped_dek.as_deref() {
            Some(b64) => B64
                .decode(b64.as_bytes())
                .with_context(|| "decoding manifest.encryption.wrapped_dek as base64")?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    if target_backend
        .unwrap(&manifest.uuid, &probe_wrapped)
        .await
        .is_ok()
    {
        bail!(
            "target keystore '{target_name}' already holds an unwrappable DEK for \
             volume '{name}'. Import refuses to overwrite a working keystore binding \
             — use `thurvsa volume key migrate --to {target_name}` if you mean to \
             swap wrap targets in place."
        );
    }

    let wrapped_out = target_backend
        .wrap(&manifest.uuid, &plain_dek)
        .await
        .map_err(|e| anyhow!("wrap via target backend '{target_name}': {e}"))?;

    let new_wrapped_field = if target_backend.manages_local_blob() {
        None
    } else {
        Some(B64.encode(&wrapped_out))
    };

    let new_meta = core_block::volume::VolumeEncryptionMeta {
        algorithm: enc.algorithm,
        keystore_backend: target_name.clone(),
        wrapped_dek: new_wrapped_field,
    };
    manifest.encryption = Some(new_meta);

    let vol_dir = VolumeManifest::dir_for(data_dir, name);
    manifest.persist(&vol_dir).map_err(|e| anyhow!(e))?;

    println!("OK: volume '{name}' DEK imported into keystore '{target_name}'");
    if target_backend.manages_local_blob() {
        println!(
            "  Sidecar written: <data_dir>/keys/{}.key (mode 0600)",
            hex::encode(manifest.uuid)
        );
    } else {
        println!("  Wrapped DEK stored in manifest.encryption.wrapped_dek");
    }
    println!("  Restart thurvsad to pick up the keystore binding.");
    Ok(())
}

fn print_manifest(
    path: &str,
    m: &VolumeManifest,
    r: Option<&VolumeRuntime>,
    lun: Option<u64>,
    allocated_pages: Option<u64>,
) {
    println!("Volume: {}", m.name);
    if let Some(lun) = lun {
        println!("  LUN:               {lun}");
    }
    println!("  Path:              {path}");
    println!("  UUID:              {}", hex::encode(m.uuid));
    println!(
        "  Size:              {} ({} bytes)",
        format_bytes(m.size_bytes),
        m.size_bytes
    );
    if let Some(pages) = allocated_pages {
        let used = pages.saturating_mul(u64::from(m.page_size_bytes));
        let pct = if m.size_bytes > 0 {
            (used as f64 / m.size_bytes as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "  Used:              {} ({:.1}% of size, {} pages)",
            format_bytes(used),
            pct,
            pages
        );
    }
    println!("  Sector size:       {} B", m.sector_bytes);
    println!(
        "  Page size:         {} ({} bytes)",
        format_bytes(u64::from(m.page_size_bytes)),
        m.page_size_bytes
    );
    println!("  Backend:           {}", m.backend);
    println!("  Dedup scope:       {}", m.dedup_scope.as_str());
    println!("  WORM:              {}", m.worm);
    if let Some(r) = r {
        println!(
            "  {:<22} {}",
            "Host bytes written:",
            format_bytes(r.host_bytes_written)
        );
        println!(
            "  {:<22} {}",
            "Host bytes read:",
            format_bytes(r.host_bytes_read)
        );
        println!(
            "  {:<22} {}",
            "Backend bytes written:",
            with_host_ratio(r.backend_bytes_written, r.host_bytes_written)
        );
        println!(
            "  {:<22} {}",
            "Backend bytes read:",
            with_host_ratio(r.backend_bytes_read, r.host_bytes_read)
        );
        println!("  Modified:          {}", r.modified_at.to_rfc3339());
    }
    println!("  Created:           {}", m.created_at.to_rfc3339());
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct VolumeRow {
    lun: u64,
    name: String,
    size_bytes: u64,
    sector_bytes: u32,
    page_size_bytes: u32,
    backend: String,
    dedup_scope: String,
    worm: bool,
    uuid: String,
}

#[derive(Debug, Deserialize)]
struct VolumeListing {
    volumes: Vec<VolumeRow>,
}
