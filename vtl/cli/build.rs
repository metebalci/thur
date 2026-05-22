// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Auto-regenerate `thurvtl-completion.bash` from the clap `Cli`
//! enum on every relevant build. The same `cli.rs` file is `include!`d
//! by both `main.rs` and this build script, so the binary and the
//! completion script can never disagree about what flags exist.
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
use std::path::PathBuf;
use std::process::Command;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

include!("src/cli.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=src/commands/defaults_reference.yaml");
    println!("cargo:rerun-if-changed=Cargo.toml");

    emit_version_env();

    // Layout C puts this crate at `<workspace>/vtl/cli/`, so the
    // workspace root is two parents up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest.parent().and_then(|p| p.parent()) else {
        return;
    };
    let dist_dir = workspace_root.join("dist");
    let _ = fs::create_dir_all(&dist_dir);

    // Bash + zsh cover the overwhelming majority of operators.
    // Anyone on fish / elvish / powershell / nushell can still run
    // `thurvtl config completion <shell>` on demand — the
    // checked-in pair just spares the common case from a manual step.
    for (shell, filename) in [
        (Shell::Bash, "thurvtl-completion.bash"),
        (Shell::Zsh, "thurvtl-completion.zsh"),
    ] {
        let target = dist_dir.join(filename);
        let mut new_content: Vec<u8> = Vec::new();
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "thurvtl", &mut new_content);
        write_if_changed(&target, &new_content);
    }

    // Mirror the full enumerated defaults reference to `dist/`.
    // Source of truth = `src/commands/defaults_reference.yaml` (a real
    // .yaml file so editors / yaml linters cooperate); the CLI's
    // `config generate` reads the same bytes via
    // `include_str!`. The checked-in `dist/thurvtl.defaults.yaml` is
    // therefore a build-maintained mirror — operators never have to
    // run `--defaults > thurvtl.defaults.yaml` after touching the
    // template. Same byte-compare guard so an unchanged yaml leaves
    // the on-disk mirror's mtime alone.
    let defaults_src = include_bytes!("src/commands/defaults_reference.yaml");
    write_if_changed(&dist_dir.join("thurvtl.defaults.yaml"), defaults_src);
}

/// Emit `THURVTL_VERSION=<crate-ver> (<sha>[-dirty])` for clap's
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

    // CARGO_PKG_VERSION is always populated by cargo at build time;
    // env!() resolves it at compile time of build.rs itself, so there
    // is no Result to unwrap.
    let pkg_ver = env!("CARGO_PKG_VERSION");
    let version = if dirty {
        format!("{pkg_ver} ({sha}-dirty)")
    } else {
        format!("{pkg_ver} ({sha})")
    };
    println!("cargo:rustc-env=THURVTL_VERSION={version}");

    // Trigger a rebuild when HEAD moves. We watch `.git/logs/HEAD`,
    // not `.git/HEAD`: the latter just contains `ref: refs/heads/<br>`
    // and only mutates on branch swap, so commits on the current
    // branch leave its mtime untouched. `.git/logs/HEAD` is appended
    // on every HEAD movement (commit, checkout, reset, merge) and is
    // the canonical "the active commit changed" signal.
    //
    // Watching working-tree mutation for dirty-bit freshness would
    // mean instructing cargo to rerun on every file in the repo,
    // which isn't worth the cost.
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
