// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

mod audit_helper;
mod commands;
mod output;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

/// Version from Cargo.toml
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// CLI definition (Cli + every subcommand enum) lives in cli.rs so the
// build script can include the same file and feed it to clap_complete
// for auto-regenerated shell completion. Keep main.rs slim — anything
// new that touches the user-facing CLI surface goes in cli.rs.
include!("cli.rs");

// MinimalConfig + impl Cli helpers stay in main.rs because they
// pull in serde_yaml / VERSION — neither of which the build script
// needs (and pulling them in via include! would drag the universe
// into build-dependencies).

/// Minimal config structure: just the top-level fields the CLI needs
/// before dispatching to a subcommand.
#[derive(Debug, Deserialize)]
struct MinimalConfig {
    data_dir: String,
}

impl Cli {
    /// Resolve the config path. `--config PATH` wins; otherwise the
    /// production location at `/etc/thurvtl/thurvtl.yaml`. We
    /// deliberately don't fall back to `./thurvtl.yaml` — devs
    /// running outside `/etc/thurvtl/` should pass `--config`
    /// explicitly so the loaded config is unambiguous in logs.
    fn get_config_path(&self) -> String {
        if let Some(ref path) = self.args.config {
            return path.clone();
        }
        shared_naming::TAPE_LIBRARY.config_path.to_string()
    }

    /// Resolve the `--user` target. Falls back to the product's
    /// system user when the operator didn't override.
    fn target_user(&self) -> &str {
        self.args
            .user
            .as_deref()
            .unwrap_or(shared_naming::TAPE_LIBRARY.system_user)
    }

    /// Read the minimal config block (data_dir).
    fn read_minimal(config_path: &str) -> Result<MinimalConfig> {
        let config_content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path))?;
        serde_yaml::from_str(&config_content)
            .with_context(|| format!("Failed to parse config file: {}", config_path))
    }

    /// True for commands that read or write `thurvtl.yaml` /
    /// `<data_dir>` directly (chassis assembly + DR). These run with
    /// `sudo` on a packaged install so they can read the 0640
    /// conffile and write under `data_dir`. Everything else is
    /// daemon-routed and only touches the admin Unix socket — the
    /// CLI does **not** read the yaml in that case.
    fn is_daemon_down(&self) -> bool {
        if matches!(
            self.command,
            Commands::Library {
                action: LibraryAction::Init { .. }
                    | LibraryAction::Modify { .. }
                    | LibraryAction::Partition { .. }
                    | LibraryAction::Restore { .. }
            }
        ) {
            return true;
        }
        // `system cloud benchmark` reads <data_dir>/cloud-backends.json
        // directly and contacts the cloud SDKs — no admin socket
        // round-trip. Lets operators validate a backend pre-
        // daemon-start.
        if matches!(
            self.command,
            Commands::System {
                action: SystemAction::Cloud {
                    action: CloudAction::Benchmark { .. }
                }
            }
        ) {
            return true;
        }
        // `system regenerate-cert` reads `http.tls` from the yaml and
        // rewrites the cert/key files in place — daemon-down so the
        // listener isn't serving a cert mid-rewrite.
        if matches!(
            self.command,
            Commands::System {
                action: SystemAction::RegenerateCert
            }
        ) {
            return true;
        }
        // `cartridge key {migrate, show}` reads / rewrites
        // `<data_dir>/tapes/<barcode>/manifest.json` and
        // (un)wraps via the keystore — daemon must be stopped so
        // the live cartridge-load path doesn't pick up a half-
        // rewritten manifest. The yaml load surfaces `data_dir`
        // for the manifest path; the `keystore.backends:` block is
        // pulled from the same YAML conffile.
        matches!(
            self.command,
            Commands::Cartridge {
                action: CartridgeAction::Key { .. }
            }
        )
    }

    /// Print the runtime header for daemon-routed commands. Doesn't
    /// touch yaml — the CLI doesn't have permission to in the
    /// general case (conffile is 0640 root:thurvtl).
    fn print_runtime_header() {
        eprintln!("Version: v{}", VERSION);
        let socket = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY)
            .socket_path()
            .display()
            .to_string();
        eprintln!("Admin socket: {}", socket);
        eprintln!();
    }

    /// Print configuration header
    fn print_header(config_path: &str, data_dir: &str) {
        eprintln!("Version: v{}", VERSION);
        eprintln!("Config: {}", config_path);
        eprintln!("Data directory: {}", data_dir);
        eprintln!();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Pre-parse intercept for `--copyright`. Lets the flag work
    // without a subcommand (clap would otherwise reject for
    // missing one).
    shared_cli::handle_copyright_flag();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config_path = cli.get_config_path();

    // `config` doesn't read the config file; everything else needs
    // `data_dir` loaded from yaml.
    if matches!(cli.command, Commands::Config { .. }) {
        match cli.command {
            Commands::Config { action } => match action {
                shared_cli::ConfigAction::Defaults => {
                    shared_cli::emit_defaults(commands::generate_config::reference_content());
                }
                shared_cli::ConfigAction::SystemdUnit => {
                    shared_cli::emit_systemd_unit(commands::generate_config::systemd_unit_content());
                }
                shared_cli::ConfigAction::Completion { shell } => {
                    use clap::CommandFactory;
                    let mut cmd = Cli::command();
                    shared_cli::emit_completion(&mut cmd, shell)?;
                }
            },
            _ => unreachable!(),
        }
        return Ok(());
    }

    // Strict split: daemon-down commands (`library init`,
    // `library modify`, `library partition *`) read the yaml — they
    // need `data_dir` to write under and run with `sudo` on a
    // packaged install so the 0640 conffile is readable. Every
    // other command is daemon-routed and only talks to the admin
    // Unix socket; it must not touch the yaml at all.
    let data_dir = if cli.is_daemon_down() {
        Cli::read_minimal(&config_path)?.data_dir
    } else {
        String::new()
    };
    let target_user = cli.target_user().to_string();

    if cli.is_daemon_down() {
        Cli::print_header(&config_path, &data_dir);
    } else {
        Cli::print_runtime_header();
    }

    match cli.command {
        Commands::Changer { action } => match action {
            ChangerAction::Inventory { filter, json } => {
                commands::library::cmd_inventory(filter, json).await?;
            }
            ChangerAction::Load {
                slot,
                drive,
                cross_partition,
            } => {
                commands::library::cmd_load(slot, drive, cross_partition).await?;
            }
            ChangerAction::Unload {
                drive,
                slot,
                force,
                cross_partition,
            } => {
                commands::library::cmd_unload(drive, slot, force, cross_partition).await?;
            }
            ChangerAction::Move {
                from_slot,
                to_slot,
                cross_partition,
            } => {
                commands::library::cmd_move(from_slot, to_slot, cross_partition).await?;
            }
        },
        Commands::Cartridge { action } => match action {
            CartridgeAction::Create {
                barcode,
                lto_generation,
                chunk_size_mb,
                chunking,
                chunking_min_kb,
                chunking_max_kb,
                multi,
                backend,
                worm,
                dedup,
                encrypt,
                keystore,
            } => {
                commands::cartridge::cmd_create(
                    &barcode,
                    lto_generation,
                    chunk_size_mb,
                    chunking.as_deref(),
                    chunking_min_kb,
                    chunking_max_kb,
                    multi,
                    backend.as_deref(),
                    worm,
                    dedup.as_deref(),
                    encrypt,
                    keystore.as_deref(),
                )
                .await?;
            }
            CartridgeAction::Archive {
                barcode,
                target_backend,
                label,
                dry_run,
            } => {
                let exit = commands::cartridge::cmd_archive(
                    &barcode,
                    &target_backend,
                    label.as_deref(),
                    dry_run,
                )
                .await?;
                if exit != 0 {
                    std::process::exit(exit);
                }
            }
            CartridgeAction::Migrate {
                barcode,
                target_backend,
                mode,
                no_verify,
                dry_run,
            } => {
                let exit = commands::cartridge::cmd_migrate(
                    &barcode,
                    &target_backend,
                    &mode,
                    !no_verify,
                    dry_run,
                )
                .await?;
                if exit != 0 {
                    std::process::exit(exit);
                }
            }
            CartridgeAction::Import { path, slot } => {
                commands::cartridge::cmd_import(&path, slot).await?;
            }
            CartridgeAction::Export { slot, path } => {
                commands::cartridge::cmd_export(slot, &path).await?;
            }
            CartridgeAction::List { json } => {
                commands::cartridge::cmd_list(json).await?;
            }
            CartridgeAction::Info { identifier, json } => {
                commands::cartridge::cmd_info(&identifier, json).await?;
            }
            CartridgeAction::LegalHold { action } => match action {
                LegalHoldAction::Set {
                    barcode,
                    id,
                    reason,
                } => {
                    commands::cartridge::cmd_legal_hold_set(
                        &barcode,
                        reason.as_deref(),
                        id.as_deref(),
                    )
                    .await?;
                }
                LegalHoldAction::Clear {
                    barcode,
                    id,
                    reason,
                } => {
                    commands::cartridge::cmd_legal_hold_clear(
                        &barcode,
                        reason.as_deref(),
                        id.as_deref(),
                    )
                    .await?;
                }
                LegalHoldAction::Status { barcode, full } => {
                    commands::cartridge::cmd_legal_hold_status(&barcode, full).await?;
                }
            },
            CartridgeAction::Key { action } => match action {
                CartridgeKeyAction::Migrate {
                    barcode,
                    to,
                    purge_local,
                } => {
                    commands::cartridge_key::cmd_key_migrate(
                        std::path::Path::new(&data_dir),
                        std::path::Path::new(&config_path),
                        &barcode,
                        &to,
                        purge_local,
                    )
                    .await?;
                }
                CartridgeKeyAction::Show { barcode } => {
                    commands::cartridge_key::cmd_key_show(
                        std::path::Path::new(&data_dir),
                        &barcode,
                    )
                    .await?;
                }
            },
        },
        Commands::Drive { action } => match action {
            DriveAction::Status { drive, json } => {
                commands::drive::cmd_status(drive, json).await?;
            }
            DriveAction::SelfTest { drive, json } => {
                let exit_code = commands::self_test::cmd_drive_self_test(drive, json).await?;
                std::process::exit(exit_code);
            }
        },
        Commands::Library { action } => match action {
            LibraryAction::Init {
                slots,
                mail_slots,
                drives,
                lto_generation,
                firmware,
                transport_base,
                storage_base,
                import_export_base,
                data_transfer_base,
            } => {
                shared_cli::privdrop::drop_to_user_if_root(&target_user)?;
                commands::library::cmd_init(
                    &data_dir,
                    slots,
                    mail_slots,
                    drives,
                    lto_generation,
                    firmware,
                    transport_base,
                    storage_base,
                    import_export_base,
                    data_transfer_base,
                    &config_path,
                )
                .await?;
            }
            LibraryAction::Info {
                json,
                with_cartridges,
            } => {
                commands::library::cmd_info(json, with_cartridges).await?;
            }
            LibraryAction::Modify {
                slots,
                mail_slots,
                drives,
                lto_generation,
                firmware,
            } => {
                shared_cli::privdrop::drop_to_user_if_root(&target_user)?;
                commands::library::cmd_modify(
                    &data_dir,
                    slots,
                    mail_slots,
                    drives,
                    lto_generation,
                    firmware,
                    &config_path,
                )
                .await?;
            }
            LibraryAction::Restore {
                backend,
                barcodes,
                dry_run,
                allow_existing,
            } => {
                shared_cli::privdrop::drop_to_user_if_root(&target_user)?;
                let exit_code = commands::library::cmd_restore(
                    &data_dir,
                    &config_path,
                    backend.as_deref(),
                    barcodes,
                    dry_run,
                    allow_existing,
                )
                .await?;
                std::process::exit(exit_code);
            }
            LibraryAction::RestoreArchive {
                backend,
                barcode,
                label,
                as_barcode,
                allow_existing,
                dry_run,
            } => {
                let exit_code = commands::library::cmd_restore_archive(
                    &backend,
                    &barcode,
                    &label,
                    as_barcode.as_deref(),
                    allow_existing,
                    dry_run,
                )
                .await?;
                std::process::exit(exit_code);
            }
            LibraryAction::Monitor { interval } => {
                commands::monitor::cmd_monitor(interval).await?;
            }
            LibraryAction::SelfTest { json } => {
                let exit_code = commands::self_test::cmd_library_self_test(json).await?;
                std::process::exit(exit_code);
            }
            LibraryAction::Partition { action } => match action {
                PartitionAction::List { json } => {
                    commands::library::cmd_partition_list(&data_dir, json).await?;
                }
                PartitionAction::Create {
                    name,
                    storage_start,
                    storage_end,
                    mail_start,
                    mail_end,
                    drives,
                } => {
                    commands::library::cmd_partition_create(
                        &data_dir,
                        &config_path,
                        name,
                        storage_start,
                        storage_end,
                        mail_start,
                        mail_end,
                        drives,
                    )
                    .await?;
                }
                PartitionAction::Modify {
                    name,
                    storage_start,
                    storage_end,
                    mail_start,
                    mail_end,
                    drives,
                } => {
                    commands::library::cmd_partition_modify(
                        &data_dir,
                        &config_path,
                        name,
                        storage_start,
                        storage_end,
                        mail_start,
                        mail_end,
                        drives,
                    )
                    .await?;
                }
                PartitionAction::Delete { name, merge_into } => {
                    commands::library::cmd_partition_delete(
                        &data_dir,
                        &config_path,
                        name,
                        merge_into,
                    )
                    .await?;
                }
            },
        },
        Commands::System { action } => match action {
            SystemAction::Gc { dry_run, cloud } => {
                commands::gc::cmd_gc(dry_run, cloud).await?;
            }
            SystemAction::Cloud { action } => match action {
                CloudAction::Check => {
                    commands::cloud::cmd_check().await?;
                }
                CloudAction::Benchmark {
                    backends,
                    total_gb,
                    chunk_size_mb,
                    concurrency,
                    chunk_size_mb_sweep,
                    concurrency_sweep,
                    skip_download,
                    yes,
                } => {
                    commands::cloud::cmd_benchmark(
                        &config_path,
                        backends,
                        total_gb,
                        chunk_size_mb,
                        concurrency,
                        chunk_size_mb_sweep,
                        concurrency_sweep,
                        skip_download,
                        yes,
                    )
                    .await?;
                }
            },
            SystemAction::Stats { json } => {
                let code = commands::stats::cmd_stats(json).await?;
                std::process::exit(i32::from(code));
            }
            SystemAction::DaemonHealth { json } => {
                shared_cli_system::cmd_daemon_health(&shared_naming::TAPE_LIBRARY, json).await?;
            }
            SystemAction::Verify {
                skip_cloud,
                verbose,
                json,
                barcodes,
            } => {
                let code =
                    commands::verify::cmd_verify(skip_cloud, verbose, json, barcodes).await?;
                std::process::exit(i32::from(code));
            }
            SystemAction::RegenerateCert => {
                shared_cli_system::cmd_regenerate_cert(
                    &shared_naming::TAPE_LIBRARY,
                    std::path::Path::new(&config_path),
                )
                .await?;
            }
            SystemAction::Audit { action } => {
                use shared_cli_system::audit;
                let p = &shared_naming::TAPE_LIBRARY;
                let code = match action {
                    AuditAction::Tail { follow, lines } => {
                        audit::cmd_tail(p, follow, lines).await?
                    }
                    AuditAction::Export { format, from, to } => {
                        audit::cmd_export(p, &format, from.as_deref(), to.as_deref()).await?
                    }
                    AuditAction::Verify => audit::cmd_verify(p).await?,
                    AuditAction::VerifyOffline { dir, json } => {
                        audit::cmd_verify_offline(&dir, json)?
                    }
                    AuditAction::Rotate { accept_break } => {
                        audit::cmd_rotate(p, accept_break).await?
                    }
                };
                std::process::exit(i32::from(code));
            }
            SystemAction::Alerting { action } => match action {
                AlertingAction::List { json } => {
                    shared_cli_alerting::list(&shared_naming::TAPE_LIBRARY, json).await?
                }
                AlertingAction::Test { sink, severity } => {
                    shared_cli_alerting::test(&shared_naming::TAPE_LIBRARY, &sink, &severity)
                        .await?
                }
            },
        },
        Commands::Iscsi { action } => match action {
            IscsiAction::Users { action } => match action {
                IscsiUsersAction::List { json } => commands::credentials::users_list(json).await?,
                IscsiUsersAction::Add {
                    name,
                    password,
                    password_stdin,
                    mutual_chap,
                    partition,
                } => {
                    commands::credentials::users_add(
                        &name,
                        password.as_deref(),
                        password_stdin,
                        mutual_chap,
                        partition.as_deref(),
                    )
                    .await?
                }
                IscsiUsersAction::Remove { name } => {
                    commands::credentials::users_remove(&name).await?
                }
                IscsiUsersAction::Disable { name } => {
                    commands::credentials::users_set_disabled(&name, true).await?
                }
                IscsiUsersAction::Enable { name } => {
                    commands::credentials::users_set_disabled(&name, false).await?
                }
                IscsiUsersAction::Rotate {
                    name,
                    password,
                    password_stdin,
                    grace,
                    cancel,
                } => {
                    if cancel {
                        commands::credentials::users_rotate_cancel(&name).await?
                    } else {
                        commands::credentials::users_rotate(
                            &name,
                            password.as_deref(),
                            password_stdin,
                            &grace,
                        )
                        .await?
                    }
                }
            },
            IscsiAction::Target { action } => match action {
                IscsiTargetAction::Show { json } => {
                    commands::credentials::target_show(json).await?
                }
                IscsiTargetAction::Set {
                    username,
                    password,
                    password_stdin,
                } => {
                    commands::credentials::target_set(
                        &username,
                        password.as_deref(),
                        password_stdin,
                    )
                    .await?
                }
                IscsiTargetAction::Clear => commands::credentials::target_clear().await?,
            },
        },
        Commands::Config { .. } => unreachable!("handled above"),
    }

    Ok(())
}

#[cfg(test)]
mod config_parse_tests {
    use super::*;

    /// Path to the canonical `dist/thurvtl.defaults.yaml`. Same
    /// lookup the in-sync guard rail uses. Layout C puts this crate
    /// at `<workspace>/vtl/cli/`, so the workspace root is two
    /// parents up.
    fn defaults_yaml() -> String {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::PathBuf::from(manifest_dir)
            .parent()
            .and_then(|p| p.parent())
            .expect("thurvtl must live two levels under the repo root")
            .join("dist/thurvtl.defaults.yaml");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
    }

    /// `thurvtl.defaults.yaml` leaves `data_dir` commented out (the
    /// operator must pick a location), so the bare file does not
    /// satisfy `MinimalConfig`. Inject the one required field and
    /// parse — proves the CLI accepts every default key.
    #[test]
    fn cli_parses_defaults_yaml() {
        let yaml = format!("data_dir: /tmp/thur-test\n{}", defaults_yaml());
        let cfg: MinimalConfig =
            serde_yaml::from_str(&yaml).expect("CLI must parse thurvtl.defaults.yaml");
        assert_eq!(cfg.data_dir, "/tmp/thur-test");
    }
}
