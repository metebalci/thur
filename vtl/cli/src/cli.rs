// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// `THURVTL_VERSION` is set by build.rs to "<crate-ver> (<sha>[-dirty])".
// `option_env!` is required (not `env!`) because this file is also
// `include!`d by build.rs itself — at that point the env var doesn't
// exist yet, so we fall back to the bare crate version.
const THURVTL_VERSION_STR: &str = match option_env!("THURVTL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "thurvtl")]
#[command(about = "ThurVTL Management CLI", long_about = None)]
#[command(version = THURVTL_VERSION_STR)]
struct Cli {
    /// Global flags (`--config`, `--user`) shared with thurvsa.
    #[command(flatten)]
    args: shared_cli::GlobalArgs,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Library management (init, info, modify, monitor)
    Library {
        #[command(subcommand)]
        action: LibraryAction,
    },

    /// Cartridge management operations
    Cartridge {
        #[command(subcommand)]
        action: CartridgeAction,
    },

    /// Changer / SMC operations (inventory, move, load, unload)
    Changer {
        #[command(subcommand)]
        action: ChangerAction,
    },

    /// Drive operations and status
    Drive {
        #[command(subcommand)]
        action: DriveAction,
    },

    /// System operations.
    System {
        #[command(subcommand)]
        action: SystemAction,
    },

    /// iSCSI CHAP credentials.
    ///
    /// Manages `<data_dir>/iscsi-users.json` -- the daemon reads it
    /// fresh on every login, so edits take effect on the next
    /// session without restart.
    Iscsi {
        #[command(subcommand)]
        action: IscsiAction,
    },

    /// Configuration helpers (defaults yaml, systemd unit, shell completion).
    Config {
        #[command(subcommand)]
        action: shared_cli::ConfigAction,
    },
}

#[derive(Subcommand)]
enum AlertingAction {
    /// Show configured alert sinks and dedup window.
    List {
        #[arg(long)]
        json: bool,
    },

    /// Fire a synthetic alert through one sink.
    ///
    /// Bypasses the rate limiter and event-class gate so the chosen
    /// sink fires regardless of config; useful for validating SMTP
    /// credentials or webhook URLs end-to-end without waiting for
    /// a real event.
    Test {
        /// Sink name from the YAML `alerting.sinks` list.
        sink: String,
        /// Severity tag on the synthetic alert.
        #[arg(long, value_parser = ["info", "warn", "error"], default_value = "warn")]
        severity: String,
    },
}

// ---------- iscsi noun (mirrors thurvsa) ----------

#[derive(Subcommand)]
enum IscsiAction {
    /// CHAP user lifecycle.
    Users {
        #[command(subcommand)]
        action: IscsiUsersAction,
    },

    /// Mutual-CHAP target credential (singleton).
    Target {
        #[command(subcommand)]
        action: IscsiTargetAction,
    },
}

#[derive(Subcommand)]
enum IscsiUsersAction {
    /// List every CHAP user.
    List {
        /// Emit JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Add a new CHAP user.
    Add {
        /// Username (CHAP identity the initiator presents).
        name: String,

        /// Password as a CLI arg. Mutually exclusive with `--password-stdin`.
        #[arg(long, conflicts_with = "password_stdin")]
        password: Option<String>,

        /// Read the password from stdin (single line).
        #[arg(long)]
        password_stdin: bool,

        /// Enable mutual CHAP (target authenticates back).
        #[arg(long)]
        mutual_chap: bool,

        /// Partition the user is fenced to.
        ///
        /// VTL-only. Must name a partition defined in
        /// `library.json::partitions`.
        #[arg(long)]
        partition: Option<String>,
    },

    /// Remove a CHAP user.
    Remove { name: String },

    /// Disable a user without removing the entry.
    Disable { name: String },

    /// Re-enable a previously disabled user.
    Enable { name: String },

    /// Rotate a user's password with a grace window.
    Rotate {
        /// Username to rotate.
        name: String,

        /// New password as a CLI arg. Mutually exclusive with
        /// `--password-stdin`.
        #[arg(long, conflicts_with_all = ["password_stdin", "cancel"])]
        password: Option<String>,

        /// Read the new password from stdin (single line).
        #[arg(long, conflicts_with = "cancel")]
        password_stdin: bool,

        /// Grace window (humantime: `24h`, `5m`, `1d12h`). Default `24h`.
        #[arg(long, default_value = "24h", conflicts_with = "cancel")]
        grace: String,

        /// Cancel an in-flight rotation: drop the new password,
        /// restore the previous one.
        #[arg(long)]
        cancel: bool,
    },
}

#[derive(Subcommand)]
enum IscsiTargetAction {
    /// Show the current target identity (password value hidden).
    Show {
        /// Emit JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Set both target_username and target_password.
    Set {
        /// Target username.
        #[arg(long)]
        username: String,

        /// Password as a CLI arg. Mutually exclusive with `--password-stdin`.
        #[arg(long, conflicts_with = "password_stdin")]
        password: Option<String>,

        /// Read the password from stdin (single line).
        #[arg(long)]
        password_stdin: bool,
    },

    /// Clear both target_username and target_password.
    Clear,
}

#[derive(Subcommand)]
enum ChangerAction {
    /// List all cartridges in the library
    Inventory {
        /// Filter by barcode pattern
        #[arg(long)]
        filter: Option<String>,

        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Move cartridge from one slot to another (changes home slot)
    Move {
        /// Source slot ID
        from_slot: u16,

        /// Destination slot ID
        to_slot: u16,

        /// Allow source and destination to belong to different
        /// logical partitions. Default refuses cross-partition moves
        /// when partitions are defined; this flag is the operator-
        /// console override and is recorded in the audit log.
        #[arg(long)]
        cross_partition: bool,
    },

    /// Load cartridge from slot to drive
    Load {
        /// Source slot ID (storage or mail slot)
        slot: u16,

        /// Destination drive ID (0-based)
        drive: u16,

        /// Allow loading from a slot in one partition into a drive
        /// in another. See `changer move --cross-partition` for the
        /// semantics; same audit-tag treatment.
        #[arg(long)]
        cross_partition: bool,
    },

    /// Unload cartridge from drive to slot
    Unload {
        /// Source drive ID (0-based)
        drive: u16,

        /// Destination slot ID (optional, auto-select if not specified)
        slot: Option<u16>,

        /// Bypass host-asserted PREVENT MEDIUM REMOVAL bit 1.
        ///
        /// Mirrors the operator-console "force unload" — only matters
        /// when an iSCSI session has the mechanical-eject bit set on
        /// this drive. Bit 0 (data-transport) gates SCSI UNLOAD on
        /// the host side and never blocks the admin path. Audited
        /// as `force: true`.
        #[arg(long)]
        force: bool,

        /// Allow unloading into a storage slot in a different partition
        /// than the drive's. See `changer move --cross-partition` for
        /// the semantics.
        #[arg(long)]
        cross_partition: bool,
    },
}

#[derive(Subcommand)]
enum SystemAction {
    /// Garbage-collect orphan chunks from the chunk pool.
    Gc {
        /// Show what would be deleted without actually deleting.
        #[arg(long)]
        dry_run: bool,

        /// Also delete orphan objects from the storage backend.
        ///
        /// Skipped by default — local cleanup is the common case.
        #[arg(long)]
        storage: bool,
    },

    /// Audit-chain operations.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },

    /// Storage-backend operations.
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },

    /// Dedup ratio, per-cartridge contribution, HEAD-skip rate.
    ///
    /// Walks every cartridge's chunks.idx and computes logical (host
    /// writes), unique pool, exclusive vs shared per cartridge.
    /// Helps tune chunk-size, chunking mode, and the two compression
    /// layers against real workloads.
    Stats {
        /// Emit the full report as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Probe the daemon's admin Unix socket.
    ///
    /// Connects to `<data_dir>/admin.sock`, calls `GET
    /// /api/v1/health`, and renders the daemon's identity (version,
    /// data dir, API version). Confirms the transport is reachable
    /// and that the daemon is the one this CLI's config points at.
    DaemonHealth {
        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Live activity screen — holds and redraws ~1s, Ctrl-C to exit.
    ///
    /// Daemon-routed. Streams a per-second snapshot over the admin
    /// socket: uptime, cartridges loaded, drives busy, iSCSI sessions,
    /// per-backend pool used/cap + backpressure, per-backend storage
    /// PUT/GET rate over the last 60s, audit events over the last 5m.
    Monitor,

    /// Library-wide consistency check.
    Verify {
        /// Skip the storage-backend sweep (local-only audit).
        ///
        /// The storage sweep is the default — verifying without it
        /// leaves cold-bucket DR untested.
        #[arg(long)]
        skip_storage: bool,

        /// Per-cartridge breakdown (partitions, every error/warning).
        #[arg(long)]
        verbose: bool,

        /// Emit the full report as JSON for CI / automation.
        ///
        /// Replaces the human-readable text output.
        #[arg(long)]
        json: bool,

        /// Optional barcodes to limit the cartridge sweep.
        ///
        /// Inventory cross-check is suppressed when scoped (the
        /// report doesn't have full state).
        barcodes: Vec<String>,
    },

    /// Regenerate the admin HTTP self-signed TLS cert.
    ///
    /// Daemon-down only. Overwrites the cert/key files in place,
    /// re-deriving SANs (hostname, loopback, `http.tls.extra_sans`).
    /// Refuses unless the existing cert was auto-generated by the
    /// daemon — to replace an operator-supplied cert, delete the
    /// cert/key/.autogen files by hand first. Restart the daemon to
    /// serve the new cert.
    RegenerateCert,

    /// First-party alerting (email + webhook).
    ///
    /// Configure via the `alerting:` block in thurvtl.yaml; this
    /// subcommand inspects and tests live sinks. Daemon-routed
    /// only (no daemon-down fallback — alerting state lives only
    /// in the running daemon).
    Alerting {
        #[command(subcommand)]
        action: AlertingAction,
    },
}

#[derive(Subcommand)]
enum AuditAction {
    /// Print recent audit entries (optionally follow with -f).
    ///
    /// Follow mode only watches today's file (rotated files don't
    /// grow). Works in both plain and tamper-evident modes.
    Tail {
        /// Follow new entries as they land.
        ///
        /// Implementation: poll today's file every 500 ms.
        #[arg(short, long)]
        follow: bool,

        /// Number of trailing entries before follow mode (default 20).
        #[arg(short = 'n', long, default_value = "20")]
        lines: usize,
    },

    /// Export entries in the requested date range.
    Export {
        /// Output format: `jsonl` or `csv`.
        ///
        /// `jsonl` writes one entry per line. `csv` produces flat
        /// columns; nested params are JSON-encoded into one cell.
        #[arg(long, value_parser = ["jsonl", "csv"], default_value = "jsonl")]
        format: String,

        /// Inclusive start date (YYYY-MM-DD).
        ///
        /// Default: unbounded (all available entries).
        #[arg(long)]
        from: Option<String>,

        /// Inclusive end date (YYYY-MM-DD).
        ///
        /// Default: unbounded.
        #[arg(long)]
        to: Option<String>,
    },

    /// Verify the tamper-evident chain end-to-end.
    ///
    /// Walks every entry from genesis to tail, recomputing
    /// entry_hash and verifying prev_hash linkage. Tamper-evident
    /// mode only; plain-mode files have no chain to verify. Exit
    /// codes: 0 chain valid, 1 break found, 2 file IO error,
    /// 3 plain-mode (unverifiable).
    Verify,

    /// Offline-verify a copy of an audit directory.
    ///
    /// No daemon required. Walks every entry under `--dir` and
    /// verifies the BLAKE3 chain. Use after the audit directory has
    /// been copied off-host (cold backup) and the operator wants to
    /// validate it without booting the producing daemon. Exit codes:
    /// 0 valid, 1 break detected, 2 I/O / parse error.
    VerifyOffline {
        /// Path to the audit directory to verify (typically the
        /// `audit/` subdirectory of a `data_dir` copy).
        #[arg(long)]
        dir: std::path::PathBuf,

        /// Emit the VerifyReport as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Operator-acknowledged chain reset after a verify failure.
    ///
    /// Writes an `audit.chain_reset` entry whose `prev_hash` is a
    /// sentinel `blake3:break:<old_hash>` so the break stays visible
    /// forever. The new chain anchors off the reset entry.
    Rotate {
        /// Required confirmation. Without this flag, refuses to run.
        #[arg(long)]
        accept_break: bool,
    },
}

#[derive(Subcommand)]
enum StorageAction {
    /// Check storage-backend connectivity, auth, and read/write/delete.
    ///
    /// Always allowed — does not require the daemon to be stopped.
    Check,

    /// First-party storage-backend throughput benchmark (daemon-down).
    ///
    /// Drives parallel upload / download / delete against the named
    /// backend(s) defined under `storage.backends:` in the YAML conffile
    /// and prints parseable `[BENCH]` lines. Issues real backend API
    /// calls and transfers real bytes — runs against the operator's
    /// bucket, so expect non-zero cost on metered backends.
    Benchmark {
        /// Backend name to benchmark. Repeatable.
        ///
        /// Defaults to every backend defined under `storage.backends:`
        /// in the YAML conffile (lexicographic order).
        #[arg(long = "backend")]
        backends: Vec<String>,

        /// GiB per cell. Default 32.
        #[arg(long, default_value_t = 32)]
        total_gb: usize,

        /// MiB per upload. Default 8 matches the FastCDC chunk average.
        #[arg(long, default_value_t = 8)]
        chunk_size_mb: usize,

        /// Parallel in-flight uploads per cell. Default 16.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,

        /// Sweep chunk size across this comma-separated list.
        #[arg(long, value_delimiter = ',')]
        chunk_size_mb_sweep: Vec<usize>,

        /// Sweep concurrency across this comma-separated list.
        #[arg(long, value_delimiter = ',')]
        concurrency_sweep: Vec<usize>,

        /// Skip the download phase.
        #[arg(long)]
        skip_download: bool,

        /// Bypass the sweep-preview prompt (scripted runs).
        #[arg(long)]
        yes: bool,
    },
}


#[derive(Subcommand)]
enum CartridgeAction {
    /// Create new blank cartridge (places in first available slot)
    Create {
        /// Cartridge barcode/label
        barcode: String,

        /// LTO generation (currently 8 only).
        ///
        /// The VTL ships as a clean LTO-8 drive — only `8` is
        /// accepted today. The flag is kept for forward-compat with
        /// future LTO-9 support (see docs/LTO-9.md). Falls back
        /// to the library default when omitted.
        #[arg(long, value_parser = clap::value_parser!(u8).range(8..=8))]
        lto_generation: Option<u8>,

        /// Chunk size in megabytes.
        ///
        /// Default: 128 for fixed, 8 for fastcdc, or use value from
        /// config file. For fixed chunking this is the exact chunk
        /// size; for fastcdc it's the target average. Set to 0 for
        /// unlimited (single chunk, fixed mode only).
        #[arg(long)]
        chunk_size_mb: Option<u64>,

        /// Chunking strategy: `fastcdc` (default) or `fixed`.
        ///
        /// `fastcdc` is content-defined and gives the best dedup
        /// ratio across shifted/edited backups. `fixed` uses legacy
        /// fixed-size chunks. Sticky for the cartridge's lifetime.
        #[arg(long, value_parser = ["fastcdc", "fixed"])]
        chunking: Option<String>,

        /// FastCDC minimum chunk size in kilobytes (advanced).
        ///
        /// Overrides the derived minimum (avg/8, floored at 64 KiB).
        /// KiB unit lets operators push below the 1 MiB default;
        /// e.g. `--chunking-min-kb 256`. FastCDC mode only — rejected
        /// on `fixed`. Must satisfy `min <= chunk_size <= max`.
        #[arg(long)]
        chunking_min_kb: Option<u64>,

        /// FastCDC maximum chunk size in kilobytes (advanced).
        ///
        /// Overrides the derived maximum (avg*4, floored at 32 MiB).
        /// KiB unit for symmetry with `--chunking-min-kb`; e.g.
        /// `--chunking-max-kb 65536` for 64 MiB. FastCDC mode only —
        /// rejected on `fixed`. Must satisfy
        /// `min <= chunk_size <= max`.
        #[arg(long)]
        chunking_max_kb: Option<u64>,

        /// Create N cartridges in one call (default 1).
        ///
        /// The given barcode must end in a numeric suffix; subsequent
        /// barcodes increment the suffix preserving its zero-padded
        /// width (e.g. TAPE001 -> TAPE002).
        #[arg(long, default_value = "1", value_parser = clap::value_parser!(u32).range(1..))]
        multi: u32,

        /// Cloud backend name to bind this cartridge to.
        ///
        /// Required when the config defines multiple backends in
        /// `cloud.backends`; optional (and inferred) when only one
        /// backend is configured. The chosen name is sticky: every
        /// chunk upload, manifest backup, and refetch routes through
        /// this backend for the life of the cartridge.
        #[arg(long)]
        backend: Option<String>,

        /// Make this cartridge WORM (Write Once Read Many).
        ///
        /// Writes are only allowed at end-of-data; ERASE / FORMAT
        /// MEDIUM / ALLOW OVERWRITE are refused outright. Sticky for
        /// the cartridge's lifetime. The chosen backend must have
        /// retention_mode set (governance or compliance) — the
        /// bucket-level immutability is what enforces WORM cloud-side.
        #[arg(long)]
        worm: bool,

        /// Dedup scope: `global` (default) or `local`.
        ///
        /// `global` joins the shared per-backend pool — identical
        /// bytes from any pair of `global` cartridges on the same
        /// backend collapse into one pool file / one cloud object
        /// (cross-cartridge dedup, the headline storage feature).
        /// `local` namespaces every chunk under the cartridge's
        /// barcode — chunks are isolated per-cartridge (compliance /
        /// tenant separation, per-cartridge cleanup). Both modes
        /// content-address chunks by BLAKE3, so intra-cartridge
        /// dedup fires either way; only the scope of sharing
        /// differs. Default falls back to `cli.cartridge_dedup` in
        /// the config file when omitted. Sticky for the cartridge's
        /// lifetime.
        #[arg(long, value_parser = ["local", "global"])]
        dedup: Option<String>,

        /// Enable at-rest encryption (requires --keystore).
        ///
        /// The daemon mints a per-cartridge AES-256-GCM DEK and wraps
        /// it with the --keystore backend. Sticky for the cartridge's
        /// lifetime.
        #[arg(long, requires = "keystore")]
        encrypt: bool,

        /// Keystore backend that wraps this cartridge's DEK.
        ///
        /// Picks an entry from `keystore.backends:` in the YAML
        /// conffile. Required together with `--encrypt`. Sticky for
        /// the cartridge's lifetime — move via
        /// `cartridge key migrate --to NEW`.
        #[arg(long, requires = "encrypt")]
        keystore: Option<String>,
    },

    /// Archive a cartridge to a different cloud backend.
    ///
    /// Produces a frozen, self-contained snapshot at
    /// `archives/<barcode>/<label>/` on the target backend.
    /// Source cartridge is unaffected. Multiple archives under
    /// distinct labels can coexist. Restore later via
    /// `library restore-archive`. Refuses if loaded in a drive;
    /// WORM cartridges require target retention to be governance
    /// or compliance.
    Archive {
        /// Cartridge barcode.
        barcode: String,
        /// Target backend name.
        #[arg(long)]
        target_backend: String,
        /// 1-64-char alphanumeric label (`-`/`_` allowed). Defaults
        /// to an ISO-8601 UTC timestamp.
        #[arg(long)]
        label: Option<String>,
        /// Plan only — no PUTs.
        #[arg(long)]
        dry_run: bool,
    },

    /// Move a cartridge to a different cloud backend.
    ///
    /// Same barcode, same logical identity; only the bound backend
    /// changes. Two modes: `move` copies every chunk + manifest
    /// backup from source to target (BLAKE3-verified inline), flips
    /// `manifest.backend`, then deletes source objects (best-effort;
    /// orphans fall to GC). `rebind` does a pointer rewrite only —
    /// HEAD-verifies the target has every chunk + sentinel (unless
    /// `--no-verify`), then flips the manifest. Use `rebind` when
    /// you already replicate buckets out-of-band.
    /// Refuses while the cartridge is loaded in a drive; WORM
    /// cartridges require the target to have governance or
    /// compliance retention.
    Migrate {
        /// Cartridge barcode.
        barcode: String,
        /// Target backend name (must exist under `cloud.backends:`).
        #[arg(long)]
        target_backend: String,
        /// Migration mode.
        #[arg(long, default_value = "move", value_parser = ["move", "rebind"])]
        mode: String,
        /// Skip the per-chunk HEAD verify pass (rebind mode only).
        #[arg(long)]
        no_verify: bool,
        /// Plan only — no mutation on source, target, or local pool.
        #[arg(long)]
        dry_run: bool,
    },

    /// Import existing cartridge from filesystem
    Import {
        /// Path to cartridge directory
        path: String,

        /// Slot ID to place cartridge
        slot: u16,
    },

    /// Export cartridge to filesystem
    Export {
        /// Slot ID of cartridge to export
        slot: u16,

        /// Destination directory path
        path: String,
    },

    /// List all cartridges with metadata
    List {
        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Show detailed cartridge information
    Info {
        /// Cartridge barcode or slot ID
        identifier: String,

        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Per-cartridge legal hold (cloud-native).
    ///
    /// Provider primitives are the only source of truth — no local
    /// "is held" flag is kept. Refuses against the local backend.
    /// Applies the hold to every chunk + manifest backup the
    /// cartridge references on its bound cloud backend.
    LegalHold {
        #[command(subcommand)]
        action: LegalHoldAction,
    },

    /// At-rest encryption DEK management.
    ///
    /// Manages per-cartridge appliance-side DEKs wrapped by an
    /// entry under `keystore.backends:` in the YAML conffile.
    /// Independent of host-driven AME (SSC-4 SECURITY PROTOCOL).
    /// Daemon-down: reads/writes `manifest.json` directly +
    /// (un)wraps via the keystore — stop `thurvtld` before
    /// running.
    Key {
        #[command(subcommand)]
        action: CartridgeKeyAction,
    },
}

#[derive(Subcommand)]
enum CartridgeKeyAction {
    /// Move a cartridge's DEK wrap-target to a different keystore.
    ///
    /// Cartridge data is NOT re-encrypted — the plaintext DEK is
    /// unwrapped from the current backend, re-wrapped by the new
    /// one, and `manifest.encryption.{keystore_backend,
    /// wrapped_dek}` are updated atomically. Restart the daemon
    /// after so it picks up the new keystore binding.
    Migrate {
        /// Cartridge barcode.
        barcode: String,
        /// New keystore-backend name (must exist under `keystore.backends:`).
        #[arg(long)]
        to: String,
        /// Delete the `local` sidecar after a successful migrate
        /// off `local`. Default off so a crash mid-migrate leaves
        /// the sidecar present (recoverable rollback).
        #[arg(long)]
        purge_local: bool,
    },

    /// Show a cartridge's at-rest encryption metadata.
    ///
    /// Read-only. Prints the algorithm, keystore-backend name,
    /// wrapped-DEK length, and (for `local`) the sidecar path.
    /// Never prints key material.
    Show {
        /// Cartridge barcode.
        barcode: String,
    },
}

#[derive(Subcommand)]
enum LegalHoldAction {
    /// Engage legal hold on the cartridge's cloud objects.
    Set {
        /// Cartridge barcode.
        barcode: String,

        /// Operator-supplied label (audit log only).
        ///
        /// E.g. "case-2026-1138". Provider primitives are single
        /// ON/OFF — the label is for audit-trail correlation.
        #[arg(long)]
        id: Option<String>,

        /// Reason for the hold; recorded in the audit log only.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Release legal hold on the cartridge's cloud objects.
    Clear {
        /// Cartridge barcode.
        barcode: String,

        /// Operator-supplied label of the hold being released.
        ///
        /// Audit log only.
        #[arg(long)]
        id: Option<String>,

        /// Reason for the release; recorded in the audit log only.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Read legal-hold state from the cloud provider.
    ///
    /// Defaults to a single sentinel read on
    /// `manifests/<barcode>/manifest-latest.json` — the apply/clear
    /// ordering guarantees this is the durable answer to "is this
    /// cartridge held?". Use --full to sweep every chunk + manifest
    /// backup and verify the body matches the sentinel.
    Status {
        /// Cartridge barcode.
        barcode: String,

        /// Sweep every chunk + manifest backup, not just the sentinel.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Subcommand)]
enum DriveAction {
    /// Show drive status and current operation
    Status {
        /// Drive ID (0-based)
        drive: u16,

        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Run the SPC-4 self-test against a drive LUN.
    ///
    /// Triggers the same diagnostic the host runs via SEND
    /// DIAGNOSTIC; the result is also stamped into the drive's
    /// ring buffer so the next host RECEIVE DIAGNOSTIC RESULTS
    /// reflects this CLI-issued probe. Exit 0 on PASS, 1 on FAIL.
    SelfTest {
        /// Drive ID (0-based)
        drive: u16,

        /// Emit the structured result as JSON for automation.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LibraryAction {
    /// Show library information
    Info {
        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,

        /// Also show summed per-cartridge byte counters.
        ///
        /// Walks every cartridge's `runtime.json` — one extra file
        /// read per cartridge. Omitted by default; the bare
        /// `library info` stays topology-only and cheap.
        #[arg(long)]
        with_cartridges: bool,
    },

    /// Show min / current / max for num_slots and num_drives.
    ///
    /// Current values reflect the persisted YAML declaration; min
    /// values are computed against live inventory so the operator
    /// can see exactly which cartridge or loaded drive would block a
    /// proposed shrink. Max values are the SMC-3 / iSCSI ceilings.
    /// Daemon-routed (live read).
    Bounds {
        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Restore cartridges from a cloud backend after disaster recovery.
    ///
    /// Walks `manifests/` under the named backend, downloads each
    /// cartridge's manifest + index pages, and populates storage
    /// slots in barcode-sort order. Requires the `library:` block in
    /// thurvtl.yaml to be set and the daemon to have started at
    /// least once (so `library.json` is materialized — chassis
    /// topology is not cloud-replicated). Chunks lazy-load on first
    /// host read.
    Restore {
        /// Cloud backend name to restore from.
        ///
        /// Required when `cloud.backends:` in the YAML conffile declares
        /// more than one backend; inferred when exactly one is configured.
        #[arg(long)]
        backend: Option<String>,

        /// Restore only these barcodes (comma-separated).
        ///
        /// Default: restore everything discovered under `manifests/`.
        #[arg(long, value_delimiter = ',')]
        barcodes: Vec<String>,

        /// List what would be restored without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Skip cartridges whose local directory already exists.
        ///
        /// Useful for resuming an interrupted restore. Without this
        /// flag, an existing local dir is a fatal per-cartridge error.
        #[arg(long)]
        allow_existing: bool,
    },

    /// Pull a frozen archive back into a live cartridge.
    ///
    /// Reads from `archives/<barcode>/<label>/` on the named backend.
    /// Restored cartridge is bound to that backend and seated into a
    /// storage slot. Chunks are downloaded eagerly into the local
    /// pool; the daemon's orphan-upload sweep mirrors them into the
    /// backend's regular pool prefix later. Pass `--as-barcode` to
    /// rename the restored cartridge (mints a fresh UUID).
    RestoreArchive {
        /// Backend the archive lives on.
        #[arg(long)]
        backend: String,
        /// Source barcode the archive was created under.
        #[arg(long)]
        barcode: String,
        /// Archive label.
        #[arg(long)]
        label: String,
        /// Local barcode for the restored cartridge. Defaults to
        /// the source barcode.
        #[arg(long)]
        as_barcode: Option<String>,
        /// Skip silently if the destination barcode already exists
        /// locally. Without this flag, an existing local dir is an error.
        #[arg(long)]
        allow_existing: bool,
        /// Plan only — no downloads, no inventory mutation.
        #[arg(long)]
        dry_run: bool,
    },

    /// Monitor library activity in real-time
    Monitor {
        /// Update interval in seconds
        #[arg(long, default_value = "2")]
        interval: u64,
    },

    /// Run the SPC-4 self-test against the changer LUN.
    ///
    /// Walks library.json + inventory.json + every cartridge's
    /// manifest.json, and probes every configured cloud backend.
    /// Result is stamped into the LU0 ring buffer so the next host
    /// RECEIVE DIAGNOSTIC RESULTS reflects this CLI-issued probe.
    /// Exit 0 on PASS, 1 on FAIL.
    SelfTest {
        /// Emit the structured result as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Manage logical partitions (chassis-assembly, daemon-down).
    ///
    /// Carve the library into N independent logical partitions so
    /// multiple media servers share the daemon without racing for
    /// the same drives, slots, or robot. Empty layout = legacy
    /// single-partition library. Per-partition CHAP credentials live
    /// alongside in `iscsi.auth.users`.
    Partition {
        #[command(subcommand)]
        action: PartitionAction,
    },
}

#[derive(Subcommand)]
enum PartitionAction {
    /// List defined partitions.
    List {
        /// Emit the response as JSON for automation.
        #[arg(long)]
        json: bool,
    },

    /// Create a new partition.
    ///
    /// Storage range and mail range are half-open `[start, end)`.
    /// Drives is a comma-separated list of drive ids. The partition
    /// layout must cover every storage slot, mail slot, and drive
    /// exactly once across all defined partitions; this command fails
    /// if the resulting layout doesn't. Most chassis carry one mail
    /// slot (the global IE element) — assign it to whichever
    /// partition owns the host that drives imports/exports.
    Create {
        /// Partition name (1-64 chars, unique).
        name: String,

        /// Storage-slot range start (inclusive).
        #[arg(long)]
        storage_start: u32,

        /// Storage-slot range end (exclusive).
        #[arg(long)]
        storage_end: u32,

        /// Mail-slot range start (inclusive). Default 0 (no mail slots).
        #[arg(long, default_value_t = 0)]
        mail_start: u32,

        /// Mail-slot range end (exclusive). Default 0 (no mail slots).
        #[arg(long, default_value_t = 0)]
        mail_end: u32,

        /// Drive ids assigned to this partition (comma-separated).
        #[arg(long, value_delimiter = ',')]
        drives: Vec<u32>,
    },

    /// Modify an existing partition.
    ///
    /// Only the fields you pass are updated; the others stay as they
    /// are. The resulting full layout is re-validated.
    Modify {
        /// Partition name to modify.
        name: String,

        #[arg(long)]
        storage_start: Option<u32>,

        #[arg(long)]
        storage_end: Option<u32>,

        #[arg(long)]
        mail_start: Option<u32>,

        #[arg(long)]
        mail_end: Option<u32>,

        /// Replace the drive set (comma-separated). Pass `--drives ""`
        /// to clear it.
        #[arg(long, value_delimiter = ',')]
        drives: Option<Vec<u32>>,
    },

    /// Delete a partition.
    ///
    /// Only allowed when the operator is also reassigning the freed
    /// slots/drives via `--merge-into <other>` (which extends the
    /// other partition to absorb them) or when this is the last
    /// partition (which reverts the library to legacy single-
    /// partition mode).
    Delete {
        /// Partition name to delete.
        name: String,

        /// Reassign the freed slots/drives to another partition.
        ///
        /// Required unless this is the last remaining partition.
        #[arg(long)]
        merge_into: Option<String>,
    },
}