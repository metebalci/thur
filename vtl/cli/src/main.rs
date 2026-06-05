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
                action: LibraryAction::Partition { .. } | LibraryAction::Restore { .. }
            }
        ) {
            return true;
        }
        // `system storage benchmark` reads the YAML `storage.backends:`
        // block directly and contacts the backend SDKs — no admin
        // socket round-trip. Lets operators validate a backend
        // pre-daemon-start.
        if matches!(
            self.command,
            Commands::System {
                action: SystemAction::Storage {
                    action: StorageAction::Benchmark { .. }
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

    // Strict split: daemon-down commands (`library partition *`,
    // `library restore`) read the yaml — they need `data_dir` to
    // write under and run with `sudo` on a packaged install so the
    // 0640 conffile is readable. Every other command is daemon-routed
    // and only talks to the admin Unix socket; it must not touch the
    // yaml at all.
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
            CartridgeAction::ResetStats { barcode } => {
                commands::cartridge::cmd_reset_stats(&barcode).await?;
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
            DriveAction::ResetStats { drive, all } => {
                commands::drive::cmd_reset_stats(drive, all).await?;
            }
        },
        Commands::Library { action } => match action {
            LibraryAction::Info {
                json,
                with_cartridges,
            } => {
                commands::library::cmd_info(json, with_cartridges).await?;
            }
            LibraryAction::Bounds { json } => {
                commands::library::cmd_bounds(json).await?;
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
            SystemAction::Gc { dry_run, storage } => {
                shared_cli_system::cmd_gc(&shared_naming::TAPE_LIBRARY, dry_run, storage).await?;
            }
            SystemAction::Storage { action } => match action {
                StorageAction::Check => {
                    shared_cli_system::cmd_storage_check(&shared_naming::TAPE_LIBRARY).await?;
                }
                StorageAction::Benchmark {
                    backends,
                    total_gb,
                    chunk_size_mb,
                    concurrency,
                    chunk_size_mb_sweep,
                    concurrency_sweep,
                    skip_download,
                    yes,
                } => {
                    commands::storage::cmd_benchmark(
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
            SystemAction::Monitor => {
                let code = shared_cli_system::cmd_monitor(&shared_naming::TAPE_LIBRARY).await?;
                std::process::exit(i32::from(code));
            }
            SystemAction::ResetStats => {
                commands::stats::cmd_reset_stats().await?;
            }
            SystemAction::Verify {
                skip_storage,
                verbose,
                json,
                barcodes,
            } => {
                let code =
                    commands::verify::cmd_verify(skip_storage, verbose, json, barcodes).await?;
                std::process::exit(i32::from(code));
            }
            SystemAction::Tiering { action } => match action {
                TieringAction::Plan { json } => {
                    let code = commands::tiering::cmd_tiering_plan(json).await?;
                    std::process::exit(i32::from(code));
                }
                TieringAction::RunNow { json } => {
                    let code = commands::tiering::cmd_tiering_run(json).await?;
                    std::process::exit(i32::from(code));
                }
                TieringAction::Status { json } => {
                    let code = commands::tiering::cmd_tiering_status(json).await?;
                    std::process::exit(i32::from(code));
                }
            },
            SystemAction::RegenerateCert => {
                shared_cli_system::cmd_regenerate_cert(
                    &shared_naming::TAPE_LIBRARY,
                    std::path::Path::new(&config_path),
                )
                .await?;
            }
            SystemAction::SetAdminPassword => {
                shared_cli_system::cmd_set_admin_password(&shared_naming::TAPE_LIBRARY).await?;
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

#[cfg(test)]
mod cli_parse_tests {
    use super::*;
    use clap::Parser;

    /// Helper: parse an argv array into a `Cli`. `Cli` doesn't derive
    /// `Debug`, so `Result::expect` isn't available; go through
    /// `Result::ok()` + `Option::expect`, which has no `Debug` bound.
    /// `clippy::ok_expect` would suggest `Result::expect`, which the
    /// missing `Debug` impl rules out — hence the targeted allow.
    #[allow(clippy::ok_expect)]
    fn parse<const N: usize>(args: [&str; N]) -> Cli {
        Cli::try_parse_from(args).ok().expect("argv must parse")
    }

    #[test]
    fn bare_binary_with_no_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["thurvtl"]).is_err());
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["thurvtl", "frobnicate"]).is_err());
    }

    #[test]
    fn help_flag_short_circuits() {
        // clap returns an Err whose kind is DisplayHelp for `--help`.
        // `Cli` has no `Debug`, so `unwrap_err` is unavailable — read
        // the error kind through `.err()`.
        let kind = Cli::try_parse_from(["thurvtl", "--help"])
            .err()
            .map(|e| e.kind());
        assert_eq!(kind, Some(clap::error::ErrorKind::DisplayHelp));
    }

    #[test]
    fn version_flag_short_circuits() {
        let kind = Cli::try_parse_from(["thurvtl", "--version"])
            .err()
            .map(|e| e.kind());
        assert_eq!(kind, Some(clap::error::ErrorKind::DisplayVersion));
    }

    #[test]
    fn global_config_flag_is_captured() {
        let cli = parse(["thurvtl", "--config", "/etc/x.yaml", "library", "info"]);
        assert_eq!(cli.args.config.as_deref(), Some("/etc/x.yaml"));
        assert_eq!(cli.get_config_path(), "/etc/x.yaml");
    }

    #[test]
    fn config_path_defaults_to_etc() {
        let cli = parse(["thurvtl", "library", "info"]);
        assert!(cli.args.config.is_none());
        assert_eq!(
            cli.get_config_path(),
            shared_naming::TAPE_LIBRARY.config_path
        );
    }

    #[test]
    fn target_user_override_and_default() {
        let cli = parse(["thurvtl", "--user", "operator", "library", "info"]);
        assert_eq!(cli.target_user(), "operator");
        let cli = parse(["thurvtl", "library", "info"]);
        assert_eq!(cli.target_user(), shared_naming::TAPE_LIBRARY.system_user);
    }

    // ---- daemon-down classification ----

    #[test]
    fn library_partition_is_daemon_down() {
        let cli = parse(["thurvtl", "library", "partition", "list"]);
        assert!(cli.is_daemon_down());
    }

    #[test]
    fn library_restore_is_daemon_down() {
        let cli = parse(["thurvtl", "library", "restore", "--dry-run"]);
        assert!(cli.is_daemon_down());
    }

    #[test]
    fn storage_benchmark_is_daemon_down() {
        let cli = parse(["thurvtl", "system", "storage", "benchmark"]);
        assert!(cli.is_daemon_down());
    }

    #[test]
    fn regenerate_cert_is_daemon_down() {
        let cli = parse(["thurvtl", "system", "regenerate-cert"]);
        assert!(cli.is_daemon_down());
    }

    #[test]
    fn cartridge_key_is_daemon_down() {
        let cli = parse(["thurvtl", "cartridge", "key", "show", "TAPE001"]);
        assert!(cli.is_daemon_down());
    }

    #[test]
    fn library_info_is_daemon_routed() {
        let cli = parse(["thurvtl", "library", "info"]);
        assert!(!cli.is_daemon_down());
    }

    #[test]
    fn cartridge_list_is_daemon_routed() {
        let cli = parse(["thurvtl", "cartridge", "list"]);
        assert!(!cli.is_daemon_down());
    }

    #[test]
    fn system_gc_is_daemon_routed() {
        let cli = parse(["thurvtl", "system", "gc"]);
        assert!(!cli.is_daemon_down());
    }

    // ---- library bounds is daemon-routed (chassis topology lives in YAML;
    // CLI verbs to mutate the chassis no longer exist).

    #[test]
    fn library_init_and_modify_are_no_longer_subcommands() {
        // Chassis topology now lives in thurvtl.yaml's `library:`
        // block; the daemon materializes library.json on first start
        // and reconciles on every subsequent start. The imperative
        // `library init` / `library modify` verbs were removed.
        assert!(Cli::try_parse_from(["thurvtl", "library", "init"]).is_err());
        assert!(Cli::try_parse_from(["thurvtl", "library", "modify"]).is_err());
    }

    #[test]
    fn library_bounds_is_daemon_routed() {
        let cli = parse(["thurvtl", "library", "bounds"]);
        assert!(!cli.is_daemon_down());
    }

    // ---- cartridge create flag validation ----

    #[test]
    fn cartridge_create_minimal() {
        let cli = parse(["thurvtl", "cartridge", "create", "TAPE001"]);
        assert!(matches!(
            cli.command,
            Commands::Cartridge {
                action: CartridgeAction::Create {
                    ref barcode,
                    multi: 1,
                    worm: false,
                    ..
                },
            } if barcode == "TAPE001"
        ));
    }

    #[test]
    fn cartridge_create_rejects_zero_multi() {
        assert!(
            Cli::try_parse_from(["thurvtl", "cartridge", "create", "T1", "--multi", "0"]).is_err()
        );
    }

    #[test]
    fn cartridge_create_encrypt_requires_keystore() {
        assert!(
            Cli::try_parse_from(["thurvtl", "cartridge", "create", "T1", "--encrypt"]).is_err()
        );
    }

    #[test]
    fn cartridge_create_encrypt_with_keystore_ok() {
        let cli = parse([
            "thurvtl",
            "cartridge",
            "create",
            "T1",
            "--encrypt",
            "--keystore",
            "kms1",
        ]);
        assert!(matches!(
            cli.command,
            Commands::Cartridge {
                action: CartridgeAction::Create {
                    encrypt: true,
                    ref keystore,
                    ..
                },
            } if keystore.as_deref() == Some("kms1")
        ));
    }

    #[test]
    fn cartridge_create_rejects_bad_chunking() {
        assert!(
            Cli::try_parse_from([
                "thurvtl",
                "cartridge",
                "create",
                "T1",
                "--chunking",
                "bogus",
            ])
            .is_err()
        );
    }

    #[test]
    fn cartridge_create_rejects_bad_dedup_scope() {
        assert!(
            Cli::try_parse_from([
                "thurvtl",
                "cartridge",
                "create",
                "T1",
                "--dedup",
                "regional"
            ])
            .is_err()
        );
    }

    #[test]
    fn cartridge_create_rejects_lto_generation_9() {
        assert!(
            Cli::try_parse_from([
                "thurvtl",
                "cartridge",
                "create",
                "T1",
                "--lto-generation",
                "9",
            ])
            .is_err()
        );
    }

    // ---- changer ----

    #[test]
    fn changer_move_parses_slots() {
        let cli = parse(["thurvtl", "changer", "move", "3", "9"]);
        assert!(matches!(
            cli.command,
            Commands::Changer {
                action: ChangerAction::Move {
                    from_slot: 3,
                    to_slot: 9,
                    cross_partition: false,
                },
            }
        ));
    }

    #[test]
    fn changer_unload_slot_is_optional() {
        let cli = parse(["thurvtl", "changer", "unload", "1", "--force"]);
        assert!(matches!(
            cli.command,
            Commands::Changer {
                action: ChangerAction::Unload {
                    drive: 1,
                    slot: None,
                    force: true,
                    ..
                },
            }
        ));
    }

    #[test]
    fn changer_move_rejects_non_numeric_slot() {
        assert!(Cli::try_parse_from(["thurvtl", "changer", "move", "abc", "9"]).is_err());
    }

    // ---- drive ----

    #[test]
    fn drive_status_parses_id() {
        let cli = parse(["thurvtl", "drive", "status", "2", "--json"]);
        assert!(matches!(
            cli.command,
            Commands::Drive {
                action: DriveAction::Status {
                    drive: 2,
                    json: true
                },
            }
        ));
    }

    #[test]
    fn drive_self_test_parses_id() {
        let cli = parse(["thurvtl", "drive", "self-test", "0"]);
        assert!(matches!(
            cli.command,
            Commands::Drive {
                action: DriveAction::SelfTest {
                    drive: 0,
                    json: false
                }
            }
        ));
    }

    // ---- reset-stats ----

    #[test]
    fn drive_reset_stats_parses_id_and_all() {
        let cli = parse(["thurvtl", "drive", "reset-stats", "1"]);
        assert!(matches!(
            cli.command,
            Commands::Drive {
                action: DriveAction::ResetStats {
                    drive: Some(1),
                    all: false
                }
            }
        ));
        let all = parse(["thurvtl", "drive", "reset-stats", "--all"]);
        assert!(matches!(
            all.command,
            Commands::Drive {
                action: DriveAction::ResetStats {
                    drive: None,
                    all: true
                }
            }
        ));
    }

    #[test]
    fn cartridge_reset_stats_parses_barcode() {
        let cli = parse(["thurvtl", "cartridge", "reset-stats", "TST001L8"]);
        assert!(matches!(
            cli.command,
            Commands::Cartridge {
                action: CartridgeAction::ResetStats { barcode },
            } if barcode == "TST001L8"
        ));
    }

    #[test]
    fn cartridge_reset_stats_requires_barcode() {
        assert!(Cli::try_parse_from(["thurvtl", "cartridge", "reset-stats"]).is_err());
    }

    #[test]
    fn system_reset_stats_parses() {
        let cli = parse(["thurvtl", "system", "reset-stats"]);
        assert!(matches!(
            cli.command,
            Commands::System {
                action: SystemAction::ResetStats,
            }
        ));
    }

    // ---- system audit ----

    #[test]
    fn audit_export_rejects_bad_format() {
        assert!(
            Cli::try_parse_from(["thurvtl", "system", "audit", "export", "--format", "xml"])
                .is_err()
        );
    }

    #[test]
    fn audit_export_default_format_is_jsonl() {
        let cli = parse(["thurvtl", "system", "audit", "export"]);
        assert!(matches!(
            cli.command,
            Commands::System {
                action: SystemAction::Audit {
                    action: AuditAction::Export { ref format, .. },
                },
            } if format == "jsonl"
        ));
    }

    #[test]
    fn audit_tail_default_lines_is_twenty() {
        let cli = parse(["thurvtl", "system", "audit", "tail"]);
        assert!(matches!(
            cli.command,
            Commands::System {
                action: SystemAction::Audit {
                    action: AuditAction::Tail {
                        follow: false,
                        lines: 20
                    },
                },
            }
        ));
    }

    #[test]
    fn audit_verify_offline_requires_dir() {
        assert!(Cli::try_parse_from(["thurvtl", "system", "audit", "verify-offline"]).is_err());
    }

    // ---- iscsi ----

    #[test]
    fn iscsi_users_add_password_conflicts_with_stdin() {
        assert!(
            Cli::try_parse_from([
                "thurvtl",
                "iscsi",
                "users",
                "add",
                "alice",
                "--password",
                "p",
                "--password-stdin",
            ])
            .is_err()
        );
    }

    #[test]
    fn iscsi_users_add_minimal() {
        let cli = parse(["thurvtl", "iscsi", "users", "add", "alice"]);
        assert!(matches!(
            cli.command,
            Commands::Iscsi {
                action: IscsiAction::Users {
                    action: IscsiUsersAction::Add { ref name, mutual_chap: false, .. },
                },
            } if name == "alice"
        ));
    }

    #[test]
    fn iscsi_users_rotate_default_grace() {
        let cli = parse(["thurvtl", "iscsi", "users", "rotate", "bob"]);
        assert!(matches!(
            cli.command,
            Commands::Iscsi {
                action: IscsiAction::Users {
                    action: IscsiUsersAction::Rotate { ref grace, cancel: false, .. },
                },
            } if grace == "24h"
        ));
    }

    #[test]
    fn iscsi_target_set_requires_username() {
        assert!(Cli::try_parse_from(["thurvtl", "iscsi", "target", "set"]).is_err());
    }

    // ---- cartridge migrate / archive ----

    #[test]
    fn cartridge_migrate_default_mode_is_move() {
        let cli = parse([
            "thurvtl",
            "cartridge",
            "migrate",
            "T1",
            "--target-backend",
            "s3b",
        ]);
        assert!(matches!(
            cli.command,
            Commands::Cartridge {
                action: CartridgeAction::Migrate {
                    ref mode,
                    no_verify: false,
                    dry_run: false,
                    ..
                },
            } if mode == "move"
        ));
    }

    #[test]
    fn cartridge_migrate_rejects_bad_mode() {
        assert!(
            Cli::try_parse_from([
                "thurvtl",
                "cartridge",
                "migrate",
                "T1",
                "--target-backend",
                "s3b",
                "--mode",
                "teleport",
            ])
            .is_err()
        );
    }

    #[test]
    fn cartridge_archive_requires_target_backend() {
        assert!(Cli::try_parse_from(["thurvtl", "cartridge", "archive", "T1"]).is_err());
    }

    // ---- config subcommand ----

    #[test]
    fn config_defaults_parses() {
        let cli = parse(["thurvtl", "config", "defaults"]);
        assert!(matches!(cli.command, Commands::Config { .. }));
    }

    // ---- library partition create ----

    #[test]
    fn partition_create_parses_drive_list() {
        let cli = parse([
            "thurvtl",
            "library",
            "partition",
            "create",
            "p1",
            "--storage-start",
            "0",
            "--storage-end",
            "20",
            "--mail-start",
            "0",
            "--mail-end",
            "1",
            "--drives",
            "0,1,2",
        ]);
        assert!(matches!(
            cli.command,
            Commands::Library {
                action: LibraryAction::Partition {
                    action: PartitionAction::Create {
                        ref name,
                        storage_start: 0,
                        storage_end: 20,
                        mail_start: 0,
                        mail_end: 1,
                        ref drives,
                        ..
                    },
                },
            } if name == "p1" && *drives == [0, 1, 2]
        ));
    }

    #[test]
    fn partition_create_defaults_mail_to_empty() {
        // Mail-slot flags are optional; omitting them yields the empty
        // range [0, 0). Used by the second-and-later partition when
        // partition 1 already claims the mail slot.
        let cli = parse([
            "thurvtl",
            "library",
            "partition",
            "create",
            "p2",
            "--storage-start",
            "20",
            "--storage-end",
            "40",
            "--drives",
            "3",
        ]);
        assert!(matches!(
            cli.command,
            Commands::Library {
                action: LibraryAction::Partition {
                    action: PartitionAction::Create {
                        mail_start: 0,
                        mail_end: 0,
                        ..
                    },
                },
            }
        ));
    }
}
