# Workspace Layout

This document covers per-crate API surfaces, module breakdowns, and the
adapter layers that connect crates to each other. Top-level orientation —
which crates exist and why, how the two products share infrastructure — is
in [`../CLAUDE.md`](../CLAUDE.md) § Workspace Layout. This document is the
deeper reference you reach for when you need to know exactly what a crate
exports and how the pieces fit together.

Crates live under `shared/`, `core/`, `scsi/`, `nvme/`, `vtl/`, and `vsa/`.
The two product crates are `vtl-{daemon,cli}` and `vsa-{daemon,cli}` on
disk; the binaries they produce are `thurvtl-{daemon,cli}` and
`thurvsa-{daemon,cli}`.

## shared crates

- **shared-admin-proto** — wire types crossing the admin Unix socket.
  One `lib.rs` carrying `JobEvent` (tagged-union NDJSON event — Log /
  Progress / Result / Done, `#[serde(tag = "type", rename_all =
  "snake_case")]`), its constructors (`info` / `warn` / `error` /
  `progress` / `result` / `done` / `done_with_error` / `is_terminal`),
  and `JobAccepted` (`{ job_id, kind, started_at: String }`). The dep set
  is serde-only so both daemons and CLIs can depend on it without pulling
  in axum or hyper. `PeerCred` is server-only due to the orphan rule and
  lives in `shared-admin-server` instead.
- **shared-admin-client** — CLI dialer over the admin Unix socket
  (UnixStream → hyper → JSON). `AdminClient::new(socket_path,
  host_header)` / `AdminClient::auto_discover(&ProductIdentity)`
  (derives the `{NAME}_ADMIN_SOCKET` env override from
  `ProductIdentity.name`). Surface: `get_json` / `post_json` /
  `put_json` / `delete_json(path, Option<&B>)`, `ping`, `run_job<B, F>`
  NDJSON streamer (POST `/api/v1/jobs/:kind` then GET `/.../events`,
  consuming JSON-per-line until terminal `Done`), `urlencode`. Error
  path tries `{"error":"..."}` body parse, falls back to
  `String::from_utf8_lossy`. Consumed by both CLIs.
- **shared-admin-server** — daemon-side admin transport.
  - `run_admin_server(socket_path, router)` (`server.rs`): bind + chmod
    0660 + `SO_PEERCRED` capture + per-request
    `extensions.insert(PeerCred)` + hyper auto-builder serve. The caller
    hands in a pre-built `axum::Router`; the transport layer is stateless.
  - `PeerCred` (`peer.rs`): `{ uid, gid, pid }` plus
    `audit_descriptor()` (`unix:<uid>:<pid>`). Implements
    `axum::FromRequestParts`; both the type and its impl live here due to
    the orphan rule.
  - `JobRegistry` / `JobEmitter` / `JobHandle` (`jobs.rs`): NDJSON job
    lifecycle — `Arc<JobInner>` per job, `Mutex<Vec<JobEvent>>` log +
    `Notify` wake-ups, cancel-safe `JobHandle::next_events(&mut
    cursor)`. 300 s retention TTL, opportunistic `reap()` on POST.
  - `jobs_router<S: HasJobs, F: dispatch fn>(state, dispatch)`
    (`server.rs`): pre-built sub-router — `POST /api/v1/jobs/:kind`
    (calls the product dispatch closure; 400 + synthetic `Done` on
    unknown kind) and `GET /api/v1/jobs/:id/events` (NDJSON streaming).
    The `HasJobs` trait wires the registry out of product state.
  Each daemon's `admin/mod.rs` builds a product `Router`, merges with
  `jobs_router(...)`, and hands it to `run_admin_server(...)`.
- **shared-admin-http** — shared admin HTTP listener. `run_http_server`
  owns the bind / serve / TLS plumbing; each daemon builds its own
  `axum::Router` and hands it in. TLS is opt-in via `TlsConfig` —
  a missing cert+key pair is auto-generated self-signed on first boot.
  Exposes `HttpListenerConfig`, `TlsConfig`, `regenerate_cert`,
  `CertGenerationOutcome`, `RegenerateOutcome`.
- **shared-admin-iscsi** — cross-product axum handlers for iSCSI CHAP
  user lifecycle (`add` / `remove` / `disable` / `enable` / `rotate` /
  `rotate cancel` / `list`) and the mutual-CHAP target credential
  (`target {set, clear, show}`). Both daemons mount the same routes;
  each daemon's `AdminState` implements `IscsiUsersState` to plumb
  `data_dir` plus an optional `AuditChannel`. Audit op names are pinned
  (`iscsi.users.add`, `iscsi.users.rotate.start`, `iscsi.target.set`, …)
  so a multi-product audit chain reads uniformly across both daemons.
- **shared-admin-audit** — cross-product `system.audit.*` job handlers:
  `run_tail` / `run_export` / `run_verify` / `run_rotate`, each
  `(JobEmitter, serde_json::Value, PathBuf)`. Both daemons route those
  job kinds here from their `job_dispatch::dispatch`; the only
  per-product input is the audit-log directory, passed as a plain
  `PathBuf` (no trait — the handlers need nothing else). Deliberately
  kept out of `shared-audit` so that lower-level crate stays free of
  the `JobEmitter` / job-protocol deps.
- **shared-admin-cloud-check** — cross-product cloud-backend
  reachability. `run_cloud_check(JobEmitter, Arc<ObjectStoreConfig>)`
  is the `system.cloud_check` job handler both daemons mount (CLI verb
  `system storage check`); `run_reachability_ticker(Arc<ObjectStoreConfig>, u64)`
  is the opt-in periodic ticker each daemon spawns when
  `storage.check_interval_seconds` is non-zero, and `probe_backends_once`
  is the shared per-tick probe. All three reuse
  `shared_object_store::validate_object_store_backend` and fire
  `shared_alerting::record::backend_reachability`. Kept out of
  `shared-object-store` (which owns the probe) so that lower-level crate
  stays free of the `JobEmitter` + `shared-alerting` deps — same split
  as `shared-admin-audit` / `shared-admin-monitor`.
- **shared-health** — axum handler for `GET /health`, an
  unauthenticated liveness probe. Body is `{ status, daemon, version }`;
  per-product topology stays on `/info`. Daemons supply `HealthMeta
  { product: &ProductIdentity, version: &'static str }` via
  `axum::extract::FromRef`.
- **shared-alerting** — opt-in first-party alerting. Two sinks: `email`
  (SMTP via `lettre`, STARTTLS default, `${ENV_VAR}` password
  interpolation) and `webhook` (HTTP POST via `reqwest`, per-sink Tera
  body template covering PagerDuty / Slack / Discord / ntfy /
  ServiceNow). A process-global `AlertingDispatcher` is installed by
  each daemon's main via `set_global`, mirroring how
  `shared_telemetry::set_global` works; producers emit through
  `shared_alerting::record::*`. Per-class dedup wraps
  `shared_audit::audit_ratelimit::AuditRateLimiter`. Five event
  classes: backend reachability, audit-log append failure, disk-cache
  watermark + backpressure timeout (incl. VSA `lru.idx` degradation),
  repeated CHAP failures, and orphaned objects (failed best-effort
  storage delete, e.g. `cartridge migrate` source-delete). Hosts the
  cross-product `system.alerting.test` admin job and the
  `/api/v1/system/alerting` handler (behind the `http` feature).
  Audit-append failures bridge in via an `AppendFailureHook` function
  pointer installed at boot — this breaks what would otherwise be a
  circular dependency between `shared-audit` and `shared-alerting`.
  Design in [`ALERTING.md`](ALERTING.md).
- **shared-cli-alerting** — cross-product CLI for `alerting list` (GET
  `/api/v1/system/alerting`) and `alerting test <SINK> [--severity …]`
  (drives the `system.alerting.test` job). Daemon-routed only.
  Parameterized on `&'static ProductIdentity`.
- **shared-cli-iscsi** — cross-product CLI implementations for `iscsi
  users` and `iscsi target` verbs. Daemon-routed only: the admin socket
  must answer so the daemon serializes the edit and emits an audit row;
  if the socket is down the command refuses with a "start the daemon"
  message rather than mutating the JSON file directly. Parameterized on
  `&'static ProductIdentity`.
- **shared-cli-system** — cross-product `system <verb>` CLI commands,
  each generic over `ProductIdentity`. Modules: `regenerate_cert`
  (`cmd_regenerate_cert`); `daemon_health` (`cmd_daemon_health` — the
  `GET /api/v1/health` probe); `audit` (`cmd_tail` / `cmd_export` /
  `cmd_verify` / `cmd_verify_offline` / `cmd_rotate`, driving the
  `system.audit.*` jobs, with `cmd_verify_offline` the lone
  daemon-free verb); and `secrets_io` (passphrase-prompt + mode-0600
  write helpers shared with `volume key export/import`).
- **shared-cli** — CLI UX helpers (not the admin client — that's
  `shared-admin-client`). `emit_completion(&mut Cli::command(), shell)`
  carries the `$SHELL`-detection + `clap_complete::generate` block.
  `emit_defaults` / `emit_systemd_unit` are one-line `print!` wrappers;
  callers `include_str!` their per-crate content. There is no
  `shared-naming` dep here because completion emission is
  identity-agnostic.
- **shared-crypto** — AES-256-GCM primitives + IV derivation. Three
  public functions (`encrypt_block`, `decrypt_block`, `derive_iv`),
  three length constants (`KEY_LEN`, `IV_LEN`, `TAG_LEN`), one error
  enum (`CryptoError`), and an `OsRng` / `RngCore` re-export from
  `aes_gcm::aead`. The interface is pure byte-slice in / out with no
  SCSI coupling. The SCSI-flavored types (`EncryptionMode` with the
  SSC-4 `EXTERNAL` value, `DecryptionMode`, `KeyScope`,
  `DriveEncryptionState`, the `ALGORITHM_INDEX_AES_256_GCM` /
  `ALGORITHM_CODE_AES_256_GCM` registry constants) stay in
  `core-stream::encryption`. `core-stream::encryption::{encrypt_block,
  decrypt_block, KEY_LEN, IV_LEN, TAG_LEN}` are thin re-export /
  error-mapping wrappers; `core-stream::block_index::derive_iv`
  forwards to `shared_crypto::derive_iv` and retains the tape-flavored
  `(uuid, chunk_id, offset)` signature.
- **shared-dedup-stats** — cross-product dedup math for `system stats`.
  A plain data boundary: the caller reduces each scanned entity to an
  `EntityScan { label, backend, namespace, chunks: HashMap<hash,
  size> }`, and `compute_dedup(&[EntityScan])` returns
  `(Vec<EntityContribution>, Vec<BackendDedup>)` — the per-entity
  exclusive/shared split and the per-backend unique pool bytes. No
  trait, no I/O, no serde. Entity enumeration differs per product
  (VTL walks each cartridge's `chunks.idx`, VSA each volume's
  `pages.idx`) and stays in each daemon's `job_dispatch::stats`.
- **shared-disk-evict** — the two genuinely identical halves of each
  daemon's disk-cache eviction worker. `resolve_and_apply_caps` does
  the `auto`-mode per-backend cap recompute against current free space
  (byte-for-byte the same in both daemons) and pushes the new ceilings
  into each backend's `PoolBudget`; `check_usage_or_alert` logs the
  within-budget utilization line, fires the soft-watermark alert, and
  returns whether eviction is needed. The wakeup source (VTL:
  upload-completion `Notify` + 5-min backstop; VSA: interval tick) and
  the evict call itself (VTL's async cloud-backup evict vs VSA's sync
  fs-only trim) genuinely differ and stay per-daemon; both now offload
  the blocking usage walk + eviction to `spawn_blocking`.
- **shared-object-store** — storage-backend abstraction. `object_store_backend.rs` (the
  `ObjectStoreBackend` trait), `object_store_config.rs` (`ObjectStoreConfig` schema +
  `FailureKind` classifier + `validate_object_store_backend`),
  `object_store_helpers.rs` (decorrelated-jitter retry), `s3.rs` / `gcs.rs` /
  `azure.rs` / `local.rs`, `compression.rs` (LZ4 / zstd primitives).
  Errors are surfaced via `ObjectStoreError`; `core-mediachanger` defines
  `From<ObjectStoreError> for VtlError` so `?` propagation is unaffected, and
  re-exports the flat names (`core_mediachanger::ObjectStoreBackend`,
  `…ObjectStoreConfig`, `…CompressionAlgo`, …).
- **shared-object-store-bench** — first-party storage-backend throughput
  benchmark engine. `run` drives N parallel `ObjectStoreBackend::upload_chunk`
  / `download_chunk` / `delete_object` calls through the same SDK +
  network path the daemon uses. Output: `[BENCH]` lines on stdout,
  `[BENCH-ERR]` on stderr without aborting sibling cells. Knobs via
  `BenchOptions`. Consumed by the `system storage benchmark` CLI verbs of
  both products.
- **shared-iscsi** — cross-product iSCSI primitives.
  - `alua.rs` — ALUA topology (SPC-4 §5.16). `AluaTopology` built
    from `ServerConfig::listen_portals`: sequential per-portal
    RTPIs (1, 2, …), per-TPG `AsymmetricAccessState` (default
    `ActiveOptimized`), stable per-port NAA-3 identifier derived
    from a daemon-supplied namespace (chassis serial for VTL,
    target IQN for VSA). Exposes
    `push_vpd83_target_port_descriptors` (the three TP-association
    designators per portal: NAA + RelativeTargetPort +
    TargetPortGroup) and `report_target_port_groups_body`
    (REPORT TPG body, one descriptor per distinct TPGT).
  - `auth.rs` — CHAP (MD5 / SHA-1 / SHA-256 / SHA3-256 with
    target-preference selection).
  - `unit_attention.rs` — per-(TSIH, LUN) UA queue.
  - `session.rs` — `SessionManager` (TSIH / CmdSN / StatSN bookkeeping,
    partition fence, RFC 3720 §3.2.2.1 CmdSN window policy).
  - `sense.rs` — generic SCSI sense surface (`SenseKey`,
    `AdditionalSenseCode`, ASC/ASCQ table, `SenseDataBuilder`); a thin
    re-export shell of `scsi-spc`.
  - `error.rs` — `IscsiError` (`InvalidOp` / `InvalidSession` /
    `AuthFailed`; `From<IscsiError> for VtlError` lives in
    core-mediachanger).
  - `metrics.rs` — pluggable `MetricsSink` trait; the consuming product
    installs a forwarder so the `sessions_active` gauge lands in its
    OTel MeterProvider.
  - `transport.rs` — connection lifecycle: `Pdu` + `read_pdu` /
    `write_pdu` / `build_empty_pdu` (RFC 3720 §11 BHS framing),
    `handle_login_phase`, `collect_write_data` / `send_r2t` /
    `derive_r2t_ttt` (R2T loop), the per-connection FFP loop
    `serve_connection`, accept-loop `run`.
  - `handler.rs` — the product-agnostic `ScsiHandler` trait
    (`dispatch(ScsiRequest) -> ScsiResponse`, `target_iqn`,
    `on_session_close`); VTL's `VtlTapeHandler` and VSA's
    `IscsiDiskScsiHandler` both impl it. Login-phase audit emits
    through the `LoginAuditSink` trait; `ServerConfig::audit` is an
    `Arc<dyn LoginAuditSink>` so a product can branch at runtime
    between its real adapter and `NoopLoginAudit`.
- **shared-keystore** — pluggable VSA volume-DEK keystore.
  `KeyStoreBackend` trait (`generate_and_wrap` / `wrap` / `unwrap` /
  `forget` / `health_check`) + six backends: `local.rs` (on-disk
  plaintext sidecar), `awskms.rs`, `vault.rs`, `azurekv.rs`,
  `gcpkms.rs`, `kmip.rs` (+ `kmip_wire.rs` hand-rolled TTLV). Config /
  auth resolution via `keystore_config.rs`; `error.rs`,
  `passphrase_envelope.rs`. Tagged-enum config mirrors `shared-object-store`.
- **shared-audit** — append-only, BLAKE3-chained audit log. `audit.rs`
  (`AuditLog`, daily JSONL rotation + zstd-encoded post-rollover
  compression, `replay_pending` queue drain, `verify` / `read_entries`,
  `AuditActor` / `AuditEntry` / `AuditResult`, `AuditMode::
  TamperEvident`), `audit_channel.rs` (cloneable `AuditChannel`
  producer + bounded mpsc + single-writer drainer `spawn_writer`,
  `Shutdown` sentinel draining FIFO before exit), `audit_ratelimit.rs`
  (`AuditRateLimiter` — host-driven failure rollups over a 60 s window
  with a 10 s flush sweep).
  Internal metric calls forward to `shared-telemetry` for the
  `audit_entries_total` / `audit_chain_resets_total` /
  `audit_queue_drops_total` counters. core-mediachanger re-exports the
  flat names (`core_mediachanger::AuditLog`, `…AuditChannel`,
  `core_mediachanger::audit::*`, …).
- **shared-telemetry** — OpenTelemetry SDK plumbing. One
  `SdkMeterProvider` with two readers: Prometheus pull (always wired) +
  OTLP push (opt-in). The `Telemetry` struct carries every instrument
  handle (pool, storage, chunk, iscsi, tape, prefetch, audit, daemon).
  Per-product instrument prefix (`thurvtl_*` / `thurvsa_*`)
  sourced from `shared_naming::PRODUCT.metric_prefix`; the
  `service.name` OTel resource attribute (`thurvtl` / `thurvsa`)
  carries the distinction redundantly. A process-global
  `OnceLock<Telemetry>` is installed at boot via `set_global`; `record::*`
  free functions forward through it and no-op when it is unset (CLI /
  unit tests / `--test` smoke). core-mediachanger re-exports it as
  `pub use shared_telemetry as metrics;`.
- **shared-upload-worker** — storage-upload pipeline scaffold shared by
  tape (core-stream / VTL) and block (core-block / VSA). Two surfaces:
  `upload_chunk_inert(backend, &PendingUpload)` — stateless PUT with a
  storage-side dedup HEAD probe (under `DedupScope::Global`), returns
  `UploadOutcome`; no cartridge / volume borrow held during the await.
  `run_upload_pipeline` — drives a batch through `upload_chunk_inert`
  with at most `max_concurrent` PUTs in flight (`buffer_unordered`),
  each completion firing a caller-supplied post-upload hook
  (apply-outcome, auto-hold, eviction-Notify). Product-specific glue
  (the `mpsc` request type, crash-recovery scan, cartridge / volume
  open) stays in the daemons. `core_stream::cartridge` re-exports the
  payload types under their legacy names (`PendingUploadPayload`,
  `ChunkUploadOutcome`).
- **shared-verify-core** — cross-product chunk-pool + storage
  verification sweeps for `system verify`. A product implements the
  `VerifyTarget` trait — `live_chunks()` (the `(backend, namespace) ->
  {hash}` map) and `cloud_entities()` (per-entity storage expectations).
  `sweep_local_pool(data_dir, target)` returns one `PoolSweep` per
  backend (the local orphan scan over every `(backend, namespace)`
  pool); `sweep_storage(target, backend_name, backend)` runs the bounded
  HEAD storm against one storage backend and the `chunks/` orphan scan.
  The tape side additionally HEADs index-page objects + the manifest
  sentinel (no block analogue, stays in `core-mediachanger`); each
  product assembles its own `VerifyReport` from the sweep results.
- **shared-naming** — single source of truth for per-product identity
  strings (system user, IQN, NQN, conffile path, data dir, run dir,
  admin socket, service unit, metric prefix). `ProductIdentity` struct
  + consts `TAPE` / `TAPE_LIBRARY` / `DISK` (`TAPE_LIBRARY` =
  `thurvtl`, `DISK` = `thurvsa`), plus `VENDOR_DOMAIN`,
  `VENDOR_INQUIRY`, `MAX_QUALIFIED_NAME_LEN`.
- **shared-pool** — content-addressed chunk pool. One `ChunkPool`
  carrying both insertion APIs (`insert_bytes(&[u8])` for buffer-driven
  block writes, `insert_from_path(src, hash)` for staging-file-driven
  tape writes), namespace-aware `object_key`, `iter_chunks` for GC,
  atomic tmp+rename inserts. Layout
  `<root>/chunks/<backend>/[<namespace>/]<aa>/<bb>/<hash>.dat`.
  core-stream's `chunk_store.rs` aliases `ChunkPool` as `ChunkStore`
  (core-mediachanger re-exports it flat); core-block's `chunk_pool.rs`
  re-exports verbatim. `From<ChunkPoolError> for VtlError` lives in
  `core/stream/src/errors.rs`.

## scsi crates

- **scsi-spc** — SPC-4 baseline. `sense.rs` (the unified `SenseData`
  value carrying both fixed-format / 0x70 and descriptor-format / 0x72
  encodings, the fluent `SenseDataBuilder`, the ASC/ASCQ table, named
  consts like `SenseData::INVALID_OPCODE`), `scsi.rs` (`ScsiStatus` /
  `ScsiRequest` / `ScsiResponse` — the one canonical shape;
  `ScsiResponse.sense` is `Option<SenseData>`, transport serializes via
  `to_bytes()` at PDU-wrap time), `lun.rs` (SAM-5 single-level +
  flat-space LUN encoder / decoder), `inquiry.rs` (INQUIRY standard
  data layout + identity ASCII padding; `build_inquiry_std` emits the
  36-byte SPC-4 minimum + a per-caller `InquiryFlags { spc_version,
  hisup, cmdque }`), `vpd.rs` (VPD page header + designator descriptor
  framing — pages 0x00 / 0x80 / 0x83; `DesignatorType::
  LogicalUnitGroup` (0x06) for the LUG descriptor), `report_luns.rs`
  (`build_report_luns`), `mode.rs` (MODE PARAMETER HEADER 6/10 encoders
  + length patchers), `pr.rs` (persistent-reservation primitive types —
  scope / type / service-action enums, `ReservationKey` newtype),
  `reservations.rs` (the transport-neutral PERSISTENT RESERVE state
  machine — `ReservationManager` + `Nexus`, per-LUN registrations /
  reservation / `PR_GENERATION`, the PROUT service-action handlers, the
  PRIN renderers, `allow_read` / `allow_write` / `drop_nexus`, and the
  `prin` / `prout` entry points returning the response-neutral
  `PrInOutcome` / `PrOutOutcome`; both products' dispatchers are thin
  adapters over it). shared-iscsi's `sense.rs` + `handler.rs`
  request/response types and VSA's `scsi/types.rs` are thin re-export
  shells of scsi-spc.
- **scsi-ssc** — drive-LUN SCSI dispatch + drive-manager primitives +
  tape SCSI helpers (sense, log pages, MAM attributes, encryption
  pages). PERSISTENT RESERVE IN/OUT (0x5E / 0x5F) run against the
  shared `scsi_spc::reservations::ReservationManager` (threaded through
  `ScsiCtx`) on both the drive LUN and the changer LUN — keyed per-LUN,
  so the two are independent. `dispatch_drive_lun` enforces the
  reservation gate that fences medium read/write opcodes with
  RESERVATION CONFLICT (the changer's gate lives in scsi-smc).
  Consumed by `thurvtld`.
- **scsi-smc** — changer-LUN SCSI dispatch (the six SMC opcodes:
  INITIALIZE / READ ELEMENT STATUS, MOVE / EXCHANGE MEDIUM, SEND VOLUME
  TAG, INITIALIZE WITH RANGE) plus element-address topology helpers
  (`ElementType`, `ElementAddressConfig`). The per-command `SmcScsiCtx`
  wraps scsi-ssc's `ScsiCtx`. `pr_enforce` is the changer-side mirror of
  scsi-ssc's reservation gate: a reservation held on the changer LUN
  fences MOVE / EXCHANGE / element-status opcodes with RESERVATION
  CONFLICT (issue #53). Consumed by `thurvtld`.
- **scsi-sbc** — SBC-3 block-target SCSI dispatch (every data-path
  opcode: READ / WRITE 10/16, COMPARE AND WRITE, UNMAP, WRITE SAME,
  SYNCHRONIZE CACHE; INQUIRY + VPD, READ CAPACITY, REPORT LUNS, MODE
  SENSE/SELECT, PERSISTENT RESERVE IN/OUT, MAINTENANCE IN, probes).
  `SbcScsiDispatcher` implements `shared_iscsi::ScsiHandler`; the
  daemon plugs its `VolumeRegistry` in via the `VolumeLookup` trait.
  Consumed by `thurvsad`.

## nvme crates

- **nvme-base** — NVMe Base Spec primitives: 64-byte SQE, 16-byte CQE
  (SCT / SC / DNR), Admin opcode enum, FUSE / PSDT sub-fields, Identify
  Controller / Namespace / Active NS list builders, Fabrics shapes
  (`ConnectData`, `FabricsType`, `extract_fctype`), `ControllerRegs`
  (CC / CSTS / VS / CAP), log-page builders (SMART, Error Info, FW
  Slot).
- **nvme-nvm** — NVM Command Set dispatch (Read / Write / Flush /
  Compare / Write Zeroes / DSM Deallocate / Verify; fused Compare+Write
  via `handle_fused_compare_write`). Admin coverage: Identify, Keep
  Alive, Get/Set Features (Number of Queues), Get Log Page, Abort.
  `NvmeNvmDispatcher` impls `NvmeCommandHandler`; the daemon plugs
  `VolumeRegistry` in via the `NamespaceLookup` trait. Reaches into
  `core-block::PageCache` directly. `nsid = lun + 1`.
- **nvme-tcp** — NVMe/TCP transport. Per-connection state machine:
  ICReq/ICResp handshake (advertises MAXH2CDATA = 128 KiB, captures
  host MAXR2T), Connect with SUBNQN admission, Property Get/Set against
  shared `ControllerRegs`, Disconnect, command loop with R2T flow,
  fused Compare+Write pair tracking, C2HData SUCCESS-bit folding,
  C2HTermReq on protocol violations, per-controller CNTLID allocation at
  Connect, reservation notifications via AER (Admin 0x0C) + LID 0x80
  parked on a shared `nvme_nvm::ControllerRegistry`. Out of scope:
  TLS-PSK auth, CRC32C digests, multi-outstanding R2T, discovery
  controller.

## core-stream

- **core-stream** — SSC-4 / LTO tape-cartridge primitives. `tape.rs`
  (Block / BlockKind / Filemark), `cartridge/` (Cartridge, manifest,
  write_data, read_block, capacity / Early Warning / EOM, partitions —
  split into `mod.rs`, `chunking.rs`, `storage.rs`, `indexing.rs`,
  `runtime.rs`), `cartridge_archive.rs` / `cartridge_migrate.rs`
  (cross-backend / cross-region ops), `block_index.rs` (per-partition
  LBA index), `chunk_index.rs` (per-cartridge chunk index),
  `lru_index.rs`, `dirty_pages.rs` + `index_backup.rs` (page-granular
  index backups to the storage backend), `prefetch.rs` (sequential read-ahead),
  `mode_state.rs` (SCSI MODE SELECT round-trip bodies), `drive_state.rs`
  (per-drive emulated NVRAM), `fastcdc.rs` (content-defined chunking),
  `encryption.rs` (LTO Application-Managed Encryption — AES-256-GCM),
  `disk_cache.rs` (refcount-aware eviction + `PoolBudget`),
  `chunk_store.rs` (re-export shell on `shared_pool::ChunkPool`),
  `errors.rs` (`VtlError` + `From<ObjectStoreError>` / `From<IscsiError>` /
  `From<ChunkPoolError>` bridges), `legal_hold.rs` (cartridge sentinel
  `manifests/<barcode>/manifest-latest.json`, key collection,
  apply-cartridge / read-status), `drive_topology.rs` (the
  `DriveTopology` trait).

## core-mediachanger

- **core-mediachanger** — SMC-3 medium-changer + library inventory +
  library-wide verify. Composes `core-stream`. `library/` (topology +
  inventory + the `DriveTopology` impl — drive_count / drive_ids /
  partition_for_drive — over the slot grid + drive list; split into
  `mod.rs`, `inventory.rs`, `partitions.rs`, `restore.rs`,
  `restore_archive.rs`), `legal_hold.rs` (smc-side
  `find_drive_for_loaded_cartridge` — the only inventory-coupled hold
  helper; cartridge sentinel logic lives in core-stream),
  `daemon_lock.rs` (`DaemonLock`, `check_daemon_not_running`,
  `is_daemon_running`), `events.rs` (`PositionChangeReason`,
  `TapeEvent`), `verify.rs` (cross-cartridge auditor), `lbp.rs`
  (Logical Block Protection trailer), `direct_io.rs` / `io_uring.rs`
  (vestigial, feature-gated). Flat-re-exports the core-stream surface
  (`core_mediachanger::Cartridge`, `…ChunkStore`,
  `core_mediachanger::cartridge`, …) plus the shared-object-store /
  shared-audit / shared-telemetry crates' flat names + module paths.

## vtl crates

- **vtl-daemon** (binary `thurvtld`) — Tokio async, IQN
  `iqn.2025-10.com.metebalci:thurvtl`. iSCSI target on 3260 routed via
  `shared_iscsi::transport::run` against `VtlTapeHandler`
  (`iscsi/handler.rs`) — the trait impl wraps the SSC-4 / SMC-3 / SPC-4
  dispatch tree (`iscsi/protocol.rs::handle_scsi_command` + the
  per-opcode `handle_*` arms) and threads the storage-prefetch / SEND
  DIAGNOSTIC self-test / MOVE MEDIUM legal-hold hooks around it.
  `iscsi/server.rs` constructs the handler from `DaemonState` and
  builds a `VtlLoginAudit` adapter bridging shared-iscsi login events
  into VTL's audit channel. Other surfaces: `config.rs`,
  `drive_manager.rs`, `iscsi/scsi/*`, `memory_buffer_manager.rs`
  (per-tape memory buffers, drives upload/prefetch workers),
  `diagnostics.rs` (SCSI SEND / RECEIVE DIAGNOSTIC RESULTS — per-LUN
  self-test ring buffer feeding page 0x10), `main.rs`. HTTP on 9090
  (`/health`, `/metrics`, `/sessions`, `/info`). `--test` runs
  in-process smoke tests and exits. Admin socket at
  `/run/thurvtl/admin.sock` — `admin/mod.rs` builds the product router,
  merges `jobs_router` (dispatch in `admin/job_dispatch/*.rs`: gc /
  verify / stats / cloud_check (routed to `shared-admin-cloud-check`; CLI verb `system storage check`) / self_test / audit / archive /
  restore_archive / migrate / alerting) and the alerting route, and
  hands off to `run_admin_server`.
- **vtl-cli** (binary `thurvtl`) — top-level subcommands `library`,
  `cartridge`, `changer`, `drive`, `system`, `iscsi`, `alerting`,
  `config`. Shared formatters in `output/formatters.rs`.

## core-block

- **core-block** — SBC-3 direct-access (block) device-type core,
  consumed by `vsa-daemon`. Per-volume page table (sparse `page_id →
  chunk_hash` map), 4 KiB sectors, default 64 KiB page,
  thin-provisioned, Global dedup scope by default.

### core-block layout

- `core-block::volume::VolumeManifest` — on-disk identity schema at
  `<data_dir>/volumes/<name>/manifest.json` (atomic tmp+rename).
  Creation-frozen: `name`, `uuid` (16-byte hex), `size_bytes`,
  `sector_bytes`, `page_size_bytes`, `backend`, `dedup_scope`, `worm`,
  `created_at`, optional `encryption`, optional `dedup_namespace`
  (schema v5; the family chunk-pool namespace inherited by a clone —
  absent means "namespace from my own uuid", routed through
  `pool_namespace()`/`dedup_namespace_uuid()`), and optional
  `crypto_uuid` (schema v6; the crypto identity a clone of an encrypted
  volume inherits — absent means "crypto identity from my own uuid",
  routed through `dek_uuid()`, seeds AES-GCM IV + keystore wrap-context).
  Volume names are 1-64
  ASCII alphanumeric + `-`/`_`. `VolumeManifest::create` also
  materializes the empty page index (`pages.idx`) plus a zero-valued
  `runtime.json` — a volume directory always has all three files or none.
- `core-block::snapshot::SnapshotManifest` — frozen point-in-time
  snapshot of a volume's page table (issue #13) at
  `<data_dir>/volumes/<parent>/snapshots/<snap>/{snap.json, pages.idx}`.
  Carries the parent uuid (binds the copied index), the family
  `dedup_namespace`, backend, dedup scope, page/sector size, the
  parent's live size, optional encryption, and optional `crypto_uuid`
  (schema v2; copied from the parent so a clone made from a snapshot of
  an encrypted clone inherits the right crypto identity). Nested under
  the parent so the discovery LUN walk skips it while GC + eviction
  descend into
  `snapshots/`. `list_all` is the cross-volume walk GC uses to fold
  snapshot indexes into the live set.
- `core-block::runtime_state::VolumeRuntime` — daemon-mutated sidecar
  at `<data_dir>/volumes/<name>/runtime.json` (atomic tmp+rename).
  Carries `host_bytes_written` (lifetime host write counter,
  pre-dedup/compression) and `modified_at`. Split
  out of the manifest so the identity file stays byte-stable
  post-create and `volume key migrate` can rewrite `encryption.*`
  daemon-up without racing the writer's flush.
- `core-block::page_index::PageIndex` — sparse `page_id → BLAKE3 hash`
  map at `<data_dir>/volumes/<name>/pages.idx`. 64-byte header (`CRPI`
  magic, version 1, volume uuid + page size rebound) followed by
  64-byte fixed-size records indexed positionally (`offset = 64 +
  page_id * 64`). Grows via sparse-file holes — unallocated pages
  consume zero disk on ext4 / btrfs / xfs / zfs. Each `set` / `clear`
  is one `pwrite_at` + `sync_data`; no in-memory cache.
- `core-block::chunk_pool::ChunkPool` — per-backend content-addressed
  pool, layout `<data_dir>/chunks/<backend>/[<volume>/]<aa>/<bb>/
  <hash>.dat`. Optional volume-name namespace fires under `Local` dedup
  scope (no cross-volume sharing); absent under `Global`. `insert_bytes`
  is atomic, idempotent on hash collision; `object_key` drops the
  per-backend prefix but keeps the volume namespace.
- `core-block::uploader::VolumeWriter` — per-volume page write
  primitive. `open(data_dir, name, Arc<dyn ObjectStoreBackend>)` bundles
  `VolumeManifest` + `PageIndex` + `ChunkPool`. `write_page(page_id,
  &[u8])` runs BLAKE3 hash → pool insert → backend `upload_chunk`
  (HEAD-skipped on `Global` dedup hits) → page index `set`.
  `read_page(page_id)` is pool-first / backend-fallback. Synchronous
  per-call upload at this layer.
- `core-block::cache::PageCache` — per-volume in-memory write-back
  cache fronting `VolumeWriter`. Byte-grained API (`read_bytes` /
  `write_bytes` / `compare_and_write_bytes` / `unmap_bytes` /
  `synchronize_bytes`); sub-page host I/O via RMW (load the affected
  page(s) from cache or `VolumeWriter::read_page`, splice host bytes at
  sector grain, mark dirty). LRU eviction under a byte budget (default
  `DEFAULT_CACHE_BUDGET_BYTES = 64 MiB`); evicting a dirty page flushes
  through `write_page` first. Optional background flush worker
  (`run_flush_worker`) wakes on a `Notify` (signaled at 50 % of budget)
  and a 5 s tick, drains the dirty set, exits when `request_shutdown`
  is set. SYNCHRONIZE CACHE is a real fence (await flush of every dirty
  page in the LBA range). Concurrency: single `tokio::sync::Mutex` over
  cache state; load and flush paths drop the lock during the backend
  await. Dirty pages carry a monotonic version counter so a flush
  racing a host rewrite leaves the entry dirty for the next pass.
- Other modules: `disk_cache.rs`, `lru_index.rs`, `upload_index.rs`.

### thurvsad SCSI dispatcher

`vsa-daemon`'s `scsi::IscsiDiskScsiHandler` is the SBC-3 dispatcher.
Holds an `Arc<VolumeRegistry>` (LUN → `Arc<PageCache>` map, built at
boot by walking `<data_dir>/volumes/`, `RwLock`-backed — read on every
dispatch, written on `volume create` / `destroy`) plus an
`Arc<ReservationManager>`, and dispatches four surfaces.

**Discovery.** TEST UNIT READY (0x00), INQUIRY (0x12) standard data +
VPD pages 0x00 / 0x80 / 0x83 / 0xB0 / 0xB2 — 0xB0 (Block Limits)
advertises MAXIMUM COMPARE AND WRITE LENGTH = sectors-per-page;
0xB2 (Logical Block Provisioning) sets LBPU=1, LBPRZ=001, PROVISIONING
TYPE=010 (thin). READ CAPACITY 10 (0x25) caps at `0xFFFFFFFF` to force
RC16 on big volumes; READ CAPACITY 16 (0x9E sa 0x10) full 8-byte last
LBA + LBPME=1 + LBPRZ=1 byte 14. REPORT LUNS (0xA0) with SAM-5
single-level + flat-space encoding, MODE SENSE 6 / 10 (0x1A / 0x5A) for
the Caching (0x08) and Control (0x0A) pages plus the all-pages alias
(0x3F).

**Host-probe stubs** (`scsi/probes.rs`): REQUEST SENSE (0x03) returns
NoSense, START STOP UNIT (0x1B) accepts any PowerCondition / LOEJ /
START as GOOD, PREVENT/ALLOW MEDIUM REMOVAL (0x1E) accepts any prevent
flags as GOOD, LOG SENSE (0x4D) serves page 0x00 only.

**Capability discovery** (`scsi/maintenance.rs`): MAINTENANCE IN (0xA3)
routes service action 0x0C REPORT SUPPORTED OPCODES (every routed CDB
in ascending order) and 0x0D REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS
(ATS / ATSS / CTSS / LURS / ITNRS).

**Data path.** WRITE 10 / 16 (0x2A / 0x8A), READ 10 / 16 (0x28 / 0x88),
VERIFY 10 / 16 (0x2F / 0x8F — BYTCHK=00 reads to surface medium errors,
BYTCHK=01 compares against data-out and emits MISCOMPARE, BYTCHK=10/11
rejected), SYNCHRONIZE CACHE 10 / 16 (0x35 / 0x91), WRITE SAME 10 / 16
(0x41 / 0x93 — UNMAP=1 + zero pattern routes via `cache.unmap_bytes`,
other patterns expand across the range via `cache.write_bytes` in
16 MiB sector-aligned chunks; NDOB=1 on the 16-byte form means
zero-fill; 16-byte NUMBER OF BLOCKS = 0 means "to end of medium",
10-byte form treats it as no-op; ANCHOR / WRPROTECT / PBDATA / LBDATA
rejected), COMPARE AND WRITE (0x89), UNMAP (0x42).

**Reservations.** PERSISTENT RESERVE IN (0x5E) service actions
0x00-0x03, PERSISTENT RESERVE OUT (0x5F) service actions 0x00-0x06;
REGISTER AND MOVE (0x07) is rejected with ILLEGAL REQUEST. INQUIRY
against unmapped LUNs returns the SPC-4 "no LUN" pattern (peripheral
qualifier 0b011 + type 0x1F) rather than CHECK CONDITION — initiators
rely on this to discover the LUN map.

Identity strings: vendor `THUR`, product `CIRRUS BLOCK`, revision
`0001`; serial / device-id are the volume UUID hex.

**MODE SELECT 6 / 10** (0x15 / 0x55) accepts parameter lists that
re-assert the values MODE SENSE returned (PF=1 required, SP=1 rejected
with SAVING PARAMETERS NOT SUPPORTED) — every Changeable bit is zero,
so the host can't flip WCE / RCD / DRA / D_SENSE; mutating any field
surfaces INVALID FIELD IN PARAMETER LIST.

**MODE SENSE** advertises WCE=1 / RCD=1 / DRA=1 (the in-memory
write-back cache is real and lost on daemon crash, so SBC-3 §6.4.6.4
mandates WCE=1), an SBC-3-baseline Control page body, and a short or
long block descriptor reflecting `(logical block count,
sector_bytes)`. WORM volumes flip WP=1 in the DEVICE-SPECIFIC PARAMETER
byte. PC=Changeable returns an all-zero mask. Unknown opcodes → CHECK
CONDITION + INVALID OP CODE.

**Sector-grain data path.** WRITE / READ / VERIFY / WRITE SAME / CAW /
UNMAP route every request through the per-volume `PageCache`. Sub-page
LBA / transfer length is supported via RMW. WORM volumes refuse WRITE /
CAW / UNMAP with WRITE PROTECTED (sense key 0x07, ASC/ASCQ 0x27/0x00).
Unallocated pages on READ surface as zeroed sector content (sparse
holes). SYNCHRONIZE CACHE is a real fence —
`cache.synchronize_bytes` awaits storage-backend ack of every dirty page in the
LBA range; out-of-range sync gets ILLEGAL REQUEST.

COMPARE AND WRITE (0x89) routes through `cache.compare_and_write_bytes`:
phase 1 compares on-disk + cached bytes against the host's first half
of `data_out`; phase 2 commits the second half via `write_bytes` only
on a clean match. Diff returns CHECK CONDITION + MISCOMPARE (sense key
0x0E, ASC/ASCQ 0x1D/0x00) without writing. The triple is atomic against
other CAWs on the same LUN via a per-LUN `tokio::sync::Mutex` registry
(`scsi/data_path.rs::CawLocks`). Sub-page CAW (1-sector VMFS heartbeat)
is honored end-to-end. UNMAP (0x42) parses an 8-byte header + N ×
16-byte UNMAP BLOCK DESCRIPTOR list, validates every descriptor up
front, then routes each through `cache.unmap_bytes`: full-page
descriptors drop the cached entry and synchronously clear the
page-index slot (backend chunks linger until `system gc`); sub-page
descriptors zero the affected sectors and mark dirty.

UNMAP and CAW are advertised end-to-end: VPD 0xB0 carries MAXIMUM
COMPARE AND WRITE LENGTH / MAXIMUM UNMAP LBA COUNT / OPTIMAL UNMAP
GRANULARITY (= sectors-per-page), VPD 0xB2 carries LBPU / LBPRZ /
PROVISIONING TYPE, READ CAPACITY (16) sets LBPME + LBPRZ in byte 14.

**Reservations subsystem** (`scsi/sbc/src/reservations.rs`): a thin
adapter over the shared `scsi_spc::reservations::ReservationManager`
(the state machine itself was hoisted into scsi-spc so the tape drive
LUN can share it — see scsi-spc above). The adapter builds a `Nexus`
from the SBC `ScsiRequest`, parses the 0x5E / 0x5F CDBs via the shared
slicers, and maps `PrInOutcome` / `PrOutOutcome` onto `ScsiResponse`.
Per-LUN registrations + at most one reservation, keyed by I_T nexus
`(tsih, initiator_iqn)`; the SBC-3 type matrix WR_EX / EX_AC /
WR_EX_RO / EX_AC_RO / WR_EX_AR / EX_AC_AR is honored end-to-end (REPORT
CAPABILITIES advertises TYPE_MASK = `0xEA, 0x01`). Data-path
enforcement: WRITE / SYNCHRONIZE CACHE check `allow_write`, READ checks
`allow_read`; deny → SCSI status 0x18 (RESERVATION CONFLICT) with no
sense. State is in-memory only — PTPL advertised as not capable.
`ScsiHandler::on_session_close` calls
`ReservationManager::drop_nexus(tsih)`. The thurvtl tape drive LUN
reuses the same manager (`vtl/daemon` threads an `Arc<ReservationManager>`
through `ScsiCtx`; enforcement + handlers live in scsi-ssc).

Modules:
`vsa/daemon/src/scsi/{types,handler,inquiry,sizing,data_path,mode_sense,reservations,probes,maintenance}.rs`
+ `vsa/daemon/src/registry.rs`.

### thurvsad boot wiring

`config.rs` reads `/etc/thurvsa/thurvsa.yaml` (`--config PATH`
overrides) into `DaemonConfig { data_dir, storage:
shared_object_store::ObjectStoreConfig }` and validates the storage section.
`discovery.rs` walks `<data_dir>/volumes/` via `VolumeManifest::list`,
sorts by name (deterministic LUN map across restarts), instantiates one
`Arc<dyn ObjectStoreBackend>` per unique `manifest.backend`, opens a
`VolumeWriter` per volume, and returns a populated `VolumeRegistry`.
Volumes referencing an undefined backend are a hard fail. `main.rs`
parses the CLI, loads the config, opens the `shared_audit::AuditLog`,
runs discovery, builds a `ChapAuthenticator` from `iscsi.auth` if
`enabled`, instantiates `IscsiDiskScsiHandler`, and binds
`shared_iscsi::transport::run` on `0.0.0.0:3260`. Target IQN is
`iqn.2025-10.com.metebalci:thurvsa`.

- `vsa-daemon::auth::build` mirrors VTL's `IscsiServer::new` CHAP
  construction. Same `allowed_algorithms` aliases (`MD5` / `SHA-1` /
  `SHA-256` / `SHA3-256`, integer IDs `5..=8`). Disabled by default.
  No per-user `partition` field — VSA has no library topology.
- `vsa-daemon::audit::IscsiDiskLoginAudit` is the `LoginAuditSink`
  adapter, forwarding `LoginAuditEvent::ChapSuccess` / `ChapFailure`
  into the `AuditChannel` as `iscsi.chap.success` / `iscsi.chap.failure`
  — same op names VTL emits. On shutdown a `daemon.stop` entry is
  pushed before `writer.shutdown().await`.
- `audit.enabled: false` is a dev escape hatch — runs without an audit
  log, falls back to `NoopLoginAudit`. Default is on.

### thurvsad admin socket

Unix socket at `/run/thurvsa/admin.sock` (mode 0660). Same `axum`
router over a hyper-served `tokio::net::UnixListener` and `PeerCred`
SO_PEERCRED plumbing as VTL. Routes: `GET /api/v1/health`, `GET
/api/v1/volumes`, `GET /api/v1/volumes/{name}`, `POST /api/v1/volumes`
(live create — writes the manifest, instantiates / reuses a cached
`Arc<dyn ObjectStoreBackend>`, opens a `VolumeWriter`, picks the next free
LUN via `VolumeRegistry::next_free_lun`, registers — the SCSI
dispatcher sees the new LUN on its next command without a restart),
`DELETE /api/v1/volumes/{name}` (live destroy — unregisters and removes
the on-disk volume directory; per-volume chunks stay for the GC sweep).
Mutations emit `volume.create` / `volume.destroy` audit entries.

### thurvsad HTTP

Unified HTTP server on `0.0.0.0:9090`. Routes (`vsa/daemon/src/http.rs`):
`/health`, `/metrics`, `/sessions` (from shared crates) plus VSA's
local `/info` (`{ daemon, version, listen_address, iqn, volume_count }`).
Telemetry installed via `shared_telemetry::set_global` at boot with
`service.name=thurvsa`; instrument prefix `thurvsa_*` from
`shared_naming::PRODUCT.metric_prefix`.

### thurvsa

`vsa-cli` (binary `thurvsa`) reads `/etc/thurvsa/thurvsa.yaml` (or
`--config PATH`) for `data_dir` and the `storage.backends` registry.
Top-level subcommands: `volume`, `system`, `iscsi`, `nvmetcp`,
`alerting`, `config`. `volume create / list / info / destroy / modify`
are daemon-routed; `volume key migrate` and `system regenerate-cert`
are daemon-down. `config` is pure-local. The
runtime is a `tokio::Builder::new_current_thread()` block-on.
`vsa/cli/build.rs` `include!`s `src/cli.rs`, regenerates
`dist/thurvsa-completion.{bash,zsh}` on every relevant build,
mirrors `src/commands/defaults_reference.yaml` to
`dist/thurvsa.defaults.yaml`, and emits `THURVSA_VERSION`.

## Shared infrastructure summary

The shared layer exists so the two products never diverge silently on
behaviors that have to match. `shared-object-store` carries the `ObjectStoreBackend`
trait plus S3 / GCS / Azure / Local implementations, retry logic, and
compression primitives. `shared-audit` carries the BLAKE3-chained audit
log, the cloneable `AuditChannel` producer and single-writer drainer,
and the `AuditRateLimiter`. `shared-telemetry` carries the OpenTelemetry
SDK plumbing, the Prometheus pull and OTLP push readers, and the
`record::*` global-handle pattern. `shared-iscsi`,
`shared-admin-*`, `shared-cli-*`, `shared-keystore`, `shared-alerting`,
`shared-health`, `shared-object-store-bench`, and `shared-upload-worker` are
likewise consumed by both products. The tape-side disk cache lives in
`core-stream`; the block-side equivalent is in `core-block`.
