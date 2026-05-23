// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Auto-regenerate `thurvsa-completion.{bash,zsh}` and `thurvsa.1`
//! from the clap `Cli` enum on every relevant build, and mirror
//! `defaults_reference.yaml` to `dist/thurvsa.defaults.yaml`. The same
//! `cli.rs` file is `include!`d by both `main.rs` and this build
//! script, so the binary, the completion scripts, and the man page
//! can never disagree about what flags exist.
//!
//! Two layers of don't-redo-work:
//!
//! 1. `cargo:rerun-if-changed=src/cli.rs` — cargo only re-runs this
//!    script when the CLI definition file changes. Most builds skip
//!    it entirely.
//! 2. Even when the script does run, we only write the file if its
//!    bytes actually differ from disk. So an unchanged Cli leaves
//!    the on-disk script's mtime alone — no spurious `git status`
//!    entry, no rebuild cascade in tools that watch mtimes.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use clap_mangen::Man;

include!("src/cli.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=src/commands/defaults_reference.yaml");
    println!("cargo:rerun-if-changed=Cargo.toml");

    emit_version_env();

    // Layout C puts this crate at `<workspace>/vsa/cli/`, so the
    // workspace root is two parents up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest.parent().and_then(|p| p.parent()) else {
        return;
    };
    let dist_dir = workspace_root.join("dist");
    let _ = fs::create_dir_all(&dist_dir);

    // Bash + zsh cover the overwhelming majority of operators.
    // Anyone on fish / elvish / powershell / nushell can still run
    // `thurvsa config completion <shell>` on demand — the
    // checked-in pair just spares the common case from a manual step.
    for (shell, filename) in [
        (Shell::Bash, "thurvsa-completion.bash"),
        (Shell::Zsh, "thurvsa-completion.zsh"),
    ] {
        let target = dist_dir.join(filename);
        let mut new_content: Vec<u8> = Vec::new();
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "thurvsa", &mut new_content);
        write_if_changed(&target, &new_content);
    }

    // Mirror the full enumerated defaults reference to `dist/`.
    // Source of truth = `src/commands/defaults_reference.yaml`; the
    // CLI's `config defaults` reads the same bytes via `include_str!`.
    // The checked-in `dist/thurvsa.defaults.yaml` is therefore a
    // build-maintained mirror — operators never have to run `--defaults
    // > thurvsa.defaults.yaml` after touching the template.
    let defaults_src = include_bytes!("src/commands/defaults_reference.yaml");
    write_if_changed(&dist_dir.join("thurvsa.defaults.yaml"), defaults_src);

    // Render a section-1 man page from the same Cli tree. clap_mangen's
    // `Man::render` covers NAME / SYNOPSIS / DESCRIPTION / OPTIONS
    // / SUBCOMMANDS / VERSION / AUTHORS; we append a hand-rolled FILES
    // + SEE ALSO trailer pointing operators at the daemon page and the
    // shipped defaults reference (the .yaml file is the YAML-format
    // documentation, no separate section-5 page).
    let cmd = Cli::command().name("thurvsa");
    let mut man_buf: Vec<u8> = Vec::new();
    if let Err(e) = Man::new(cmd).render(&mut man_buf) {
        println!("cargo:warning=could not render thurvsa.1: {e}");
    } else {
        let _ = writeln!(
            &mut man_buf,
            ".SH FILES\n\
             .TP\n\
             .I /etc/thurvsa/thurvsa.yaml\n\
             Daemon configuration. Minimal starter shipped at install; the\n\
             full annotated reference for every key is at\n\
             .IR /usr/share/doc/thurvsa/thurvsa.defaults.yaml .\n\
             .TP\n\
             .I /run/thurvsa/admin.sock\n\
             Admin Unix socket the CLI dials for daemon-routed verbs (mode\n\
             0660, peer-cred authed).\n\
             .SH SEE ALSO\n\
             .BR thurvsad (8)"
        );
        write_if_changed(&dist_dir.join("thurvsa.1"), &man_buf);
    }
}

/// Emit `THURVSA_VERSION=<crate-ver> (<sha>[-dirty])` for clap's
/// `#[command(version = ...)]` to pick up. SHA comes from `git
/// rev-parse --short=7 HEAD`; dirty flag from `git status
/// --porcelain`. Outside a git checkout (e.g. distro tarball
/// rebuild), SHA falls back to `unknown` and dirty to false.
fn emit_version_env() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let pkg_ver = env!("CARGO_PKG_VERSION");
    let version = if dirty {
        format!("{pkg_ver} ({sha}-dirty)")
    } else {
        format!("{pkg_ver} ({sha})")
    };
    println!("cargo:rustc-env=THURVSA_VERSION={version}");

    // Trigger a rebuild when HEAD moves. We watch `.git/logs/HEAD`,
    // not `.git/HEAD`: the latter just contains `ref: refs/heads/<br>`
    // and only mutates on branch swap, so commits on the current
    // branch leave its mtime untouched. `.git/logs/HEAD` is appended
    // on every HEAD movement (commit, checkout, reset, merge).
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest.parent().and_then(|p| p.parent()) else {
        return;
    };
    let head_log = workspace_root.join(".git/logs/HEAD");
    if head_log.exists() {
        println!("cargo:rerun-if-changed={}", head_log.display());
    }
}

/// Write `bytes` to `path` only when its current contents differ.
/// A read-only workspace (e.g. distro-packaged source tarball) is a
/// legitimate setting where we can't write next to the crate; the
/// operator already has a checked-in copy, so surface that as a
/// `cargo:warning=` rather than failing the build.
fn write_if_changed(path: &PathBuf, bytes: &[u8]) {
    let needs_write = match fs::read(path) {
        Ok(existing) => existing != bytes,
        Err(_) => true,
    };
    if needs_write && let Err(e) = fs::write(path, bytes) {
        println!("cargo:warning=could not refresh {}: {e}", path.display());
    }
}
