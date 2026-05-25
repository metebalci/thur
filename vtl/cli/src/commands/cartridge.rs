// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};

use crate::output::{create_table, format_bytes, with_host_ratio};

fn default_cartridge_chunk_size_mb() -> u64 {
    // Used as exact size for fixed mode and as the FastCDC target avg
    // for fastcdc mode. 8 MB is a reasonable default for both — small
    // enough to dedup tar streams, large enough to keep S3 PUT counts
    // manageable on a 12 TB tape.
    8
}
fn default_cartridge_chunking() -> &'static str {
    "fastcdc"
}
fn default_cartridge_dedup() -> &'static str {
    // Global is the headline storage feature — default cartridges
    // join the shared per-backend pool. Operators who want
    // per-cartridge isolation override per-cartridge with
    // `--dedup local`.
    "global"
}

// ---------------------------------------------------------------------------
// cartridge create — thin daemon-call wrappers
//
// CLI never reads `thurvtl.yaml` — it only talks to the daemon
// over the admin Unix socket. Default chunk size / chunking / dedup
// are compiled-in; per-invocation overrides come from CLI flags.
// The daemon validates against its live cloud config (backend list,
// retention_mode for WORM), locks the library, expands multi-
// barcodes, runs `Cartridge::create_with_chunking`, rolls back on
// failure, and emits audit entries.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct CartridgeCreateBody<'a> {
    barcode: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    lto_generation: Option<u8>,
    chunk_size_bytes: u64,
    chunking: &'a str,
    /// FastCDC min/max overrides. Omitted from the wire when None so
    /// the daemon falls back to its derivation (avg/8, avg*4).
    #[serde(skip_serializing_if = "Option::is_none")]
    chunking_min_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunking_max_bytes: Option<u64>,
    multi: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<&'a str>,
    worm: bool,
    dedup: &'a str,
    /// Operator passed `--encrypt`. Opt-in at-rest encryption; the
    /// daemon mints a per-cartridge DEK wrapped by `keystore`.
    encrypt: bool,
    /// At-rest keystore-backend name. Sent only with `encrypt: true`;
    /// the daemon wraps the per-cartridge DEK against it.
    #[serde(skip_serializing_if = "Option::is_none")]
    keystore: Option<&'a str>,
}

#[derive(serde::Deserialize)]
struct CartridgeCreateOkResp {
    created: Vec<CreatedCartridgeResp>,
}

#[derive(serde::Deserialize)]
struct CreatedCartridgeResp {
    barcode: String,
    slot: u32,
    backend: String,
    lto_generation: u8,
    worm: bool,
    chunking: String,
    chunk_size_bytes: u64,
    /// Echoed back by the daemon when the cartridge was created
    /// with at-rest encryption (`--encrypt --keystore NAME`).
    #[serde(default)]
    keystore: Option<String>,
}

pub async fn cmd_create(
    barcode: &str,
    lto_generation: Option<u8>,
    chunk_size_mb: Option<u64>,
    chunking_arg: Option<&str>,
    chunking_min_kb: Option<u64>,
    chunking_max_kb: Option<u64>,
    multi: u32,
    backend_arg: Option<&str>,
    worm: bool,
    dedup_arg: Option<&str>,
    encrypt: bool,
    keystore_arg: Option<&str>,
) -> Result<()> {
    let chunking = chunking_arg
        .map(str::to_lowercase)
        .unwrap_or_else(|| default_cartridge_chunking().to_string());
    let dedup = dedup_arg.unwrap_or(default_cartridge_dedup());
    let chunk_size_mb_resolved = chunk_size_mb.unwrap_or(default_cartridge_chunk_size_mb());
    let chunk_size_bytes = chunk_size_mb_resolved.saturating_mul(1024 * 1024);
    let chunking_min_bytes = chunking_min_kb.map(|kb| kb.saturating_mul(1024));
    let chunking_max_bytes = chunking_max_kb.map(|kb| kb.saturating_mul(1024));

    // Fail fast client-side on a user error the daemon would reject anyway:
    // min/max are FastCDC-only knobs.
    if chunking != "fastcdc" && (chunking_min_bytes.is_some() || chunking_max_bytes.is_some()) {
        anyhow::bail!("--chunking-min-kb / --chunking-max-kb require --chunking fastcdc");
    }

    let body = CartridgeCreateBody {
        barcode,
        lto_generation,
        chunk_size_bytes,
        chunking: &chunking,
        chunking_min_bytes,
        chunking_max_bytes,
        multi,
        backend: backend_arg,
        worm,
        dedup,
        encrypt,
        keystore: keystore_arg,
    };
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let resp: CartridgeCreateOkResp = client.post_json("/api/v1/cartridges", &body).await?;
    for c in &resp.created {
        let worm_tag = if c.worm { ", WORM" } else { "" };
        println!(
            "OK: Created cartridge '{}' (LTO-{}, {} chunks of {} MB, backend '{}'{})",
            c.barcode,
            c.lto_generation,
            c.chunking,
            c.chunk_size_bytes / (1024 * 1024),
            c.backend,
            worm_tag,
        );
        println!("  Placed in slot {}", c.slot);
        if let Some(ks) = c.keystore.as_deref() {
            println!("  At-rest encryption: keystore '{}' (AES-256-GCM)", ks);
        }
    }
    Ok(())
}

/// `thurvtl cartridge archive` — frozen-snapshot a cartridge to
/// a second backend. Daemon-routed long-running job; returns the
/// job's exit code so the caller can `std::process::exit`.
pub async fn cmd_archive(
    barcode: &str,
    target_backend: &str,
    label: Option<&str>,
    dry_run: bool,
) -> Result<i32> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    // Default label is an ISO-8601 UTC timestamp; rendered CLI-side
    // so the audit entry records exactly what the operator saw.
    let default_label;
    let label = match label {
        Some(s) => s,
        None => {
            default_label = chrono::Utc::now()
                .format("archive-%Y-%m-%dT%H-%M-%SZ")
                .to_string();
            &default_label
        }
    };
    let body = serde_json::json!({
        "barcode": barcode,
        "target_backend": target_backend,
        "label": label,
        "dry_run": dry_run,
    });
    let exit = client
        .run_job("cartridge.archive", &body, render_job_event)
        .await
        .context("archive job stream")?;
    Ok(exit)
}

fn render_job_event(ev: shared_admin_proto::JobEvent) {
    match ev {
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
    }
}

/// `thurvtl cartridge migrate` — long-running admin job that
/// drives `cartridge_migrate::run_migrate` on the daemon side.
/// Returns the job's exit code (0 success, non-zero failure) so the
/// caller can `std::process::exit`.
pub async fn cmd_migrate(
    barcode: &str,
    target_backend: &str,
    mode: &str,
    verify: bool,
    dry_run: bool,
) -> Result<i32> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({
        "barcode": barcode,
        "target_backend": target_backend,
        "mode": mode,
        "verify": verify,
        "dry_run": dry_run,
    });
    let exit = client
        .run_job("cartridge.migrate", &body, render_job_event)
        .await
        .context("migrate job stream")?;
    Ok(exit)
}

// ---------------------------------------------------------------------------
// cartridge import / export — thin daemon-call wrappers
//
// Both take server-side filesystem paths: the daemon owns the data
// directory, and every deployment supports the operator having shell
// access to the daemon host. No byte upload over the socket.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct CartridgeImportBody<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<u32>,
}

#[derive(serde::Deserialize)]
struct CartridgeImportOkResp {
    barcode: String,
    backend: String,
    slot: u32,
    fallback_slot: bool,
}

#[derive(serde::Serialize)]
struct CartridgeExportBody<'a> {
    path: &'a str,
}

#[derive(serde::Deserialize)]
struct CartridgeExportOkResp {
    barcode: String,
    slot: u32,
    #[allow(dead_code)]
    dest_path: String,
}

pub async fn cmd_import(path: &str, slot: u16) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = CartridgeImportBody {
        path,
        slot: Some(slot as u32),
    };
    let resp: CartridgeImportOkResp = client.post_json("/api/v1/cartridges/import", &body).await?;
    println!(
        "OK: Imported cartridge '{}' to slot {} (backend '{}')",
        resp.barcode, resp.slot, resp.backend
    );
    if resp.fallback_slot {
        println!(
            "  Note: Requested slot {} but placed in slot {} (first available)",
            slot, resp.slot
        );
    }
    Ok(())
}

pub async fn cmd_export(slot: u16, path: &str) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = CartridgeExportBody { path };
    let url = format!("/api/v1/cartridges/export/{}", slot as u32);
    let resp: CartridgeExportOkResp = client.post_json(&url, &body).await?;
    println!("OK: Exported cartridge '{}' to {}", resp.barcode, path);
    println!(
        "  Note: Cartridge remains in slot {} (export creates a copy)",
        resp.slot
    );
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CartridgeListResponseItem {
    barcode: String,
    location: String,
    slot_id: u32,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CartridgeListResponse {
    cartridges: Vec<CartridgeListResponseItem>,
}

pub async fn cmd_list(json: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let resp: CartridgeListResponse = client.get_json("/api/v1/cartridges").await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    println!();
    let mut table = create_table();
    table.set_header(vec!["Barcode", "Location", "Slot"]);

    for cart in &resp.cartridges {
        let slot_str = match cart.location.as_str() {
            "mail" => format!("M{}", cart.slot_id),
            "drive" => format!("D{}", cart.slot_id),
            _ => cart.slot_id.to_string(),
        };
        let location_disp = match cart.location.as_str() {
            "storage" => "Storage",
            "mail" => "Mail",
            "drive" => "Drive",
            other => other,
        };
        table.add_row(vec![
            cart.barcode.clone(),
            location_disp.to_string(),
            slot_str,
        ]);
    }

    println!("Cartridges ({} total)", resp.cartridges.len());
    println!("{table}");
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CartridgeInfoResponse {
    barcode: String,
    location: Option<String>,
    slot_id: Option<u32>,
    backend: String,
    worm: bool,
    total_blocks: usize,
    filemarks: usize,
    data_blocks: usize,
    data_bytes: u64,
    chunk_count: usize,
    host_bytes_written: u64,
    host_bytes_read: u64,
    backend_bytes_written: u64,
    backend_bytes_read: u64,
}

pub async fn cmd_info(identifier: &str, json: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = format!("/api/v1/cartridges/{}", identifier);
    let info: CartridgeInfoResponse = client.get_json(&path).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    println!();
    println!("Cartridge Information");
    println!("---------------------");
    println!("Barcode: {}", info.barcode);
    if let (Some(loc), Some(slot)) = (&info.location, info.slot_id) {
        let loc_disp = match loc.as_str() {
            "storage" => "Storage",
            "mail" => "Mail",
            "drive" => "Drive",
            other => other,
        };
        println!("Location: {} slot {}", loc_disp, slot);
    }
    println!("Cloud backend: {}", info.backend);
    println!("WORM: {}", if info.worm { "yes" } else { "no" });
    println!("Total blocks: {}", info.total_blocks);
    println!("Filemarks: {}", info.filemarks);
    println!("Data blocks: {}", info.data_blocks);
    println!("Data size: {}", format_bytes(info.data_bytes));
    println!("Chunks: {}", info.chunk_count);
    println!(
        "Host bytes written: {}",
        format_bytes(info.host_bytes_written)
    );
    println!("Host bytes read: {}", format_bytes(info.host_bytes_read));
    println!(
        "Backend bytes written: {}",
        with_host_ratio(info.backend_bytes_written, info.host_bytes_written)
    );
    println!(
        "Backend bytes read: {}",
        with_host_ratio(info.backend_bytes_read, info.host_bytes_read)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// legal-hold — thin daemon-call wrappers
//
// The daemon owns the cloud handle (lazy-initialized via storage_config),
// applies the per-object hold using the same core-mediachanger helpers
// the CLI used to call directly, refuses if the cartridge is loaded,
// and emits the audit entry. CLI work is just: serialize request,
// POST/DELETE/GET, render response.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct LegalHoldMutateBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
}

#[derive(serde::Deserialize)]
struct LegalHoldMutateResp {
    #[allow(dead_code)]
    barcode: String,
    backend: String,
    key_count: usize,
    succeeded: usize,
    failed: usize,
    #[allow(dead_code)]
    sentinel_present: bool,
    #[serde(default)]
    failures: Vec<LegalHoldFailureResp>,
}

#[derive(serde::Deserialize)]
struct LegalHoldFailureResp {
    key: String,
    error: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
enum LegalHoldStatusResp {
    Sentinel {
        barcode: String,
        backend: String,
        held: bool,
    },
    Full {
        barcode: String,
        backend: String,
        held: usize,
        not_held: usize,
        errors: usize,
        details: Vec<LegalHoldKeyStateResp>,
    },
    Empty {
        barcode: String,
        #[allow(dead_code)]
        backend: String,
    },
}

#[derive(serde::Deserialize)]
struct LegalHoldKeyStateResp {
    key: String,
    state: String,
    #[serde(default)]
    error: Option<String>,
}

pub async fn cmd_legal_hold_set(
    barcode: &str,
    reason: Option<&str>,
    id: Option<&str>,
) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = format!(
        "/api/v1/cartridges/{}/legal-hold",
        shared_admin_client::urlencode(barcode)
    );
    let body = LegalHoldMutateBody { reason, id };
    let resp: LegalHoldMutateResp = client.put_json(&path, &body).await?;
    print_legal_hold_mutate("Applied", barcode, &resp);
    if resp.failed > 0 {
        anyhow::bail!("{} of {} keys failed", resp.failed, resp.key_count);
    }
    Ok(())
}

pub async fn cmd_legal_hold_clear(
    barcode: &str,
    reason: Option<&str>,
    id: Option<&str>,
) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = format!(
        "/api/v1/cartridges/{}/legal-hold",
        shared_admin_client::urlencode(barcode)
    );
    let body = LegalHoldMutateBody { reason, id };
    let resp: LegalHoldMutateResp = client.delete_json(&path, Some(&body)).await?;
    print_legal_hold_mutate("Released", barcode, &resp);
    if resp.failed > 0 {
        anyhow::bail!("{} of {} keys failed", resp.failed, resp.key_count);
    }
    Ok(())
}

pub async fn cmd_legal_hold_status(barcode: &str, full: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = format!(
        "/api/v1/cartridges/{}/legal-hold{}",
        shared_admin_client::urlencode(barcode),
        if full { "?full=true" } else { "" }
    );
    let resp: LegalHoldStatusResp = client.get_json(&path).await?;
    match resp {
        LegalHoldStatusResp::Empty { barcode, .. } => {
            println!(
                "Cartridge '{}' has no objects in cloud yet — no hold state to report.",
                barcode
            );
        }
        LegalHoldStatusResp::Sentinel {
            barcode,
            backend,
            held,
        } => {
            if held {
                println!(
                    "Cartridge '{}' on backend '{}': HELD (sentinel: manifests/{}/manifest-latest.json)",
                    barcode, backend, barcode,
                );
            } else {
                println!(
                    "Cartridge '{}' on backend '{}': not held (sentinel read)",
                    barcode, backend
                );
            }
        }
        LegalHoldStatusResp::Full {
            barcode,
            backend,
            held,
            not_held,
            errors,
            details,
        } => {
            for d in &details {
                let label = match d.state.as_str() {
                    "held" => "HELD     ",
                    "not_held" => "not held ",
                    _ => "ERROR    ",
                };
                if let Some(ref e) = d.error {
                    println!("  {}  {}: {}", label, d.key, e);
                } else {
                    println!("  {}  {}", label, d.key);
                }
            }
            let total = held + not_held + errors;
            println!();
            println!(
                "Cartridge '{}' on backend '{}': {}/{} keys held, {} not held, {} errors (full sweep)",
                barcode, backend, held, total, not_held, errors,
            );
            if errors > 0 {
                anyhow::bail!("{} key(s) returned errors during status read", errors);
            }
        }
    }
    Ok(())
}

fn print_legal_hold_mutate(verb: &str, barcode: &str, resp: &LegalHoldMutateResp) {
    println!(
        "{} legal hold for cartridge '{}' on backend '{}': {}/{} succeeded, {} failed.",
        verb, barcode, resp.backend, resp.succeeded, resp.key_count, resp.failed,
    );
    for f in &resp.failures {
        eprintln!("  [FAIL] {}: {}", f.key, f.error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_admin_proto::JobEvent;

    #[test]
    fn cartridge_defaults() {
        assert_eq!(default_cartridge_chunk_size_mb(), 8);
        assert_eq!(default_cartridge_chunking(), "fastcdc");
        assert_eq!(default_cartridge_dedup(), "global");
    }

    #[test]
    fn render_job_event_handles_every_variant() {
        // Smoke: each arm should not panic.
        render_job_event(JobEvent::Log {
            level: "info".to_string(),
            message: "hello".to_string(),
        });
        render_job_event(JobEvent::Log {
            level: "warn".to_string(),
            message: "careful".to_string(),
        });
        render_job_event(JobEvent::Done {
            exit_code: 1,
            error: Some("boom".to_string()),
        });
        render_job_event(JobEvent::Done {
            exit_code: 0,
            error: None,
        });
        render_job_event(JobEvent::Progress {
            stage: "upload".to_string(),
            current: 1,
            total: Some(2),
        });
    }

    #[test]
    fn print_legal_hold_mutate_with_failures() {
        let resp: LegalHoldMutateResp = serde_json::from_value(serde_json::json!({
            "barcode": "TAPE001",
            "backend": "s3b",
            "key_count": 3,
            "succeeded": 2,
            "failed": 1,
            "sentinel_present": true,
            "failures": [{"key": "chunks/aa/bb.dat", "error": "access denied"}],
        }))
        .expect("parse mutate resp");
        print_legal_hold_mutate("Engaged", "TAPE001", &resp);
        assert_eq!(resp.failed, 1);
    }

    #[test]
    fn print_legal_hold_mutate_all_succeeded() {
        let resp: LegalHoldMutateResp = serde_json::from_value(serde_json::json!({
            "barcode": "TAPE001",
            "backend": "s3b",
            "key_count": 5,
            "succeeded": 5,
            "failed": 0,
            "sentinel_present": true,
        }))
        .expect("parse mutate resp");
        print_legal_hold_mutate("Released", "TAPE001", &resp);
        assert!(resp.failures.is_empty());
    }

    #[test]
    fn cartridge_list_response_round_trips() {
        let resp = CartridgeListResponse {
            cartridges: vec![CartridgeListResponseItem {
                barcode: "TAPE001".to_string(),
                location: "storage".to_string(),
                slot_id: 3,
            }],
        };
        let json = serde_json::to_value(&resp).expect("serialize");
        let back: CartridgeListResponse = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.cartridges.len(), 1);
        assert_eq!(back.cartridges[0].barcode, "TAPE001");
    }

    #[test]
    fn legal_hold_status_resp_deserializes_each_mode() {
        let sentinel: LegalHoldStatusResp = serde_json::from_value(serde_json::json!({
            "mode": "sentinel",
            "barcode": "T1",
            "backend": "s3b",
            "held": true,
        }))
        .expect("sentinel mode");
        assert!(matches!(
            sentinel,
            LegalHoldStatusResp::Sentinel { held: true, .. }
        ));

        let empty: LegalHoldStatusResp = serde_json::from_value(serde_json::json!({
            "mode": "empty",
            "barcode": "T1",
            "backend": "s3b",
        }))
        .expect("empty mode");
        assert!(matches!(empty, LegalHoldStatusResp::Empty { .. }));

        let full: LegalHoldStatusResp = serde_json::from_value(serde_json::json!({
            "mode": "full",
            "barcode": "T1",
            "backend": "s3b",
            "held": 4,
            "not_held": 1,
            "errors": 0,
            "details": [{"key": "chunks/aa.dat", "state": "held"}],
        }))
        .expect("full mode");
        assert!(matches!(
            full,
            LegalHoldStatusResp::Full {
                held: 4,
                not_held: 1,
                ..
            }
        ));
    }
}
