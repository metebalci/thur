// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

mod commands;
mod credentials;
mod stats;
mod storage;
mod verify;
mod volume;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

// CLI definition (Cli + every subcommand enum) lives in cli.rs so the
// build script can include the same file and feed it to clap_complete
// for auto-regenerated shell completion. Keep main.rs slim — anything
// new that touches the user-facing CLI surface goes in cli.rs.
include!("cli.rs");

#[derive(Debug, Deserialize)]
struct MinimalConfig {
    data_dir: String,
}

impl MinimalConfig {
    fn load(path: &str) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read config file: {path}"))?;
        serde_yaml::from_str(&raw).with_context(|| format!("parse config file: {path}"))
    }
}

impl Cli {
    /// Resolve the config path. `--config PATH` wins; otherwise the
    /// production location at `/etc/thurvsa/thurvsa.yaml`.
    fn get_config_path(&self) -> String {
        if let Some(ref path) = self.args.config {
            return path.clone();
        }
        shared_naming::DISK.config_path.to_string()
    }

    /// Resolve the `--user` target. Falls back to the product's
    /// system user when the operator didn't override.
    fn target_user(&self) -> &str {
        self.args
            .user
            .as_deref()
            .unwrap_or(shared_naming::DISK.system_user)
    }

    /// True for commands that write under `data_dir` directly while
    /// the daemon is down. Under `sudo`, those files would otherwise
    /// be owned by root and unreadable by the daemon. Only `volume
    /// key` rewrites on-disk state offline; the rest of the CLI
    /// surface (`volume create` / `list` / `info`, `iscsi`,
    /// `nvmetcp`) is daemon-routed and never privdrops.
    fn is_daemon_down(&self) -> bool {
        matches!(
            self.command,
            Commands::Volume {
                action: VolumeAction::Key { .. }
            } | Commands::System {
                action: SystemAction::RegenerateCert
            }
        )
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Local-only commands — `config defaults` / `config systemd-unit`
    // / `config completion` — emit checked-in artifacts on stdout.
    // They don't read the config file or talk to the daemon, so we
    // dispatch them before the tokio runtime + admin-socket setup.
    if let Commands::Config { action } = &cli.command {
        match action {
            shared_cli::ConfigAction::Defaults => {
                shared_cli::emit_defaults(commands::generate_config::reference_content());
            }
            shared_cli::ConfigAction::SystemdUnit => {
                shared_cli::emit_systemd_unit(commands::generate_config::systemd_unit_content());
            }
            shared_cli::ConfigAction::Completion { shell } => {
                use clap::CommandFactory;
                let mut cmd = Cli::command();
                shared_cli::emit_completion(&mut cmd, *shell)?;
            }
        }
        return Ok(());
    }

    // Build a current-thread tokio runtime for the admin-socket
    // calls. Multi-threaded would be overkill for a one-shot CLI;
    // current-thread keeps thread spawn cost off the cold path.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let config_path = cli.get_config_path();
    let target_user = cli.target_user().to_string();
    // Strict split: daemon-down commands (`volume key`, `system
    // regenerate-cert`) read the yaml — they need `data_dir` to
    // write under and run with `sudo` on a packaged install so the
    // 0640 conffile is readable. Every other command is daemon-routed
    // and only talks to the admin Unix socket; it must not touch the
    // yaml at all.
    let data_dir = if cli.is_daemon_down() {
        PathBuf::from(MinimalConfig::load(&config_path)?.data_dir)
    } else {
        PathBuf::new()
    };

    // Daemon-down writes ride under sudo; drop privs to the
    // daemon's system user up front so the resulting files are
    // owned correctly. Daemon-routed commands keep root's
    // peer-cred so the daemon can authorize them.
    if cli.is_daemon_down() {
        shared_cli::privdrop::drop_to_user_if_root(&target_user)?;
    }

    match cli.command {
        Commands::Volume { action } => match action {
            VolumeAction::Create {
                name,
                size,
                backend,
                page_size,
                dedup,
                worm,
                encrypt,
                key_file,
                keystore,
                dek_source,
                sync_after,
                lun,
            } => {
                volume::cmd_create(
                    &name,
                    &size,
                    backend.as_deref(),
                    &page_size,
                    &dedup,
                    worm,
                    encrypt,
                    key_file.as_deref(),
                    keystore.as_deref(),
                    dek_source.as_deref(),
                    &sync_after,
                    lun,
                )
                .await
            }
            VolumeAction::List { json } => volume::cmd_list(json).await,
            VolumeAction::Info { name, json } => volume::cmd_info(&name, json).await,
            VolumeAction::Destroy { name, force } => volume::cmd_destroy(&name, force).await,
            VolumeAction::Modify { name, sync_after } => {
                volume::cmd_modify_sync_after(&name, &sync_after).await
            }
            VolumeAction::Resize {
                name,
                size,
                shrink_to_fit,
            } => volume::cmd_resize(&name, size.as_deref(), shrink_to_fit).await,
            VolumeAction::Key { action } => match action {
                KeyAction::Migrate {
                    name,
                    to,
                    purge_local,
                } => {
                    volume::cmd_key_migrate(
                        &data_dir,
                        std::path::Path::new(&config_path),
                        &name,
                        &to,
                        purge_local,
                    )
                    .await
                }
                KeyAction::Export { name, to, iter } => {
                    volume::cmd_key_export(
                        &data_dir,
                        std::path::Path::new(&config_path),
                        &name,
                        &to,
                        iter,
                    )
                    .await
                }
                KeyAction::Import {
                    name,
                    from,
                    keystore,
                } => {
                    volume::cmd_key_import(
                        &data_dir,
                        std::path::Path::new(&config_path),
                        &name,
                        &from,
                        keystore.as_deref(),
                    )
                    .await
                }
            },
            VolumeAction::Snapshot { action } => match action {
                SnapshotAction::Create { volume, snapshot } => {
                    volume::cmd_snapshot_create(&volume, &snapshot).await
                }
                SnapshotAction::List { volume, json } => {
                    volume::cmd_snapshot_list(&volume, json).await
                }
                SnapshotAction::Destroy {
                    volume: vol,
                    snapshot,
                    force,
                } => volume::cmd_snapshot_destroy(&vol, &snapshot, force).await,
            },
            VolumeAction::Clone {
                source,
                new_name,
                from_snapshot,
                lun,
            } => volume::cmd_clone(&source, &new_name, from_snapshot.as_deref(), lun).await,
        },
        Commands::System { action } => match action {
            SystemAction::Storage { action } => match action {
                StorageAction::Check => {
                    shared_cli_system::cmd_storage_check(&shared_naming::DISK).await
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
                    storage::cmd_benchmark(
                        std::path::Path::new(&config_path),
                        backends,
                        total_gb,
                        chunk_size_mb,
                        concurrency,
                        chunk_size_mb_sweep,
                        concurrency_sweep,
                        skip_download,
                        yes,
                    )
                    .await
                }
            },
            SystemAction::Gc { dry_run, storage } => {
                shared_cli_system::cmd_gc(&shared_naming::DISK, dry_run, storage).await
            }
            SystemAction::RegenerateCert => {
                shared_cli_system::cmd_regenerate_cert(
                    &shared_naming::DISK,
                    std::path::Path::new(&config_path),
                )
                .await
            }
            SystemAction::Alerting { action } => match action {
                AlertingAction::List { json } => {
                    shared_cli_alerting::list(&shared_naming::DISK, json).await
                }
                AlertingAction::Test { sink, severity } => {
                    shared_cli_alerting::test(&shared_naming::DISK, &sink, &severity).await
                }
            },
            SystemAction::DaemonHealth { json } => {
                shared_cli_system::cmd_daemon_health(&shared_naming::DISK, json).await
            }
            SystemAction::Monitor => {
                let code = shared_cli_system::cmd_monitor(&shared_naming::DISK).await?;
                std::process::exit(i32::from(code));
            }
            SystemAction::Audit { action } => {
                use shared_cli_system::audit;
                let p = &shared_naming::DISK;
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
                std::process::exit(i32::from(code))
            }
            SystemAction::Stats { json } => {
                let code = stats::cmd_stats(json).await?;
                std::process::exit(i32::from(code))
            }
            SystemAction::Verify {
                skip_storage,
                verbose,
                json,
                volumes,
            } => {
                let code = verify::cmd_verify(skip_storage, verbose, json, volumes).await?;
                std::process::exit(i32::from(code))
            }
        },
        Commands::Iscsi { action } => match action {
            IscsiAction::Users { action } => match action {
                IscsiUsersAction::List { json } => credentials::users_list(json).await,
                IscsiUsersAction::Add {
                    name,
                    password,
                    password_stdin,
                    mutual_chap,
                    partition,
                    volume,
                } => {
                    // clap's `required = true` + `num_args = 1..`
                    // guarantees a non-empty Vec — under VSA's
                    // mandatory-admission model. Wrap as Some(_).
                    credentials::users_add(
                        &name,
                        password.as_deref(),
                        password_stdin,
                        mutual_chap,
                        partition.as_deref(),
                        Some(volume.as_slice()),
                    )
                    .await
                }
                IscsiUsersAction::Grant { name, volume } => {
                    credentials::users_grant(&name, &volume).await
                }
                IscsiUsersAction::Revoke { name, volume } => {
                    credentials::users_revoke(&name, &volume).await
                }
                IscsiUsersAction::Remove { name } => credentials::users_remove(&name).await,
                IscsiUsersAction::Disable { name } => {
                    credentials::users_set_disabled(&name, true).await
                }
                IscsiUsersAction::Enable { name } => {
                    credentials::users_set_disabled(&name, false).await
                }
                IscsiUsersAction::Rotate {
                    name,
                    password,
                    password_stdin,
                    grace,
                    cancel,
                } => {
                    if cancel {
                        credentials::users_rotate_cancel(&name).await
                    } else {
                        credentials::users_rotate(
                            &name,
                            password.as_deref(),
                            password_stdin,
                            &grace,
                        )
                        .await
                    }
                }
            },
            IscsiAction::Target { action } => match action {
                IscsiTargetAction::Show { json } => credentials::target_show(json).await,
                IscsiTargetAction::Set {
                    username,
                    password,
                    password_stdin,
                } => credentials::target_set(&username, password.as_deref(), password_stdin).await,
                IscsiTargetAction::Clear => credentials::target_clear().await,
            },
        },
        Commands::Nvmetcp { action } => match action {
            NvmetcpAction::Psks { action } => match action {
                NvmetcpPsksAction::List { json } => credentials::psks_list(json).await,
                NvmetcpPsksAction::Add {
                    host_nqn,
                    key,
                    volume,
                } => {
                    // clap requires at least one `--volume`; pass as
                    // Some(_) to the daemon which enforces non-empty.
                    credentials::psks_add(&host_nqn, &key, Some(volume.as_slice())).await
                }
                NvmetcpPsksAction::Grant { host_nqn, volume } => {
                    credentials::psks_grant(&host_nqn, &volume).await
                }
                NvmetcpPsksAction::Revoke { host_nqn, volume } => {
                    credentials::psks_revoke(&host_nqn, &volume).await
                }
                NvmetcpPsksAction::Remove { host_nqn } => credentials::psks_remove(&host_nqn).await,
                NvmetcpPsksAction::Disable { host_nqn } => {
                    credentials::psks_set_disabled(&host_nqn, true).await
                }
                NvmetcpPsksAction::Enable { host_nqn } => {
                    credentials::psks_set_disabled(&host_nqn, false).await
                }
                NvmetcpPsksAction::Rotate {
                    host_nqn,
                    key,
                    grace,
                    cancel,
                } => {
                    if cancel {
                        credentials::psks_rotate_cancel(&host_nqn).await
                    } else {
                        let k = key.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("--key is required (use --cancel to revert a rotation)")
                        })?;
                        credentials::psks_rotate(&host_nqn, k, &grace).await
                    }
                }
            },
            NvmetcpAction::Dhchap { action } => match action {
                NvmetcpDhchapAction::List { json } => credentials::dhchap_list(json).await,
                NvmetcpDhchapAction::Add {
                    host_nqn,
                    key,
                    ctrl_key,
                    volume,
                } => {
                    credentials::dhchap_add(
                        &host_nqn,
                        &key,
                        ctrl_key.as_deref(),
                        Some(volume.as_slice()),
                    )
                    .await
                }
                NvmetcpDhchapAction::Grant { host_nqn, volume } => {
                    credentials::dhchap_grant(&host_nqn, &volume).await
                }
                NvmetcpDhchapAction::Revoke { host_nqn, volume } => {
                    credentials::dhchap_revoke(&host_nqn, &volume).await
                }
                NvmetcpDhchapAction::Remove { host_nqn } => {
                    credentials::dhchap_remove(&host_nqn).await
                }
                NvmetcpDhchapAction::Disable { host_nqn } => {
                    credentials::dhchap_set_disabled(&host_nqn, true).await
                }
                NvmetcpDhchapAction::Enable { host_nqn } => {
                    credentials::dhchap_set_disabled(&host_nqn, false).await
                }
                NvmetcpDhchapAction::SetCtrlKey { host_nqn, key } => {
                    credentials::dhchap_set_ctrl_key(&host_nqn, &key).await
                }
                NvmetcpDhchapAction::ClearCtrlKey { host_nqn } => {
                    credentials::dhchap_clear_ctrl_key(&host_nqn).await
                }
                NvmetcpDhchapAction::Rotate {
                    host_nqn,
                    key,
                    grace,
                    cancel,
                } => {
                    if cancel {
                        credentials::dhchap_rotate_cancel(&host_nqn).await
                    } else {
                        let k = key.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("--key is required (use --cancel to revert a rotation)")
                        })?;
                        credentials::dhchap_rotate(&host_nqn, k, &grace).await
                    }
                }
            },
        },
        // Config dispatched above before runtime construction.
        Commands::Config { .. } => unreachable!(),
    }
}
