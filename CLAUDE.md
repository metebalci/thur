# CLAUDE.md

Guidance for Claude Code working in this repo. Deep reference material lives
under the top-level `docs/` tree — one flat directory, product scope on the
filename; this file is the orientation pass — see § Design Docs for the full
index.

## Project Overview

Two sibling Rust products on a shared cloud chunk pool, packaged separately
and co-resident on the same host:

- **Thur VTL** (working name `thurvtl`) — Virtual Tape Library presenting
  a spec-conformant SMC-3 medium-changer + SSC-4 LTO-8 drives over
  iSCSI on port 3260 (IQN `iqn.2025-10.com.metebalci:thurvtl`). INQUIRY
  identity vendor `MB` / product `THUR VTL`; not modeled after any
  specific physical chassis. Sequential-access cartridges,
  library/changer (caps 65535 storage slots and 255 drives — 16-bit
  SMC element address for slots, iSCSI single-byte LUN encoding for
  drives, see [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md) §
  Topology bounds; one Import/Export element is reported, hardwired,
  for backup-software compat — the operator-visible
  `cartridge import` / `cartridge export` CLI works against storage
  slots directly), cloud tiering (S3 / GCS / Azure / Local).
  Single-tape-drive deployments declare
  `library: { num_slots: 1, num_drives: 1, lto_generation: 8 }` in
  the YAML conffile.
- **Thur VSA** (working name `thurvsa`) — Virtual Storage Appliance, a
  block-storage target serving SBC-3
  direct-access LUNs. Sparse per-volume page table backed by the same
  dedup-capable chunk pool, 4 KiB sectors, default 64 KiB page,
  thin-provisioned. iSCSI default port 3260 — co-resident installs override
  one in YAML so the two products don't clash. IQN `iqn.2025-10.com.metebalci:thurvsa`.

Both daemons cleanly co-exist on the same host: disjoint system users
(`thurvtl` / `thurvsa`), conffile dirs (`/etc/thurvtl/` / `/etc/thurvsa/`),
data dirs, unit names, and admin sockets. iSCSI / HTTP ports default to the
same number on both — operators override one in YAML for co-residency. The
SCSI surface we present (and where we deliberately diverge from typical
LTO hardware) is in [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md).
Wire-level contracts in [`docs/SPEC.md`](docs/SPEC.md).

## Workspace Layout

Layout C umbrella tree (shipped 2026-05-10): crates live under
`shared/`, `core/`, `thurvtl/`, and `thurvsa/`. Design docs
live in the top-level `docs/` tree. Crate names are
unchanged (`shared-pool`, `core-stream`, `thurvtld`, …) — only
on-disk paths group by purpose.

- `shared/admin-proto` (`shared-admin-proto`) — wire types crossing
  the admin Unix socket: `JobEvent` enum, `JobAccepted` struct.
  Tiny dep set (serde only); shared by `shared-admin-client` and
  `shared-admin-server` so the on-wire format can't drift.
- `shared/admin-client` (`shared-admin-client`) — CLI dialer
  (UnixStream → hyper → JSON) generic over
  `shared_naming::ProductIdentity` for socket path / Host header /
  env-var override. Includes the NDJSON job-stream consumer
  (`run_job`), `ping`, error-body parsing.
- `shared/admin-iscsi` (`shared-admin-iscsi`) — cross-product axum
  handlers for `/api/v1/iscsi/{users,target}` lifecycle verbs (add /
  remove / disable / enable / rotate / rotate-cancel / list / target
  set-clear-show). Both daemons impl the [`IscsiUsersState`] trait
  (`data_dir` + optional `AuditChannel`); everything else — wire
  shapes, `ApiError` type, audit op names, sweep-expired-previous
  logic — lives here so the two `iscsi-users.json` surfaces can't
  drift.
- `shared/admin-server` (`shared-admin-server`) — daemon listener:
  `run_admin_server(socket_path, router)` (bind + chmod 0660 +
  SO_PEERCRED accept loop), `PeerCred` axum extractor, `JobRegistry`
  / `JobEmitter` / `JobHandle`, pre-built `jobs_router` parameterized
  on a per-product state type that impls `HasJobs` plus a
  product-supplied dispatch closure.
- `shared/admin-audit` (`shared-admin-audit`) — cross-product
  `system.audit.*` job handlers (`run_tail` / `run_export` /
  `run_verify` / `run_rotate`). Both daemons route those job kinds
  here; the only per-product input is the audit-log directory,
  passed as a plain `PathBuf`. Kept out of `shared-audit` itself so
  that low-level crate stays free of the `JobEmitter` / job-protocol
  deps.
- `shared/admin-monitor` (`shared-admin-monitor`) — cross-product
  `system.monitor` job handler. Tick loop that emits one
  `MonitorSnapshot` (JSON-encoded in the `JobEvent::Log.message`)
  per second until the CLI subscriber drops the stream; CLI side
  keeps a ring buffer and computes 60 s / 5 m rate windows. Both
  daemons impl `MonitorState` for their AdminState (daemon name /
  version / started_at / `LiveStats` from `shared-telemetry` / pool
  budgets / per-product VSA-or-VTL snapshot).
- `shared/cli` (`shared-cli`) — CLI UX helpers: `emit_completion`
  ($SHELL detection + `clap_complete::generate`),
  `emit_defaults` / `emit_systemd_unit` print wrappers. Separate
  crate from `shared-admin-client` so the latter doesn't pull
  `clap` / `clap_complete`.
- `shared/cli-iscsi` (`shared-cli-iscsi`) — cross-product CLI
  implementations for `iscsi users` and `iscsi target` verbs.
  Daemon-routed only: the admin socket must answer (the daemon
  serializes the edit + emits an audit row); when it's down the
  verb refuses via `require_daemon` rather than mutating the file
  directly. Parameterized on `&'static ProductIdentity` so the
  admin socket discovery + daemon name in the refusal flow from the
  per-product identity. VSA's NVMe-TCP `psks_*` verbs reuse the
  helpers (`parse_grace`, `resolve_password`, `require_daemon`) but
  keep their product-specific lifecycle in
  `vsa/cli/src/credentials.rs`.
- `shared/cloud` (`shared-cloud`) — `CloudBackend` trait + S3 / GCS /
  Azure / Local impls, retry, compression primitives.
- `shared/crypto` (`shared-crypto`) — AES-256-GCM primitives
  (`encrypt_block` / `decrypt_block`), IV derivation (`derive_iv`),
  length constants, and an `OsRng` re-export. Pure byte-slice surface
  consumed by `core-stream` (tape AME) and `core-block` (VSA at-rest);
  SCSI-flavored types stay in `core-stream::encryption`.
- `shared/dedup-stats` (`shared-dedup-stats`) — cross-product dedup
  math for `system stats`. A plain data boundary (no trait, no I/O):
  `compute_dedup(&[EntityScan])` buckets chunk hashes by
  `(backend, namespace)` and returns the per-entity exclusive/shared
  split + per-backend unique pool bytes. Entity enumeration
  (cartridge `chunks.idx` vs volume `pages.idx`) stays per-product.
- `shared/iscsi` (`shared-iscsi`) — iSCSI transport, CHAP auth,
  session management, unit-attention queue, login audit sink,
  product-agnostic `ScsiHandler` trait.
- `shared/keystore` (`shared-keystore`) — pluggable VSA volume-DEK
  keystore. `KeyStoreBackend` trait (`generate_and_wrap` / `wrap` /
  `unwrap` / `forget` / `health_check`) + six backends: `local`
  (on-disk plaintext sidecar at `<data_dir>/keys/<uuid>.key`),
  `awskms` (KMS envelope encryption with `volume_uuid` encryption
  context), `vault` (HashiCorp Vault Transit, hand-rolled `reqwest`),
  `azurekv` (Azure Key Vault RSA-OAEP-256 wrap/unwrap inside a JSON
  envelope binding `volume_uuid` — KV's RSA ops accept no
  service-side AAD), `gcpkms` (Cloud KMS symmetric encrypt/decrypt
  with native `additional_authenticated_data = hex(volume_uuid)`),
  `kmip` (KMIP 1.4+ AES-GCM Encrypt/Decrypt over hand-rolled TTLV
  + mTLS — no upstream KMIP crate; talks to Thales / Entrust /
  Fortanix / Utimaco / Vault Enterprise KMIP / IBM SKLM / PyKMIP).
  Tagged-enum config + auth resolution mirrors `shared-cloud`.
  Wrapped DEK lives in the volume manifest's `encryption.wrapped_dek`
  for non-local backends; local keeps the sidecar as the storage.
  Schema in [`docs/AUTH.md`](docs/AUTH.md) §
  VSA keystore backends. Operators move a volume's wrap-target with
  `thurvsa volume key migrate NAME --to NAME` (daemon-down).
- `shared/audit` (`shared-audit`) — append-only BLAKE3-chained log +
  cloneable `AuditChannel` producer + rate limiter.
- `shared/telemetry` (`shared-telemetry`) — OpenTelemetry SDK
  plumbing, Prometheus pull + OTLP push. Per-product instrument
  prefix (`thurvtl_*` / `thurvsa_*`) sourced from
  `shared_naming::PRODUCT.metric_prefix` at boot; `service.name`
  resource attribute carries the same distinction redundantly for
  OTLP relabeling.
- `scsi/spc` (`scsi-spc`) — SPC-4 baseline (sense, INQUIRY / VPD /
  mode / report-luns / PR primitives, canonical `ScsiRequest` /
  `ScsiResponse`).
- `shared/pool` (`shared-pool`) — content-addressed chunk pool
  (insertion APIs, namespace, cloud-key derivation, GC iter).
- `shared/upload-worker` (`shared-upload-worker`) — cloud-upload
  pipeline scaffold lifted out of VTL: `PendingUpload` /
  `UploadOutcome` payload types, the stateless
  `upload_chunk_inert(backend, &PendingUpload)` PUT-plus-HEAD-probe
  primitive, and the bounded-concurrency `run_upload_pipeline` that
  runs N PUTs through `buffer_unordered` with a caller-supplied
  per-completion hook (auto-hold reapply, eviction-Notify,
  per-product "uploaded" flag flip). Tape-side glue
  (MemoryBufferManager, crash-recovery scan, cartridge open) stays
  in `vtl/daemon`; the block side will plug in a parallel set of
  glue when VSA's async-upload path lands. `core_stream::cartridge`
  re-exports the payload types under their legacy names
  (`PendingUploadPayload`, `ChunkUploadOutcome`) for call-site
  continuity.
- `shared/verify-core` (`shared-verify-core`) — cross-product
  chunk-pool + cloud verification sweeps for `system verify`. A
  product implements the `VerifyTarget` trait (its live chunk set +
  per-entity cloud expectations); `sweep_local_pool` runs the local
  orphan scan and `sweep_cloud` the bounded cloud HEAD storm. The
  tape library/partition checks and the block page-table integrity
  check stay per-product, as does each product's `VerifyReport`
  shape; only the two pool sweeps are shared.
- `shared/naming` (`shared-naming`) — per-product identity strings
  (consumers still hardcode paths today; this is the migration
  target).
- `scsi/ssc` (`scsi-ssc`) — drive-LUN SCSI dispatch + drive-manager
  primitives + tape scsi helpers (sense, log pages, MAM attributes,
  encryption pages). Consumed by `thurvtld`.
- `scsi/smc` (`scsi-smc`) — changer-LUN SCSI dispatch (the six SMC
  opcodes: INITIALIZE / READ ELEMENT STATUS, MOVE / EXCHANGE MEDIUM,
  SEND VOLUME TAG, INITIALIZE WITH RANGE) plus element-address
  topology helpers (`ElementType`, `ElementAddressConfig`).
  `ElementAddressConfig` is constructed from the four element bases
  persisted in `library.json`'s `minted` stanza (minted once at first
  daemon start, immutable thereafter) plus the chassis counts. The
  per-command context
  `SmcScsiCtx` wraps `scsi-ssc`'s `ScsiCtx` and adds the `Library`
  lock + `ElementAddressConfig` borrows. Consumed by `thurvtld`.
- `scsi/sbc` (`scsi-sbc`) — SBC-3 block-target SCSI dispatch (every
  data-path opcode: READ / WRITE 10/16, COMPARE AND WRITE, UNMAP,
  WRITE SAME, SYNCHRONIZE CACHE; plus INQUIRY + VPD, READ CAPACITY,
  REPORT LUNS, MODE SENSE/SELECT, PERSISTENT RESERVE IN/OUT,
  MAINTENANCE IN, probes). `SbcScsiDispatcher` implements
  `shared_iscsi::ScsiHandler`; the daemon plugs its
  `VolumeRegistry` in via the `VolumeLookup` trait (same shape as
  `scsi-ssc::TapeDeviceFacade`). Consumed by `thurvsad`.
- `nvme/base` (`nvme-base`) — NVMe Base Spec primitives: 64-byte
  SQE, 16-byte CQE with status field (SCT / SC / DNR), Admin opcode
  enum, FUSE / PSDT sub-fields, Identify Controller / Identify
  Namespace / Active NS list builders, Fabrics command shapes
  (`ConnectData`, `FabricsType`, `extract_fctype`), controller
  register state (`ControllerRegs`: CC / CSTS / VS / CAP), log-page
  builders (SMART, Error Info, FW Slot). Wire-format ground floor
  every NVMe command set + NVMe-oF transport consumes.
- `nvme/nvm` (`nvme-nvm`) — NVM Command Set dispatch
  (Read / Write / Flush / Compare / Write Zeroes / DSM Deallocate /
  Verify; fused Compare+Write via `handle_fused_compare_write`).
  Admin command coverage: Identify, Keep Alive, Get/Set Features
  (Number of Queues), Get Log Page (Error / SMART / FW Slot),
  Abort. `NvmeNvmDispatcher` impls `NvmeCommandHandler`; the
  daemon plugs `VolumeRegistry` in via the `NamespaceLookup` trait
  (mirror of `scsi-sbc::VolumeLookup`). Reaches into
  `core-block::PageCache` directly — same boundary as the SBC
  dispatcher. NSID maps to LUN as `nsid = lun + 1`.
- `nvme/tcp` (`nvme-tcp`) — NVMe/TCP transport. Full per-connection
  state machine: ICReq/ICResp handshake (advertises MAXH2CDATA =
  128 KiB, captures host MAXR2T), Connect with SUBNQN admission,
  Property Get/Set against shared `ControllerRegs`, Disconnect,
  command loop with R2T flow (single-outstanding, partial-ICD +
  R2T tail stitching), fused Compare+Write pair tracking, C2HData
  SUCCESS-bit folding, C2HTermReq on protocol violations. Selected
  via `transport: nvmetcp` in `thurvsa.yaml` (default `iscsi`).
  Out of scope: TLS-PSK auth, CRC32C digests, multi-outstanding
  R2T, AER, discovery controller — rationale in
  [`docs/NVMETCP.md`](docs/NVMETCP.md) § *Out of scope*.
- `core/ssc` (`core-stream`) — SSC-4 / LTO tape-cartridge primitives:
  cartridge, block/chunk/lru indexes, dirty-page tracker, index-page
  backups, prefetch, FastCDC, AES-GCM encryption, disk-cache + pool
  budget, cartridge-side legal-hold sentinel, mode-page/drive-state,
  plus the `DriveTopology` trait. Consumed by
  `thurvtld` via `core-mediachanger`.
- `core/smc` (`core-mediachanger`) — SMC-3 medium-changer + library inventory +
  library-wide verify. Composes `core-stream`.
  `Library` (and the `LibraryFacade` lock-wrapper) impls
  `core_stream::DriveTopology`.
- `core/sbc` (`core-block`) — SBC-3 direct-access (block) device-type
  core.
- `thurvtl/{daemon,cli,scripts,docs}` — Thur VTL product (system user
  `thurvtl`, IQN `iqn.2025-10.com.metebalci:thurvtl`).
- `thurvsa/{daemon,cli,scripts,docs}` — Thur VSA product (system user
  `thurvsa`, IQN `iqn.2025-10.com.metebalci:thurvsa`).

Per-crate API surfaces, module breakdowns, cross-crate re-exports, and the
adapter layers between products are in
[`docs/WORKSPACE.md`](docs/WORKSPACE.md).

## Architecture (high-level)

- **Storage** — cartridges (Thur VTL) / volumes (Thur VSA) reduce to
  content-addressed chunks in a per-backend pool. Local + cloud tiers; cloud
  is source of truth, disk is warm cache. Refcount-aware eviction.
  Each backend gets its own `disk_cache.size_gb` cap (YAML default,
  optionally overridden per entry under `cloud.backends:` via
  `disk_cache_size_gb`), shared across that backend's pool +
  local-scope namespaces. Cartridge dir layout in
  [`docs/STORAGE.md`](docs/STORAGE.md); dedup details in
  [`docs/DEDUP.md`](docs/DEDUP.md); cartridge lifecycle
  (create / WORM / legal hold) in
  [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md); cross-backend
  + cross-region ops (`cartridge migrate`, `cartridge archive`,
  `library restore-archive`, `library restore`) in
  [`docs/SPEC.md`](docs/SPEC.md).
- **Pipelines** — both products park host writes against a per-backend
  `shared_pool::PoolBudget` and surface SCSI NOT READY (0x04/0x07) on
  timeout. Thur VTL runs an event-driven broadcast bus → bounded mpsc
  workers; chunk-seal is gated at the staging-rename boundary
  ([`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md)). Thur VSA
  runs a per-volume `PageCache` (write-back + RMW) backed by a
  `VolumeWriter` pool/cloud pipeline; page-seal is gated before
  `pool.insert_bytes`, eviction is per-volume `lru.idx`-driven
  ([`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md)).
  SYNCHRONIZE CACHE is a real fence on VSA.
- **Audit** — single BLAKE3-chained JSONL log per daemon, daily rotation,
  always on (no `enabled` knob). Single-writer
  task drains a cloneable `AuditChannel` mpsc; daemon-down CLI flows queue
  to `<audit_dir>/pending/` for replay on next start. Host-driven failure
  paths are rate-limited via `AuditRateLimiter` (60 s window).
  `audit.retention_days` defaults to 90. Full design in
  [`docs/AUDIT.md`](docs/AUDIT.md); schema +
  rate-limited-rollup shape in
  [`docs/SPEC.md`](docs/SPEC.md) § Audit Log.
- **Cloud retries** — S3/GCS/Azure classify errors and **fail fast on
  permanent errors** (`Auth` / `Authz` / `NotFound` / `RegionMismatch`);
  only `Network` / `Timeout` / `Other` (5xx, throttling, unclassified)
  consume the backoff budget. A revoked credential surfaces in seconds.
- **Telemetry** — one OTel `MeterProvider` with Prometheus pull (always
  wired at `GET /metrics`) + OTLP push (opt-in). Process-global handle;
  CLI / unit-test callers no-op. Per-product instrument prefix
  (`thurvtl_*` / `thurvsa_*`) sourced from
  `shared_naming::PRODUCT.metric_prefix`; `service.name` resource
  attribute (`thurvtl` / `thurvsa`) carries the distinction
  redundantly. Design in
  [`docs/TELEMETRY.md`](docs/TELEMETRY.md); full instrument
  table in [`docs/SPEC.md`](docs/SPEC.md) § Telemetry.
- **Alerting** — opt-in first-party email (SMTP via lettre) + generic
  webhook (HTTP POST with Tera-templated body — one path covers
  PagerDuty, Slack, Discord, ntfy.sh, ServiceNow) sinks. Four event
  classes: backend reachability, audit-log append failure, disk-cache
  watermark / backpressure timeout, repeated CHAP failures.
  Process-global dispatcher built from
  `alerting:` YAML at boot (mirrors `shared_telemetry::set_global`);
  producers emit via `shared_alerting::record::*`. Per-class dedup
  window (default 300 s) wraps `AuditRateLimiter`. No retries on sink
  failure — drop, log, count via
  `<product>_alerts_fired_total{outcome}`. Full design in
  [`docs/ALERTING.md`](docs/ALERTING.md).
- **Daemon lock** — `<data_dir>/.daemon.lock` PID lockfile; CLI mutating
  commands refuse if alive. Stale locks auto-clear.

## Quick Start

```bash
cargo build [--release]

# Print the full annotated config reference (every key documented)
thurvtl config defaults > thurvtl.yaml
thurvsa config defaults > thurvsa.yaml

# Declare the chassis topology in thurvtl.yaml's `library:` block:
#   library:
#     num_slots: 40       # REQUIRED
#     num_drives: 3       # REQUIRED
#     lto_generation: 8   # REQUIRED
# The daemon materializes <data_dir>/library/library.json from this
# block on first start, then diffs and reconciles on every subsequent
# start. `thurvtl library bounds` (after the daemon is up) shows the
# safe-shrink envelope.

# Run daemons (--config wins, otherwise /etc/{thurvtl,thurvsa}/...)
cargo run --bin thurvtld  [-- --config PATH]   # iSCSI :3260, HTTP :9090
cargo run --bin thurvsad  [-- --config PATH]   # iSCSI :3260, HTTP :9090
                                                     # (override one in YAML
                                                     #  for co-resident installs)

# Tests
cargo test [-p core-mediachanger]
cargo check && cargo fmt && cargo clippy
thurvtld --test    # in-process smoke (cartridge/library/S3/prefetch/…)
```

Production install paths (`.deb` / `.rpm` / `.tar.gz`) and recipes are in
[`README.md`](README.md). Cloud-credentials wiring (per-backend `auth:`
blocks, default chains, the per-product `<product>.env` daemon env
file) in [`docs/AUTH.md`](docs/AUTH.md). Release-cut flow + glibc-floor
strategy + OpenSSL vendoring in
[`docs/RELEASING.md`](docs/RELEASING.md).

### Auto-maintained artifacts

Eight checked-in artifacts under `dist/` are regenerated by the matching
`build.rs` on every `cargo build` — don't hand-edit:

- `dist/{thurvtl,thurvsa}-completion.{bash,zsh}` — derived from each
  CLI crate's `src/cli.rs` via `clap_complete`. The same `cli.rs` the binary
  uses is `include!`d, so the scripts can never drift. Bash doesn't embed
  help text (so `///` doc edits won't dirty the bash file); zsh does.
  Operators on shells we don't ship (fish, elvish, powershell, nushell) get
  scripts on demand via `<binary> config completion <shell>`.
- `dist/{thurvtl,thurvsa}.defaults.yaml` — mirrored byte-for-byte from each
  CLI crate's `src/commands/defaults_reference.yaml`. Edit the in-crate
  `.yaml`; the `dist/` copy follows on the next build.
- `dist/{thurvtl,thurvsa}.1` — section-1 man pages, generated from the
  same `cli.rs` via `clap_mangen`. The build script appends a FILES +
  SEE ALSO trailer pointing at the daemon page and the shipped
  defaults.yaml. The daemon pages themselves (`release/thurv{tl,sa}d.8`)
  are hand-written, since the daemon's flag surface is too small to
  bother generating; both `.1` and `.8` are shipped under
  `/usr/share/man/man{1,8}/` by the .deb / .rpm and tarball installs.

`build.rs` only rewrites if bytes differ. If the `dist/` bytes change after
a CLI edit, commit them in the same commit that touched `cli.rs`.

The build also emits `THURVTL_VERSION` / `THURVSA_VERSION` env vars by
shelling out to `git rev-parse --short=7 HEAD` and `git status --porcelain`
(format: `<crate-ver> (<sha>[-dirty])`). `cli.rs` references the env var via
`option_env!` with a fallback to bare `CARGO_PKG_VERSION` — required because
`cli.rs` is `include!`d by `build.rs` itself.
`rerun-if-changed=.git/logs/HEAD` retriggers the build when HEAD moves
(we watch the log, not `.git/HEAD`, because the latter only mutates on
branch swap). Outside a git checkout (distro tarball rebuild) SHA falls
back to `unknown`. Rust toolchain is pinned via `rust-toolchain.toml` and
`release/Containerfile.builder`'s `RUST_VERSION` ARG — bump in lockstep.

## Configuration

Conffiles: `/etc/thurvtl/thurvtl.yaml` + `/etc/thurvsa/thurvsa.yaml` (minimal
starters from `release/`; operator uncomments and edits). Full annotated
reference at `dist/{thurvtl,thurvsa}.defaults.yaml` — also what `config
defaults` prints, also installed at `/usr/share/doc/{thurvtl,thurvsa}/`.
Required key: `data_dir` (both). YAML carries install-time + tuning
knobs only. Every config file (YAML conffiles, daemon-managed JSON,
the `<product>.env` file) plus a key-by-key YAML reference is
catalogued in
[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).

`<data_dir>` is the **daemon's data dir**, not the library's — the library
is one component (`<data_dir>/library/`) alongside other daemon-managed
state (`<data_dir>/tapes/`, `<data_dir>/chunks/`, `<data_dir>/audit/`,
plus the JSON files below). Blowing away `<data_dir>/library/` and
restarting the daemon leaves everything else intact — the daemon
re-materializes `library.json` from the YAML `library:` block.

**Daemon-managed JSON files under `<data_dir>/`** (operationally mutated
+ credential-bearing state):

- `library.json` + `inventory.json` (under `library/`) — chassis topology
  and inventory. The chassis is *declared* in `thurvtl.yaml`'s
  `library:` block; the daemon materializes `library.json` on first
  start (minting `chassis_serial` and four SMC element bases) and
  reconciles YAML against persisted state on every subsequent start.
  Partition layout managed via `thurvtl library partition
  {create,modify,…}` (still imperative — partitions are deliberate,
  not declarative). Logical-partition design in
  [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md) § Multi-partition
  libraries; schema in
  [`docs/SPEC.md`](docs/SPEC.md) § Library Topology.
- `iscsi-users.json` — CHAP user list + mutual-CHAP target credentials.
  The YAML carries only `iscsi.auth.method` (`None | CHAP`) and
  `iscsi.auth.allowed_algorithms`. VTL users may carry a `partition:`
  binding for partition-fenced sessions; VSA ignores that field.
  Managed via `thur{vtl,vsa} iscsi users
  {add,remove,disable,enable,rotate,list}` and
  `iscsi target {set,clear,show}` for the singleton mutual-CHAP
  credential. Daemon re-reads on every login — no restart needed.
- `nvmetcp-psks.json` (VSA only) — TLS-PSK host identity list. Managed
  via `thurvsa nvmetcp psks {add,remove,disable,enable,rotate,list}`.
  Daemon re-reads on every TLS handshake — no restart needed.
S3 / GCS / Azure backends carry an optional `retention_mode` field
(`none` / `governance` / `compliance`). Required for WORM cartridges /
volumes. The daemon queries each backend's actual lock state at startup
and refuses to start if it doesn't match the declared mode in either
direction.

## CLI Surface

```
thurvtl      library / cartridge / changer / drive / system / config
thurvsa      volume / config
```

`<binary> <subcommand> --help` is the source of truth for flags (clap
drives both `--help` and the shipped completion scripts).

Most CLI commands are **daemon-routed** (live; talk to the admin socket at
`/run/{thurvtl,thurvsa}/admin.sock`, mode 0660, peer-cred-authed). Only
partition-layout ops (`library partition *`), DR-restore (`library
restore` / `library restore-archive`), and pure-local config commands
are **daemon-down**. Chassis topology (`num_slots`, `num_drives`,
`lto_generation`) is YAML-declared and reconciled by the daemon at
start; there's no imperative chassis-mutation verb. Long-running
ops (`gc` / `verify` / `stats` / `cloud check` / self-tests) ride a two-step
job protocol on the same socket. Full split, admin socket discovery, sudo
/ privdrop behavior, and the job protocol in
[`docs/CLI.md`](docs/CLI.md).

## Integration Tests

Two product-prefixed sets, in increasing order of prereqs / coverage:

- `vtl/scripts/test-{smoke,iscsi-conformance,scsi-conformance,backup-workflow,backup-cloud}.sh`
- `vsa/scripts/test-{smoke,iscsi-conformance,scsi-conformance,iscsi-fs-workflow,iscsi-fs-cloud,fs-cloud-failures,keystore,nvmetcp-conformance,nvme-fs-workflow,nvme-fs-cloud}.sh`

Run from the repo root; flags `--release`, `--keep-data`. Cloud variants
require `THURVTL_TEST_BACKEND` / `THURVSA_TEST_BACKEND` matching a non-`local`
entry in the conffile; refuses `retention_mode != none`.
`test-keystore.sh` is the keystore-backend counterpart of `test-iscsi-fs-cloud.sh`
— `THURVSA_TEST_KEYSTORE=<name>` picks an entry from
`private/keystore-backends.yaml` (override via `THURVSA_SOURCE_KEYSTORES`)
and exercises wrap / unwrap / migrate against any backend type
(`local` / `awskms` / `vault` / `azurekv` / `gcpkms`). sudo-required
scripts self-elevate via `exec sudo "$0" "$@"` (NOPASSWD sudoers entry to
be non-interactive). Exit 0 / 1, auto-cleanup unless `--keep-data`. What
each script covers + sudo / prereq specifics live in headers at the top of
each `.sh` file. Workspace dev utilities (`scripts/setup-system.sh`,
`scripts/docker-compose.yml`, `scripts/lib/test-helpers.sh`) stay
product-agnostic in the unprefixed top-level `scripts/` dir.

## Design Docs

All under the top-level `docs/` tree — one flat directory, product scope
carried on the filename. `README.md` and `CLAUDE.md` stay at the repo
root (CLAUDE.md must, for auto-loading).

- Architecture deep-dives:
  [`docs/STORAGE.md`](docs/STORAGE.md),
  [`docs/CARTRIDGE.md`](docs/CARTRIDGE.md),
  [`docs/DEDUP.md`](docs/DEDUP.md),
  [`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md),
  [`docs/AUDIT.md`](docs/AUDIT.md),
  [`docs/AUTH.md`](docs/AUTH.md),
  [`docs/TELEMETRY.md`](docs/TELEMETRY.md).
- Conformance:
  [`docs/CONFORMANCE_SCSI.md`](docs/CONFORMANCE_SCSI.md) — the whole
  SCSI surface in three parts: SPC-4 / SAM-5 / iSCSI / CHAP (shared
  baseline), SSC-4 / SMC-3 + tape VPD / SECURITY PROTOCOL Tape Data
  Encryption + the behavioral model and deliberate divergences from
  typical LTO hardware (VTL), and SBC-3 (VSA).
  [`docs/CONFORMANCE_NVME.md`](docs/CONFORMANCE_NVME.md) (NVMe
  Base / NVM Command Set / NVMe-oF / NVMe-TCP, incl. TLS-PSK),
  [`docs/NVMETCP.md`](docs/NVMETCP.md) (NVMe/TCP transport
  design walkthrough for VSA: crate split, opcode → PageCache
  mapping, NQN, auth roadmap).
- Wire-level reference:
  [`docs/SPEC.md`](docs/SPEC.md) — SCSI opcodes, VPD / mode /
  log pages, manifest schema, library / inventory schema, chunk-pool
  layout, cloud object-key shape, iSCSI / LTO emulation IDs, telemetry
  inventory. Update in lockstep with the code.
- Workspace + CLI references:
  [`docs/WORKSPACE.md`](docs/WORKSPACE.md),
  [`docs/CLI.md`](docs/CLI.md).
- Release:
  [`docs/RELEASING.md`](docs/RELEASING.md).
- Roadmap:
  [`ROADMAP.md`](ROADMAP.md) — single roadmap file, split by a
  `# Not committed` divider. Above it: committed work as **FIXES /
  FEATURES / BLOCKERS** (in that order; `/next` surfaces them in the
  same order, and stops at the divider). FIXES = bugs / cleanups,
  FEATURES = on-deck new work, BLOCKERS = must-land-before-tagging-
  beta/rc/ga (each tagged with the milestone it blocks); delete
  entries on ship — don't move to a "Shipped" section. Below the
  divider: a flat **Ideas** list plus **Parked — awaiting concrete
  request** and **Considered, declined** sections. Ideas graduate by
  moving up across the divider into FEATURES; a committed entry that
  loses its case drops back down to Parked; explicit rejections go to
  Declined with the assumption that would re-open them.
  [`docs/LTO-9.md`](docs/LTO-9.md) — what LTO-9 support would
  need, why a VTL cares less about LTO-9 than physical hardware
  would, and the reasoning for deferring past 1.0.0 GA.

## Tech Stack

Rust 2024, Tokio async, `tracing`, serde / serde_json / serde_yaml, BLAKE3,
fs2 (file locking), iSCSI (BHS 48 B + 4 B
padding, CmdSN / StatSN, 128 KiB segments).

---

## House Rules

- When committing changes, stage all relevant changes (incl. new files) and
  commit with a descriptive message.
- When you make a change, update the documentation immediately. If you
  change configuration, also change `config defaults` output (the
  in-crate `defaults_reference.yaml`) and the key-by-key reference in
  [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).
  Wire-level
  surface changes (SCSI opcodes / VPD / mode / log pages, manifest schema,
  library / inventory schema, on-disk chunk-pool layout, cloud object-key
  shape, iSCSI / LTO emulation IDs) MUST also update
  [`docs/SPEC.md`](docs/SPEC.md) — it is the external
  technical reference and must stay in sync with the code.
- The `dist/` artifacts are regenerated by `build.rs` on `cargo build`.
  Don't hand-edit; if bytes change after a CLI edit, commit them as part of
  the same commit that touched `cli.rs`.
- New code needs tests. Every non-trivial module carries a `#[cfg(test)]`
  block; per-crate coverage floors (80% critical / 50% shared / 30% daemons)
  and the `scripts/coverage.sh` workflow are in
  [`docs/TESTCOVERAGE.md`](docs/TESTCOVERAGE.md).
- Do not use emojis in print statements.
- When you create temporary files or folders, create them under `/tmp`.
- Do not consider backward compatibility unless specifically instructed to
  do so.
