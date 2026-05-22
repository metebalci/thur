// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use core_mediachanger::{Library, LibraryPartition, SlotRange, check_daemon_not_running};
use shared_cloud::CloudConfig;
use std::path::{Path, PathBuf};

use crate::output::{create_table, format_bytes, with_host_ratio};

/// Initialize a new library
#[allow(clippy::too_many_arguments)]
pub async fn cmd_init(
    data_dir: &str,
    num_storage_slots: u32,
    num_mail_slots: u32,
    num_drives: u32,
    lto_generation: u8,
    firmware: Option<String>,
    transport_base: u16,
    storage_base: u16,
    import_export_base: u16,
    data_transfer_base: u16,
    config_path: &str,
) -> Result<()> {
    // Operator's responsibility to pre-create data_dir with the right
    // ownership — the .deb / .rpm postinst handles /var/lib/thurvtl,
    // dev setups need a manual `mkdir + chown` first. After the
    // pre-init privilege drop the CLI is no longer root, so silently
    // creating data_dir would either succeed with the wrong owner or
    // EACCES with a confusing error. Bail early instead.
    if !std::path::Path::new(data_dir).is_dir() {
        anyhow::bail!(
            "data_dir '{data_dir}' does not exist or is not a directory; \
             create it (chowned to the daemon user) before `library init`",
        );
    }

    // Safety check: refuse if daemon is running
    check_daemon_not_running(data_dir)?;

    let library_root = PathBuf::from(data_dir).join("library");
    let tapes_dir = PathBuf::from(data_dir).join("tapes");

    let resolved_firmware = firmware
        .clone()
        .unwrap_or_else(|| core_mediachanger::default_firmware_for_lto(lto_generation).to_string());

    let init_result = Library::initialize(
        &library_root,
        &tapes_dir,
        num_storage_slots,
        num_mail_slots,
        num_drives,
        lto_generation,
        firmware,
        transport_base,
        storage_base,
        import_export_base,
        data_transfer_base,
    )
    .context("Failed to initialize library");

    let audit_params = serde_json::json!({
        "storage_slots": num_storage_slots,
        "mail_slots": num_mail_slots,
        "drives": num_drives,
        "lto_generation": lto_generation,
        "firmware": resolved_firmware,
        "transport_base": transport_base,
        "storage_base": storage_base,
        "import_export_base": import_export_base,
        "data_transfer_base": data_transfer_base,
    });
    crate::audit_helper::record_result(
        data_dir,
        config_path,
        "library.init",
        audit_params,
        init_result.map(|_| ()),
    )?;

    println!("OK: Library initialized successfully");
    println!("  Storage slots: {}", num_storage_slots);
    println!("  Mail slots: {}", num_mail_slots);
    println!("  Drives: {}", num_drives);
    println!("  LTO generation: LTO-{}", lto_generation);
    println!("  Firmware revision: {}", resolved_firmware);
    let mail_range = if num_mail_slots == 0 {
        "none".to_string()
    } else {
        format!(
            "{}-{}",
            import_export_base,
            import_export_base as u32 + num_mail_slots - 1
        )
    };
    println!(
        "  Element addressing: transport {}, storage {}-{}, mail {}, drives {}-{}",
        transport_base,
        storage_base,
        storage_base as u32 + num_storage_slots - 1,
        mail_range,
        data_transfer_base,
        data_transfer_base as u32 + num_drives - 1,
    );
    println!();
    println!(
        "Library manifest created at: {}/library.json",
        library_root.display()
    );

    Ok(())
}

/// Restore cartridges from a cloud backend (cross-region DR).
///
/// Daemon-down. Discovers every cartridge with a `manifest-latest.json`
/// sentinel under the named backend's `manifests/` prefix, pulls each
/// cartridge's manifest + index pages, and seats the restored
/// cartridges into storage slots in barcode-sort order. Chunks
/// lazy-load on first host read once the daemon starts. Requires
/// `library init` to have been run first — chassis topology is not
/// cloud-replicated.
pub async fn cmd_restore(
    data_dir: &str,
    config_path: &str,
    backend_arg: Option<&str>,
    barcodes: Vec<String>,
    dry_run: bool,
    allow_existing: bool,
) -> Result<i32> {
    // Safety check: refuse if daemon is running. Restore writes into
    // <data_dir>/tapes/<barcode>/ — the live daemon owns those paths.
    check_daemon_not_running(data_dir)?;

    let library_root = PathBuf::from(data_dir).join("library");
    let tapes_dir = PathBuf::from(data_dir).join("tapes");

    if !library_root.join("library.json").exists() {
        anyhow::bail!(
            "library not initialized at {}; run `thurvtl library init --slots N --drives M --lto-generation 8` first \
             (chassis topology is not cloud-replicated and must be operator-supplied)",
            library_root.display()
        );
    }

    // Parse the daemon's YAML conffile. We read it directly because
    // the daemon is down — every other CLI verb routes through the
    // admin socket so the daemon owns the live state.
    let cloud_cfg = load_cloud_config_from_yaml(config_path)?;

    // Resolve the backend name: --backend wins, otherwise infer when
    // exactly one is configured (mirrors `cartridge create`).
    let backend_name = match backend_arg {
        Some(name) => name.to_string(),
        None => {
            let names = cloud_cfg.backend_names();
            match names.len() {
                0 => anyhow::bail!(
                    "no backends defined under `cloud.backends:` in {}; add one before restoring",
                    config_path
                ),
                1 => names
                    .into_iter()
                    .next()
                    .expect("len() == 1 implies one element"),
                _ => anyhow::bail!(
                    "--backend NAME is required when `cloud.backends:` in {} declares multiple backends; configured: {}",
                    config_path,
                    names.join(", ")
                ),
            }
        }
    };
    // Verify the backend exists before going further (clearer error
    // than failing later during instantiation).
    cloud_cfg
        .backend_entry(&backend_name)
        .map_err(|e| anyhow::anyhow!("backend '{}': {}", backend_name, e))?;

    let backend_box = cloud_cfg
        .create_backend_named(&backend_name)
        .await
        .with_context(|| format!("instantiate cloud backend '{}'", backend_name))?;

    println!("Restoring cartridges from backend: {}", backend_name);
    if dry_run {
        println!("(dry run — nothing will be written)");
    }
    println!();

    let report = core_mediachanger::library::restore::run_restore(
        &tapes_dir,
        backend_box.as_ref(),
        &backend_name,
        &barcodes,
        allow_existing,
        dry_run,
    )
    .await
    .context("cloud cartridge restore")?;

    print_restore_report(&report);

    if dry_run {
        // No filesystem mutation, no audit footprint. Always exit 0
        // — discovery succeeded by definition (run_restore returned
        // Ok), and there's nothing else to assert.
        return Ok(0);
    }

    // Inventory rebuild: seat each successfully-restored cartridge
    // into a storage slot. Sort by barcode for reproducible
    // placement; refuse if more cartridges than slots.
    let successes: Vec<String> = report.successes().into_iter().map(String::from).collect();
    let inventory_result = if successes.is_empty() {
        Ok(())
    } else {
        rebuild_inventory(&library_root, &tapes_dir, &successes, &backend_name)
    };

    // Audit footprint: one entry per invocation. Stage params so
    // the dispatcher's `record_result` sees a single Ok/Err signal
    // covering discovery + restore + inventory rebuild.
    let audit_params = serde_json::json!({
        "backend": backend_name,
        "discovered": report.discovered.len(),
        "selected": report.discovered.len() - report.filtered_out.len(),
        "skipped_existing": report.skipped_existing,
        "filtered_out": report.filtered_out,
        "restored": successes,
        "failed": report.failures(),
        "allow_existing": allow_existing,
    });

    let cli_result: Result<()> = if !report.failures().is_empty() {
        Err(anyhow::anyhow!(
            "{} cartridge(s) failed to restore",
            report.failures().len()
        ))
    } else {
        inventory_result
    };

    let exit_code = if cli_result.is_err() { 1 } else { 0 };
    let _ = crate::audit_helper::record_result(
        data_dir,
        config_path,
        "library.restore",
        audit_params,
        cli_result.map_err(|e| anyhow::anyhow!("{e}")),
    );

    Ok(exit_code)
}

/// Seat restored cartridges into storage slots. `restored_barcodes`
/// must be in the order the operator wants them placed (sorted by
/// `run_restore`).
fn rebuild_inventory(
    library_root: &Path,
    tapes_dir: &Path,
    restored_barcodes: &[String],
    backend_name: &str,
) -> Result<()> {
    let mut library = Library::open(library_root, tapes_dir).context("open library")?;

    let total_slots = library.storage_slots().len();
    let already_occupied = library
        .storage_slots()
        .iter()
        .filter(|s| s.occupied)
        .count();
    let free_slots = total_slots - already_occupied;

    if restored_barcodes.len() > free_slots {
        anyhow::bail!(
            "restored {} cartridge(s) but library has {} free storage slot(s) ({} total, {} already occupied); \
             re-run `library init` with --slots >= {} or free occupied slots first",
            restored_barcodes.len(),
            free_slots,
            total_slots,
            already_occupied,
            restored_barcodes.len() + already_occupied,
        );
    }

    for barcode in restored_barcodes {
        // `add_or_create_tape` short-circuits the Create path when
        // the cartridge dir already exists (which it does — restore
        // wrote it). Resulting effect is "place existing cartridge
        // in first free slot", which is what we want.
        library
            .add_or_create_tape(barcode, backend_name)
            .with_context(|| format!("seating cartridge '{}' into a storage slot", barcode))?;
    }
    Ok(())
}

fn print_restore_report(report: &core_mediachanger::library::restore::RestoreReport) {
    println!("Discovered: {} cartridge(s)", report.discovered.len());
    if !report.filtered_out.is_empty() {
        println!(
            "  Filtered out by --barcodes: {}",
            report.filtered_out.join(", ")
        );
    }
    if report.dry_run {
        for barcode in &report.discovered {
            if !report.filtered_out.contains(barcode) {
                println!("  would restore: {}", barcode);
            }
        }
        return;
    }
    let successes = report.successes();
    if !successes.is_empty() {
        println!("Restored: {} cartridge(s)", successes.len());
        for barcode in &successes {
            println!("  {}", barcode);
        }
    }
    if !report.skipped_existing.is_empty() {
        println!(
            "Skipped (already present locally): {}",
            report.skipped_existing.join(", ")
        );
    }
    let failures = report.failures();
    if !failures.is_empty() {
        println!("Failed: {} cartridge(s)", failures.len());
        for outcome in &report.cartridges {
            if let Err(ref e) = outcome.result {
                println!("  {}: {}", outcome.barcode, e);
            }
        }
    }
    println!();
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LibraryInfoResponse {
    storage_slots: usize,
    mail_slots: usize,
    drives: usize,
    lto_generation: u8,
    firmware: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cartridge_totals: Option<CartridgeTotals>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CartridgeTotals {
    cartridges: usize,
    host_bytes_written: u64,
    host_bytes_read: u64,
    backend_bytes_written: u64,
    backend_bytes_read: u64,
}

/// Show library information.
///
/// Routes through the daemon's admin socket — `library info` is an
/// operator-console-style read of live topology. The daemon must be
/// running. With `with_cartridges`, also requests the summed
/// per-cartridge byte counters.
pub async fn cmd_info(json: bool, with_cartridges: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = if with_cartridges {
        "/api/v1/library/info?with_cartridges=true"
    } else {
        "/api/v1/library/info"
    };
    let info: LibraryInfoResponse = client.get_json(path).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    println!("Library Configuration");
    println!("---------------------");
    println!("Storage slots: {}", info.storage_slots);
    println!("Mail slots: {}", info.mail_slots);
    println!("Drives: {}", info.drives);
    println!("LTO generation: LTO-{}", info.lto_generation);
    println!("Firmware revision: {}", info.firmware);

    if let Some(t) = &info.cartridge_totals {
        println!();
        println!("Cartridge byte totals ({} cartridges)", t.cartridges);
        println!(
            "  Host bytes written: {}",
            format_bytes(t.host_bytes_written)
        );
        println!("  Host bytes read: {}", format_bytes(t.host_bytes_read));
        println!(
            "  Backend bytes written: {}",
            with_host_ratio(t.backend_bytes_written, t.host_bytes_written)
        );
        println!(
            "  Backend bytes read: {}",
            with_host_ratio(t.backend_bytes_read, t.host_bytes_read)
        );
    }
    Ok(())
}

/// Modify library topology (resize slots/drives, change LTO generation,
/// override firmware revision).
///
/// `firmware` semantics: `None` = unchanged; `Some("")` = revert to the
/// per-LTO default; `Some(s)` = set custom revision (1-4 ASCII chars).
#[allow(clippy::too_many_arguments)]
pub async fn cmd_modify(
    data_dir: &str,
    num_storage_slots: Option<u32>,
    num_mail_slots: Option<u32>,
    num_drives: Option<u32>,
    lto_generation: Option<u8>,
    firmware: Option<String>,
    config_path: &str,
) -> Result<()> {
    // Safety check: refuse if daemon is running
    check_daemon_not_running(data_dir)?;

    let library_root = PathBuf::from(data_dir).join("library");
    let tapes_dir = PathBuf::from(data_dir).join("tapes");

    let mut library = Library::open(&library_root, &tapes_dir).context("Failed to open library")?;

    // Check if any parameters were provided
    if num_storage_slots.is_none()
        && num_mail_slots.is_none()
        && num_drives.is_none()
        && lto_generation.is_none()
        && firmware.is_none()
    {
        anyhow::bail!(
            "No modifications specified. Provide at least one of: --slots, --mail-slots, --drives, --lto-generation, --firmware"
        );
    }

    // Convert CLI firmware: empty string = revert to default, non-empty
    // = override. None = leave the existing setting alone.
    let new_firmware: Option<Option<String>> =
        firmware.map(|s| if s.is_empty() { None } else { Some(s) });

    // Show current configuration
    let old_slots = library.storage_slots().len();
    let old_mail = library.mail_slots().len();
    let old_drives = library.drives().len();
    let old_lto = library.lto_generation();

    println!("Current library configuration:");
    println!("  Storage slots: {}", old_slots);
    println!("  Mail slots: {}", old_mail);
    println!("  Drives: {}", old_drives);
    println!("  LTO generation: LTO-{}", old_lto);
    println!();

    // Show what will change
    println!("Requested changes:");

    // Check for shrinking operations and warn about auto-unload
    let mut has_shrink_warning = false;

    if let Some(slots) = num_storage_slots {
        let diff = slots as i32 - old_slots as i32;
        let op = if diff > 0 { "+" } else { "" };
        println!(
            "  Storage slots: {} → {} ({}{})",
            old_slots, slots, op, diff
        );

        if diff < 0 {
            let occupied = library.storage_slots()[(slots as usize)..]
                .iter()
                .filter(|s| s.occupied)
                .count();
            if occupied > 0 {
                println!(
                    "    WARNING: {} occupied slot(s) will be unloaded (cartridge data preserved)",
                    occupied
                );
                has_shrink_warning = true;
            }
        }
    }

    if let Some(mail) = num_mail_slots {
        let diff = mail as i32 - old_mail as i32;
        let op = if diff > 0 { "+" } else { "" };
        println!("  Mail slots: {} → {} ({}{})", old_mail, mail, op, diff);

        if diff < 0 {
            let occupied = library.mail_slots()[(mail as usize)..]
                .iter()
                .filter(|s| s.occupied)
                .count();
            if occupied > 0 {
                println!(
                    "    WARNING: {} occupied mail slot(s) will be cleared (cartridge data preserved)",
                    occupied
                );
                has_shrink_warning = true;
            }
        }
    }

    if let Some(drives) = num_drives {
        let diff = drives as i32 - old_drives as i32;
        let op = if diff > 0 { "+" } else { "" };
        println!("  Drives: {} → {} ({}{})", old_drives, drives, op, diff);

        if diff < 0 {
            let occupied = library.drives()[(drives as usize)..]
                .iter()
                .filter(|d| d.occupied)
                .count();
            if occupied > 0 {
                println!(
                    "    WARNING: {} loaded drive(s) will be unloaded (cartridge data preserved)",
                    occupied
                );
                has_shrink_warning = true;
            }
        }
    }

    if let Some(lto) = lto_generation {
        println!("  LTO generation: LTO-{} → LTO-{}", old_lto, lto);
    }
    println!();

    if has_shrink_warning {
        println!("Note: Cartridge data remains safe in the tapes directory.");
        println!("      You can re-add cartridges to slots after resizing.");
        println!();
    }

    // Apply changes
    let resize_result = library.resize(
        num_storage_slots,
        num_mail_slots,
        num_drives,
        lto_generation,
        new_firmware,
    );
    let audit_params = serde_json::json!({
        "before": {"storage_slots": old_slots, "mail_slots": old_mail, "drives": old_drives, "lto_generation": old_lto},
        "requested": {"storage_slots": num_storage_slots, "mail_slots": num_mail_slots, "drives": num_drives, "lto_generation": lto_generation},
    });
    match resize_result {
        Ok((new_slots, new_mail, new_drives, new_lto)) => {
            crate::audit_helper::record_ok(
                data_dir,
                config_path,
                "library.modify",
                serde_json::json!({
                    "before": {"storage_slots": old_slots, "mail_slots": old_mail, "drives": old_drives, "lto_generation": old_lto},
                    "after": {"storage_slots": new_slots, "mail_slots": new_mail, "drives": new_drives, "lto_generation": new_lto},
                }),
            );
            println!("OK: Library modified successfully");
            println!();
            println!("New configuration:");
            println!("  Storage slots: {}", new_slots);
            println!("  Mail slots: {}", new_mail);
            println!("  Drives: {}", new_drives);
            println!("  LTO generation: LTO-{}", new_lto);
            println!();
            println!(
                "Changes persisted to: {}/library.json",
                library_root.display()
            );
            println!();
            println!(
                "NOTE: Stop the daemon before modifying, then restart for changes to take effect"
            );
        }
        Err(e) => {
            crate::audit_helper::record_err(
                data_dir,
                config_path,
                "library.modify",
                audit_params,
                &e.to_string(),
            );
            anyhow::bail!("Failed to modify library: {}", e);
        }
    }

    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ChangerInventoryResponseItem {
    slot_id: u32,
    slot_type: String,
    barcode: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ChangerInventoryResponse {
    entries: Vec<ChangerInventoryResponseItem>,
}

pub async fn cmd_inventory(filter: Option<String>, json: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = match filter.as_deref() {
        Some(f) => format!(
            "/api/v1/changer/inventory?filter={}",
            shared_admin_client::urlencode(f)
        ),
        None => "/api/v1/changer/inventory".to_string(),
    };
    let resp: ChangerInventoryResponse = client.get_json(&path).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    println!();
    let mut table = create_table();
    table.set_header(vec!["Slot", "Type", "Barcode", "Label"]);

    for item in &resp.entries {
        let slot_str = match item.slot_type.as_str() {
            "mail" => format!("M{}", item.slot_id),
            "drive" => format!("D{}", item.slot_id),
            _ => item.slot_id.to_string(),
        };
        let type_disp = match item.slot_type.as_str() {
            "storage" => "Storage",
            "mail" => "Mail",
            "drive" => "Drive",
            other => other,
        };
        table.add_row(vec![slot_str, type_disp.to_string(), item.barcode.clone()]);
    }

    println!("Library Inventory ({} cartridges)", resp.entries.len());
    println!("{table}");

    Ok(())
}

#[derive(serde::Serialize)]
struct ChangerLoadBody {
    slot: u32,
    drive: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cross_partition: bool,
}

#[derive(serde::Serialize)]
struct ChangerUnloadBody {
    drive: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<u32>,
    force: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cross_partition: bool,
}

#[derive(serde::Serialize)]
struct ChangerMoveBody {
    from: u32,
    to: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cross_partition: bool,
}

#[derive(serde::Deserialize)]
struct ChangerMutationOk {
    #[allow(dead_code)]
    action: String,
    barcode: Option<String>,
    #[allow(dead_code)]
    from: u32,
    to: u32,
    #[serde(default)]
    cross_partition: bool,
}

fn cross_partition_suffix(crossed: bool) -> &'static str {
    if crossed { " (crossed partitions)" } else { "" }
}

pub async fn cmd_load(slot: u16, drive: u16, cross_partition: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = ChangerLoadBody {
        slot: slot as u32,
        drive: drive as u32,
        cross_partition,
    };
    let resp: ChangerMutationOk = client.post_json("/api/v1/changer/load", &body).await?;
    println!(
        "OK: Cartridge {} loaded from slot {} to drive {}{}",
        resp.barcode.as_deref().unwrap_or("?"),
        slot,
        drive,
        cross_partition_suffix(resp.cross_partition),
    );
    Ok(())
}

pub async fn cmd_unload(
    drive: u16,
    slot: Option<u16>,
    force: bool,
    cross_partition: bool,
) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = ChangerUnloadBody {
        drive: drive as u32,
        slot: slot.map(|s| s as u32),
        force,
        cross_partition,
    };
    let resp: ChangerMutationOk = client.post_json("/api/v1/changer/unload", &body).await?;
    println!(
        "OK: Cartridge {} unloaded from drive {} to slot {}{}{}",
        resp.barcode.as_deref().unwrap_or("?"),
        drive,
        resp.to,
        if force { " (forced)" } else { "" },
        cross_partition_suffix(resp.cross_partition),
    );
    Ok(())
}

pub async fn cmd_move(from_slot: u16, to_slot: u16, cross_partition: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = ChangerMoveBody {
        from: from_slot as u32,
        to: to_slot as u32,
        cross_partition,
    };
    let resp: ChangerMutationOk = client.post_json("/api/v1/changer/move", &body).await?;
    println!(
        "OK: Cartridge {} moved from slot {} to slot {}{}",
        resp.barcode.as_deref().unwrap_or("?"),
        from_slot,
        to_slot,
        cross_partition_suffix(resp.cross_partition),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// library partition {list,create,modify,delete}
//
// Chassis-assembly bucket: refuses while the daemon is running, persists
// directly to library.json under file lock, queues a PendingAuditEntry
// for the daemon to replay on next start.
// ---------------------------------------------------------------------------

fn open_library_for_chassis(data_dir: &str) -> Result<Library> {
    let library_root = PathBuf::from(data_dir).join("library");
    let tapes_dir = PathBuf::from(data_dir).join("tapes");
    Library::open(&library_root, &tapes_dir).context("Failed to open library")
}

fn print_partition_table(partitions: &[LibraryPartition]) {
    if partitions.is_empty() {
        println!("(no partitions defined — legacy single-partition library)");
        return;
    }
    let mut table = create_table();
    table.set_header(vec![
        "Name",
        "Storage",
        "Mail",
        "Drives",
        "Slot count",
        "Drive count",
    ]);
    for p in partitions {
        let storage_disp = if p.storage_slots.is_empty() {
            "(none)".to_string()
        } else {
            format!("[{}, {})", p.storage_slots.start, p.storage_slots.end)
        };
        let mail_disp = if p.mail_slots.is_empty() {
            "(none)".to_string()
        } else {
            format!("[{}, {})", p.mail_slots.start, p.mail_slots.end)
        };
        let drives_disp = if p.drives.is_empty() {
            "(none)".to_string()
        } else {
            p.drives
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        table.add_row(vec![
            p.name.clone(),
            storage_disp,
            mail_disp,
            drives_disp,
            p.storage_slots.len().to_string(),
            p.drives.len().to_string(),
        ]);
    }
    println!("{table}");
}

pub async fn cmd_partition_list(data_dir: &str, json: bool) -> Result<()> {
    let library = open_library_for_chassis(data_dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(library.partitions())?);
        return Ok(());
    }
    println!("Library partitions");
    println!("------------------");
    print_partition_table(library.partitions());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_partition_create(
    data_dir: &str,
    config_path: &str,
    name: String,
    storage_start: u32,
    storage_end: u32,
    mail_start: u32,
    mail_end: u32,
    drives: Vec<u32>,
) -> Result<()> {
    check_daemon_not_running(data_dir)?;
    let mut library = open_library_for_chassis(data_dir)?;

    if library.get_partition(&name).is_some() {
        anyhow::bail!("partition '{}' already exists", name);
    }

    let mut new_layout: Vec<LibraryPartition> = library.partitions().to_vec();
    new_layout.push(LibraryPartition {
        name: name.clone(),
        storage_slots: SlotRange {
            start: storage_start,
            end: storage_end,
        },
        mail_slots: SlotRange {
            start: mail_start,
            end: mail_end,
        },
        drives: drives.clone(),
    });

    let audit_params = serde_json::json!({
        "name": name,
        "storage": [storage_start, storage_end],
        "mail": [mail_start, mail_end],
        "drives": drives,
    });
    let result = library
        .set_partitions(new_layout)
        .with_context(|| format!("create partition '{}'", name));
    crate::audit_helper::record_result(
        data_dir,
        config_path,
        "library.partition.create",
        audit_params,
        result.map(|_| ()),
    )?;

    println!("OK: Partition '{}' created", name);
    print_partition_table(library.partitions());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_partition_modify(
    data_dir: &str,
    config_path: &str,
    name: String,
    storage_start: Option<u32>,
    storage_end: Option<u32>,
    mail_start: Option<u32>,
    mail_end: Option<u32>,
    drives: Option<Vec<u32>>,
) -> Result<()> {
    check_daemon_not_running(data_dir)?;
    let mut library = open_library_for_chassis(data_dir)?;

    if storage_start.is_none()
        && storage_end.is_none()
        && mail_start.is_none()
        && mail_end.is_none()
        && drives.is_none()
    {
        anyhow::bail!(
            "no modifications specified. Provide at least one of: --storage-start, --storage-end, --mail-start, --mail-end, --drives"
        );
    }

    let mut new_layout: Vec<LibraryPartition> = library.partitions().to_vec();
    let target = new_layout
        .iter_mut()
        .find(|p| p.name == name)
        .ok_or_else(|| anyhow::anyhow!("partition '{}' not found", name))?;

    if let Some(v) = storage_start {
        target.storage_slots.start = v;
    }
    if let Some(v) = storage_end {
        target.storage_slots.end = v;
    }
    if let Some(v) = mail_start {
        target.mail_slots.start = v;
    }
    if let Some(v) = mail_end {
        target.mail_slots.end = v;
    }
    if let Some(d) = drives.clone() {
        target.drives = d;
    }

    let audit_params = serde_json::json!({
        "name": name,
        "storage_start": storage_start,
        "storage_end": storage_end,
        "mail_start": mail_start,
        "mail_end": mail_end,
        "drives": drives,
    });
    let result = library
        .set_partitions(new_layout)
        .with_context(|| format!("modify partition '{}'", name));
    crate::audit_helper::record_result(
        data_dir,
        config_path,
        "library.partition.modify",
        audit_params,
        result.map(|_| ()),
    )?;

    println!("OK: Partition '{}' modified", name);
    print_partition_table(library.partitions());
    Ok(())
}

pub async fn cmd_partition_delete(
    data_dir: &str,
    config_path: &str,
    name: String,
    merge_into: Option<String>,
) -> Result<()> {
    check_daemon_not_running(data_dir)?;
    let mut library = open_library_for_chassis(data_dir)?;

    let existing: Vec<LibraryPartition> = library.partitions().to_vec();
    let target = existing
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| anyhow::anyhow!("partition '{}' not found", name))?
        .clone();

    let mut new_layout: Vec<LibraryPartition> =
        existing.into_iter().filter(|p| p.name != name).collect();

    match merge_into.as_deref() {
        Some(other) if other == name => {
            anyhow::bail!("--merge-into target must differ from the partition being deleted");
        }
        Some(other) => {
            let dest = new_layout
                .iter_mut()
                .find(|p| p.name == other)
                .ok_or_else(|| anyhow::anyhow!("merge target partition '{}' not found", other))?;
            // Storage range absorbs target's range. Two non-adjacent
            // ranges are not representable in a single SlotRange; fail
            // if that's the case so the operator splits the merge into
            // an explicit `partition modify` step.
            absorb_range(&mut dest.storage_slots, &target.storage_slots, "storage")?;
            absorb_range(&mut dest.mail_slots, &target.mail_slots, "mail")?;
            for d in &target.drives {
                if !dest.drives.contains(d) {
                    dest.drives.push(*d);
                }
            }
            dest.drives.sort_unstable();
        }
        None => {
            if !new_layout.is_empty() {
                anyhow::bail!(
                    "deleting '{}' would leave {} partition(s) with uncovered slots/drives. Pass --merge-into <other> to reassign, or delete every partition to revert to legacy mode",
                    name,
                    new_layout.len()
                );
            }
        }
    }

    let audit_params = serde_json::json!({
        "name": name,
        "merge_into": merge_into,
    });
    let result = library
        .set_partitions(new_layout)
        .with_context(|| format!("delete partition '{}'", name));
    crate::audit_helper::record_result(
        data_dir,
        config_path,
        "library.partition.delete",
        audit_params,
        result.map(|_| ()),
    )?;

    println!("OK: Partition '{}' deleted", name);
    print_partition_table(library.partitions());
    Ok(())
}

/// Merge `src` into `dst` storage / mail range. The chassis address
/// space is contiguous, so merging two ranges only produces a valid
/// `SlotRange` when they're adjacent or one is empty. If the merge
/// would create a hole, the operator must use `partition modify`
/// after the delete.
fn absorb_range(dst: &mut SlotRange, src: &SlotRange, label: &str) -> Result<()> {
    if src.is_empty() {
        return Ok(());
    }
    if dst.is_empty() {
        *dst = *src;
        return Ok(());
    }
    if dst.end == src.start {
        dst.end = src.end;
    } else if src.end == dst.start {
        dst.start = src.start;
    } else {
        anyhow::bail!(
            "cannot merge {} ranges [{}, {}) and [{}, {}) — not adjacent. Use `library partition modify` instead",
            label,
            dst.start,
            dst.end,
            src.start,
            src.end
        );
    }
    Ok(())
}

/// `thurvtl library restore-archive` — daemon-routed admin job
/// that drives `library::restore_archive::run_restore_archive` on
/// the daemon side.
pub async fn cmd_restore_archive(
    backend: &str,
    barcode: &str,
    label: &str,
    as_barcode: Option<&str>,
    allow_existing: bool,
    dry_run: bool,
) -> Result<i32> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({
        "backend": backend,
        "barcode": barcode,
        "label": label,
        "as_barcode": as_barcode,
        "allow_existing": allow_existing,
        "dry_run": dry_run,
    });
    let exit = client
        .run_job("library.restore_archive", &body, |ev| match ev {
            shared_admin_proto::JobEvent::Log { level, message } => {
                if level == "warn" || level == "error" {
                    eprintln!("{}", message);
                } else {
                    println!("{}", message);
                }
            }
            shared_admin_proto::JobEvent::Done {
                error: Some(msg), ..
            } => {
                eprintln!("{}", msg);
            }
            shared_admin_proto::JobEvent::Result { .. }
            | shared_admin_proto::JobEvent::Progress { .. }
            | shared_admin_proto::JobEvent::Done { error: None, .. } => {}
        })
        .await
        .context("restore-archive job stream")?;
    Ok(exit)
}

/// Parse the daemon YAML conffile and extract its `cloud:` section.
/// The rest of the YAML is ignored — daemon-down CLI verbs that only
/// need to know about backends shouldn't pay the cost of parsing the
/// full daemon Config struct (which transitively pulls in every
/// product-specific config block).
fn load_cloud_config_from_yaml(config_path: &str) -> Result<CloudConfig> {
    #[derive(serde::Deserialize)]
    struct CloudOnly {
        #[serde(default)]
        cloud: CloudConfig,
    }
    let body =
        std::fs::read_to_string(config_path).with_context(|| format!("read {}", config_path))?;
    let parsed: CloudOnly = serde_yaml::from_str(&body)
        .with_context(|| format!("parse YAML conffile {}", config_path))?;
    Ok(parsed.cloud)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::library::restore::{CartridgeOutcome, RestoreReport};

    fn range(start: u32, end: u32) -> SlotRange {
        SlotRange { start, end }
    }

    #[test]
    fn cross_partition_suffix_branches() {
        assert_eq!(cross_partition_suffix(true), " (crossed partitions)");
        assert_eq!(cross_partition_suffix(false), "");
    }

    #[test]
    fn absorb_range_empty_source_is_noop() {
        let mut dst = range(0, 10);
        absorb_range(&mut dst, &range(5, 5), "storage").expect("noop");
        assert_eq!(dst, range(0, 10));
    }

    #[test]
    fn absorb_range_empty_dest_takes_source() {
        let mut dst = range(5, 5);
        absorb_range(&mut dst, &range(10, 20), "storage").expect("adopt");
        assert_eq!(dst, range(10, 20));
    }

    #[test]
    fn absorb_range_appends_adjacent_after() {
        let mut dst = range(0, 10);
        absorb_range(&mut dst, &range(10, 20), "storage").expect("append");
        assert_eq!(dst, range(0, 20));
    }

    #[test]
    fn absorb_range_prepends_adjacent_before() {
        let mut dst = range(10, 20);
        absorb_range(&mut dst, &range(0, 10), "storage").expect("prepend");
        assert_eq!(dst, range(0, 20));
    }

    #[test]
    fn absorb_range_rejects_non_adjacent() {
        let mut dst = range(0, 10);
        let err = absorb_range(&mut dst, &range(15, 25), "storage");
        assert!(err.is_err());
        assert!(
            err.expect_err("non-adjacent must fail")
                .to_string()
                .contains("not adjacent")
        );
    }

    #[test]
    fn print_partition_table_handles_empty_layout() {
        // Smoke: exercises the empty branch (prints the legacy note).
        print_partition_table(&[]);
    }

    #[test]
    fn print_partition_table_renders_partitions() {
        let parts = vec![
            LibraryPartition {
                name: "p1".to_string(),
                storage_slots: range(0, 20),
                mail_slots: range(0, 2),
                drives: vec![0, 1],
            },
            LibraryPartition {
                name: "p2".to_string(),
                storage_slots: range(20, 40),
                mail_slots: SlotRange::default(),
                drives: vec![],
            },
        ];
        // Exercises both the populated and the "(none)" cell branches.
        print_partition_table(&parts);
    }

    #[test]
    fn print_restore_report_dry_run_branch() {
        let report = RestoreReport {
            backend_name: "s3b".to_string(),
            discovered: vec!["A".to_string(), "B".to_string()],
            filtered_out: vec!["B".to_string()],
            skipped_existing: vec![],
            cartridges: vec![],
            dry_run: true,
        };
        print_restore_report(&report);
    }

    #[test]
    fn print_restore_report_success_and_failure_branches() {
        let report = RestoreReport {
            backend_name: "s3b".to_string(),
            discovered: vec!["A".to_string(), "B".to_string()],
            filtered_out: vec![],
            skipped_existing: vec!["C".to_string()],
            cartridges: vec![
                CartridgeOutcome {
                    barcode: "A".to_string(),
                    result: Ok(()),
                },
                CartridgeOutcome {
                    barcode: "B".to_string(),
                    result: Err("download failed".to_string()),
                },
            ],
            dry_run: false,
        };
        assert_eq!(report.successes(), vec!["A"]);
        assert_eq!(report.failures(), vec!["B"]);
        print_restore_report(&report);
    }

    #[test]
    fn open_library_for_chassis_fails_on_uninitialized_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = open_library_for_chassis(dir.path().to_str().expect("utf8"));
        assert!(err.is_err());
    }

    #[test]
    fn open_library_for_chassis_opens_initialized_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib_root = dir.path().join("library");
        let tapes = dir.path().join("tapes");
        core_mediachanger::Library::initialize(
            &lib_root, &tapes, 10, 0, 2, 8, None, 0, 1001, 101, 1,
        )
        .expect("init library");
        let lib = open_library_for_chassis(dir.path().to_str().expect("utf8"))
            .expect("open initialized library");
        assert_eq!(lib.storage_slots().len(), 10);
    }

    #[test]
    fn load_cloud_config_from_yaml_missing_file_errors() {
        let err = load_cloud_config_from_yaml("/nonexistent/thurvtl.yaml");
        assert!(err.is_err());
    }

    #[test]
    fn load_cloud_config_from_yaml_empty_cloud_block_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("thurvtl.yaml");
        std::fs::write(&cfg, "data_dir: /srv/thur\n").expect("write cfg");
        let cloud = load_cloud_config_from_yaml(cfg.to_str().expect("utf8"))
            .expect("parse yaml with no cloud block");
        assert!(cloud.backend_names().is_empty());
    }

    #[test]
    fn rebuild_inventory_seats_restored_cartridges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib_root = dir.path().join("library");
        let tapes = dir.path().join("tapes");
        core_mediachanger::Library::initialize(
            &lib_root, &tapes, 5, 0, 1, 8, None, 0, 1001, 101, 1,
        )
        .expect("init library");
        // The cartridge dir must exist for add_or_create_tape to take
        // the "seat existing" path.
        std::fs::create_dir_all(tapes.join("TAPE001")).expect("mkdir tape dir");
        rebuild_inventory(&lib_root, &tapes, &["TAPE001".to_string()], "local")
            .expect("rebuild inventory");
        let lib = Library::open(&lib_root, &tapes).expect("reopen");
        assert_eq!(lib.storage_slots().iter().filter(|s| s.occupied).count(), 1);
    }

    #[test]
    fn rebuild_inventory_rejects_when_too_many_for_free_slots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib_root = dir.path().join("library");
        let tapes = dir.path().join("tapes");
        core_mediachanger::Library::initialize(
            &lib_root, &tapes, 2, 0, 1, 8, None, 0, 1001, 101, 1,
        )
        .expect("init library");
        let barcodes = vec!["T1".to_string(), "T2".to_string(), "T3".to_string()];
        let err = rebuild_inventory(&lib_root, &tapes, &barcodes, "local");
        assert!(err.is_err());
        assert!(
            err.expect_err("over capacity must fail")
                .to_string()
                .contains("free storage slot")
        );
    }
}
