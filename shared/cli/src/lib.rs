// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared CLI surface — global flags, the `config` subcommand,
//! UX helpers (completion / defaults / unit emission), byte
//! formatting, and the root-drop helper for daemon-down write paths.
//!
//! Every CLI binary in the workspace flattens [`GlobalArgs`] into
//! its top-level `Cli` and references [`ConfigAction`] as the
//! `Config { action }` subcommand, so `--config`, `--user`, and
//! every `config …` verb parse and document identically across
//! products.
//!
//! Kept separate from `shared-admin-client` so the admin client
//! doesn't pull `clap` / `clap_complete` (they're only useful when
//! a `clap::Command` is in scope, i.e. inside the CLI binaries).

#![forbid(unsafe_code)]

use anyhow::{Result, anyhow};
use clap::Command;
use clap_complete::Shell;
use std::str::FromStr;

pub mod fmt;
pub mod privdrop;

pub use fmt::{format_bytes, with_host_ratio};

/// Common global flags shared by every CLI binary in the workspace.
///
/// Embed via `#[command(flatten)]` on each binary's top-level
/// `Cli` so `--config`, `--user`, and `--copyright` parse
/// identically. The defaults referenced in the help text are
/// product-specific (`thurvtl` / `thurvsa`) and resolved at runtime
/// in `main` — the flag definitions themselves stay product-neutral
/// here.
#[derive(clap::Args, Debug, Clone)]
pub struct GlobalArgs {
    /// Path to configuration file.
    ///
    /// Defaults to `/etc/thurvtl/thurvtl.yaml` (thurvtl) or
    /// `/etc/thurvsa/thurvsa.yaml` (thurvsa).
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    /// User to drop privileges to under sudo.
    ///
    /// Only applied by daemon-down commands that write under
    /// `data_dir`. Under plain `sudo` those files would otherwise
    /// be owned by root and unreadable by the daemon. No effect
    /// when the CLI is already running as a non-root user.
    /// Defaults to `thurvtl` (thurvtl) or `thurvsa`
    /// (thurvsa).
    #[arg(long, global = true)]
    pub user: Option<String>,

    /// Print the copyright + license notice and exit.
    ///
    /// Handled by [`handle_copyright_flag`] before `Cli::parse()`,
    /// so the flag works without a subcommand. Declared here too
    /// so it shows up in `--help`.
    #[arg(long, global = true)]
    pub copyright: bool,
}

/// Pre-parse intercept for `--copyright`. If the flag appears
/// anywhere in `std::env::args()`, print
/// [`shared_naming::COPYRIGHT_NOTICE`] on stdout and `exit(0)`.
///
/// Call at the very top of `main` (before `Cli::parse()`) so the
/// flag works in `<binary> --copyright` form — clap would
/// otherwise reject that for missing a subcommand. clap's parse
/// of the same flag (via [`GlobalArgs::copyright`]) is unused;
/// the field exists purely so `--help` lists the flag.
pub fn handle_copyright_flag() {
    if std::env::args().skip(1).any(|a| a == "--copyright") {
        println!("{}", shared_naming::COPYRIGHT_NOTICE);
        std::process::exit(0);
    }
}

/// `config` subcommand. Local-only artifact emission — does not
/// read the config file or talk to the daemon. Identical across
/// every CLI binary.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Emit the default configuration yaml on stdout.
    ///
    /// Same content as the checked-in
    /// `dist/<product>.defaults.yaml` (which the build script
    /// auto-maintains). Required keys are commented out so the
    /// operator picks values; optional keys with built-in defaults
    /// are written at their default value.
    Defaults,

    /// Emit the default systemd unit file on stdout.
    ///
    /// Same content the .deb / .rpm ship — operator can
    /// redirect into `/etc/systemd/system/` and `systemctl
    /// daemon-reload` to register it.
    SystemdUnit,

    /// Emit a shell completion script on stdout.
    ///
    /// Pipe into the right system path for your shell, or `source`
    /// it from the current shell to enable Tab completion
    /// immediately. Without an argument, auto-detects from `$SHELL`
    /// (basename, e.g. `/usr/bin/zsh` → `zsh`); pass an explicit
    /// shell to override.
    Completion {
        /// Target shell. Defaults to `basename $SHELL`.
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
}

/// Emit a shell completion script to stdout.
///
/// `cmd` is the live `clap::Command` (typically `Cli::command()`).
/// `shell` selects the target; passing `None` auto-detects from
/// `$SHELL` (basename, e.g. `/usr/bin/zsh` → `zsh`). Returns
/// `Err` when `$SHELL` is unset / unrecognized.
pub fn emit_completion(cmd: &mut Command, shell: Option<Shell>) -> Result<()> {
    let resolved = match shell {
        Some(s) => s,
        None => {
            let raw = std::env::var("SHELL").map_err(|_| {
                anyhow!(
                    "$SHELL is not set; pass an explicit shell name \
                     (bash | zsh | fish | elvish | powershell)"
                )
            })?;
            let name = std::path::Path::new(&raw)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&raw);
            Shell::from_str(name).map_err(|_| {
                anyhow!(
                    "unsupported shell '{name}' detected from $SHELL; \
                     pass one of: bash | zsh | fish | elvish | powershell"
                )
            })?
        }
    };
    let bin = cmd.get_name().to_string();
    clap_complete::generate(resolved, cmd, bin, &mut std::io::stdout());
    Ok(())
}

/// Emit a defaults reference artifact (`<product>.defaults.yaml`)
/// to stdout. Trivial wrapper — callers `include_str!` their own
/// content so the path stays per-crate.
pub fn emit_defaults(content: &str) {
    print!("{}", content);
}

/// Emit a systemd unit template to stdout. Same shape as
/// [`emit_defaults`]; pinned for symmetry.
pub fn emit_systemd_unit(content: &str) {
    print!("{}", content);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// Synthetic top-level CLI: flattens [`GlobalArgs`] and carries the
    /// `config` subcommand exactly the way every product binary does.
    #[derive(clap::Parser, Debug)]
    #[command(name = "synthetic")]
    struct Cli {
        #[command(flatten)]
        global: GlobalArgs,
        #[command(subcommand)]
        action: Option<TopCmd>,
    }

    #[derive(clap::Subcommand, Debug)]
    enum TopCmd {
        Config {
            #[command(subcommand)]
            action: ConfigAction,
        },
    }

    #[test]
    fn global_args_defaults_are_none_and_false() {
        let cli = Cli::parse_from(["synthetic"]);
        assert!(cli.global.config.is_none());
        assert!(cli.global.user.is_none());
        assert!(!cli.global.copyright);
        assert!(cli.action.is_none());
    }

    #[test]
    fn global_args_parse_config_and_user() {
        let cli = Cli::parse_from(["synthetic", "--config", "/tmp/x.yaml", "--user", "svc"]);
        assert_eq!(cli.global.config.as_deref(), Some("/tmp/x.yaml"));
        assert_eq!(cli.global.user.as_deref(), Some("svc"));
    }

    #[test]
    fn global_args_short_config_flag() {
        let cli = Cli::parse_from(["synthetic", "-c", "/etc/p.yaml"]);
        assert_eq!(cli.global.config.as_deref(), Some("/etc/p.yaml"));
    }

    #[test]
    fn copyright_flag_parses() {
        let cli = Cli::parse_from(["synthetic", "--copyright"]);
        assert!(cli.global.copyright);
    }

    #[test]
    fn config_defaults_subcommand_parses() {
        let cli = Cli::parse_from(["synthetic", "config", "defaults"]);
        assert!(matches!(
            cli.action,
            Some(TopCmd::Config {
                action: ConfigAction::Defaults
            })
        ));
    }

    #[test]
    fn config_systemd_unit_subcommand_parses() {
        let cli = Cli::parse_from(["synthetic", "config", "systemd-unit"]);
        assert!(matches!(
            cli.action,
            Some(TopCmd::Config {
                action: ConfigAction::SystemdUnit
            })
        ));
    }

    #[test]
    fn config_completion_without_shell_parses_to_none() {
        let cli = Cli::parse_from(["synthetic", "config", "completion"]);
        assert!(matches!(
            cli.action,
            Some(TopCmd::Config {
                action: ConfigAction::Completion { shell: None }
            })
        ));
    }

    #[test]
    fn config_completion_with_explicit_shell_parses() {
        let cli = Cli::parse_from(["synthetic", "config", "completion", "bash"]);
        assert!(matches!(
            cli.action,
            Some(TopCmd::Config {
                action: ConfigAction::Completion {
                    shell: Some(Shell::Bash)
                }
            })
        ));
    }

    #[test]
    fn config_completion_rejects_unknown_shell() {
        let err = Cli::try_parse_from(["synthetic", "config", "completion", "tcsh"]);
        assert!(err.is_err());
    }

    #[test]
    fn emit_completion_with_explicit_shell_succeeds() {
        // An explicit shell never touches $SHELL, so this is a pure
        // generate-to-stdout call with no env dependency.
        let mut cmd = Cli::command();
        assert!(emit_completion(&mut cmd, Some(Shell::Zsh)).is_ok());
    }

    #[test]
    fn emit_completion_each_supported_shell() {
        for sh in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let mut cmd = Cli::command();
            assert!(emit_completion(&mut cmd, Some(sh)).is_ok());
        }
    }

    #[test]
    fn emit_defaults_and_unit_accept_arbitrary_content() {
        // These are thin print wrappers; exercising them confirms they
        // accept content of any shape without panicking.
        emit_defaults("data_dir: /var/lib/x\n");
        emit_defaults("");
        emit_systemd_unit("[Unit]\nDescription=x\n");
        emit_systemd_unit("");
    }
}
