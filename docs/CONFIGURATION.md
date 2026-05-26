# Configuration

This document covers every file Thur VTL and Thur VSA read configuration
or state from, and every key the daemon YAML conffile accepts.

**Authoritative source.** The per-key YAML detail here mirrors
`vtl/cli/src/commands/defaults_reference.yaml` and
`vsa/cli/src/commands/defaults_reference.yaml` — the source of the
auto-generated `dist/{thurvtl,thurvsa}.defaults.yaml` and of
`<product> config defaults`. The generated `dist/*.defaults.yaml`
is the machine-checked reference; **this document must be updated in
the same commit whenever a YAML key is added or changed.**

---

## Configuration files

| File | Location | Product | Format | Managed by |
|---|---|---|---|---|
| `thurvtl.yaml` | `/etc/thurvtl/` (override: `--config PATH`) | VTL | YAML | operator (hand-edited) |
| `thurvsa.yaml` | `/etc/thurvsa/` (override: `--config PATH`) | VSA | YAML | operator (hand-edited) |
| `thurvtl.env` / `thurvsa.env` | `/etc/thurvtl/` / `/etc/thurvsa/` | both | `KEY=VALUE` | operator; loaded by systemd `EnvironmentFile` |
| `library.json` | `<data_dir>/library/` | VTL | JSON | daemon (materialized from YAML `library:` block on first start, reconciled on subsequent starts) + `thurvtl library partition` for partition layout |
| `inventory.json` | `<data_dir>/library/` | VTL | JSON | daemon + `thurvtl changer` ops |
| `iscsi-users.json` | `<data_dir>/` | both | JSON | `<product> iscsi users` / `iscsi target` |
| `nvmetcp-psks.json` | `<data_dir>/` | VSA | JSON | `thurvsa nvmetcp psks` |

The YAML conffile carries install-time and tuning knobs. The JSON files
hold operationally-mutated, credential-bearing state and must never be
hand-edited; use the CLI verbs listed above instead. `<data_dir>` is the
daemon's data directory, set by the `data_dir` key in the YAML.

---

## The daemon conffile

The daemon and CLI both resolve the conffile path the same way: use
`--config PATH` if it was given on the command line, otherwise fall back
to `/etc/<product>/<product>.yaml`. There is no working-directory
fallback, so the path is always explicit. The only required key is
`data_dir`; everything else has a default or is optional.

The full annotated reference — every key with its type, default, and a
description — is available as `thurvtl config defaults` or
`thurvsa config defaults`, and is also checked in at
`dist/*.defaults.yaml`.

### `data_dir`

| Key | Default | Description |
|---|---|---|
| `data_dir` | **none — required** | Local storage root: disk cache / chunk pool, manifests, library or volume state, the audit log. |

### `transport` — VSA only

| Key | Default | Description |
|---|---|---|
| `transport` | `iscsi` | Host-facing data path: `iscsi` binds the SCSI-over-TCP listener, `nvmetcp` binds the NVMe/TCP listener. Mutually exclusive — only one binds. |

### `iscsi`

| Key | Default | Description |
|---|---|---|
| `iscsi.listen` | `0.0.0.0:3260` | iSCSI target listen address. |
| `iscsi.target_iqn` | `iqn.2025-10.com.metebalci:thurvtl` / `:thurvsa` | Target IQN advertised to initiators. |
| `iscsi.max_sessions` | `10` | Max concurrent iSCSI sessions. **VTL only.** |
| `iscsi.session_timeout_seconds` | `300` | Per-session inactivity timeout. **VTL only.** |
| `iscsi.auth.method` | `None` | `None` (unauthenticated) or `CHAP`. |
| `iscsi.auth.allowed_algorithms` | `[SHA3-256, SHA-256, SHA-1, MD5]` | Allowed CHAP digests, strongest first. Empty list falls back to all four. |

CHAP users and the mutual-CHAP target credential live in
`iscsi-users.json`, not in the YAML — see *Daemon-managed state files*.

### `nvmetcp` — VSA only

This section is consulted only when `transport: nvmetcp`.

| Key | Default | Description |
|---|---|---|
| `nvmetcp.listen` | `0.0.0.0:4420` | NVMe/TCP listen address (IANA nvme-tcp port). |
| `nvmetcp.subnqn` | `nqn.2025-10.com.metebalci:thurvsa` | NVMe subsystem NQN. The TLS-PSK derivation binds to this — changing it rederives every per-host PSK. |
| `nvmetcp.tls.mode` | `disabled` | `disabled` (cleartext) or `psk` (TLS 1.3 with the two NVMe-TCP mandated cipher suites). |
| `nvmetcp.tls.identity_file` | `<data_dir>/nvmetcp-psks.json` | Path to the TLS-PSK host-identity file. |

### `memory_buffers` — VTL only

The memory-buffer pool is per-tape RAM that the daemon keeps alive
between iSCSI operations, used for write staging and read prefetch. It
is entirely separate from the on-disk `disk_cache`.

`write_gb_per_tape` and `read_gb_per_tape` each accept either an
integer GB count or the literal `"auto"`. Under `auto` (the default),
the daemon reads `/proc/meminfo MemTotal` once at boot, budgets
`auto_host_fraction_pct%` of host RAM across `library.num_drives`,
splits the per-drive share 2:1 between write and read (preserving the
historical 10 GB / 5 GB ratio), and clamps each field to
`[auto_min_gb_per_tape, auto_max_gb_per_tape]`. The resolved total
footprint, `(write + read) × num_drives`, is then checked against
`safety_max_host_fraction_pct%` of `MemTotal`; the daemon refuses to
start if exceeded — that's how an explicit operator override that
overcommits a small host is caught. Resolution is one-shot at boot;
changing host RAM, drive count, or these knobs needs a restart.

| Key | Default | Description |
|---|---|---|
| `memory_buffers.write_gb_per_tape` | `auto` | Per-tape write-buffer size. Integer GB pins the value; `auto` resolves against `MemTotal` and takes 2/3 of the per-drive auto budget. |
| `memory_buffers.read_gb_per_tape` | `auto` | Per-tape read-buffer size. Same shape; under `auto`, takes 1/3 of the per-drive auto budget. |
| `memory_buffers.read_prefetch_chunks_ahead` | `2` | Chunks prefetched ahead during sequential reads (0 disables; 1–3 typical). |
| `memory_buffers.auto_host_fraction_pct` | `50` | Fraction of `MemTotal` budgeted across all memory_buffers under `auto`. Range 1–100. |
| `memory_buffers.safety_max_host_fraction_pct` | `75` | Fraction of `MemTotal` the resolved total footprint must not exceed. Applies to both auto and explicit values; daemon refuses to start if exceeded. Range 1–100. |
| `memory_buffers.auto_min_gb_per_tape` | `1` | Floor (GB) for the per-tape auto-resolved value. Ignored for explicit GB. |
| `memory_buffers.auto_max_gb_per_tape` | `32` | Ceiling (GB) for the per-tape auto-resolved value. Ignored for explicit GB. |

### `disk_cache`

The disk cache is the content-addressed chunk-pool budget on disk at
`<data_dir>/chunks/`, enforced per storage backend. When a chunk seal would
push a backend's pool over its cap, or drop free filesystem space below
`disk_free_min_gb`, the seal blocks on the upload and eviction worker.
This backpressure gate is what prevents an overfull disk from turning
into a SCSI MEDIUM ERROR — the daemon parks the write until there is room
rather than failing it.

| Key | Default | Description |
|---|---|---|
| `disk_cache.size_gb` | `auto` | Per-backend budget. Integer (GB) pins the cap; `auto` re-derives `min(50% of free, max_size_gb)` floored at `min_size_gb` each eviction tick. A backend entry may override via its own `disk_cache_size_gb`. |
| `disk_cache.min_size_gb` | `4` | Floor when `size_gb: auto`. Ignored for explicit GB caps. |
| `disk_cache.max_size_gb` | `500` | Ceiling when `size_gb: auto`. Bounds eviction-scan cost on huge filesystems. |
| `disk_cache.localonly_soft_watermark_pct` | `80` | Soft watermark (% of `size_gb`); crossing it logs a warning. |
| `disk_cache.disk_free_min_gb` | `5` | Reserve free filesystem space (GB) below which seals also backpressure regardless of pool occupancy. `0` disables. |
| `disk_cache.recent_seal_pin_seconds` | `0` | Pin chunks touched within this many seconds against LRU eviction (counters verify-after-write churn). `0` = pure LRU. Default may change before RC/GA. |
| `disk_cache.ghost_ring_size` | `100000` | Per-backend bounded ring of recently-evicted chunk hashes (~100 B/entry → ~10 MB/backend at the default). On every cache miss the chunk hash is looked up in the ring; if found, `now - evicted_at` is bucketed into the `cache_miss_after_eviction_seconds` histogram so operators can see whether their cache is undersized. Measurement-only — never affects cache replacement. `0` disables. |
| `disk_cache.backpressure_max_wait_seconds` | `30` | Max seconds a page-seal parks before surfacing SBC-3 NOT READY + ASC/ASCQ 0x04/0x07. **VSA only.** |
| `disk_cache.eviction_interval_seconds` | `300` | How often the eviction worker re-scans and trims each backend's pool. **VSA only.** |

### `drive` — VTL only

This section configures the emulated tape drive's per-block compression,
which runs before encryption. The compression enable (DCE) bit starts OFF
at every cartridge load; the host may flip it on per session via MODE
SELECT page 0x0F. This YAML section only determines *which* algorithm is
used when the host does enable it — not whether it is enabled.

| Key | Default | Description |
|---|---|---|
| `drive.compression.algorithm` | `lz4` | `lz4`, `zstd`, or `sldc` (reserved — selecting it errors). Recorded per-block so the knob can change without breaking old reads. |
| `drive.compression.zstd_level` | `3` | Zstd level 1–22. Used only when `algorithm: zstd`. |

### `storage`

This section holds workspace-wide storage tuning parameters plus the named
backend definitions. Each cartridge or volume binds to one named backend
at create time, and that binding is permanent. Per-backend `auth:` block
shapes are in [`AUTH.md`](AUTH.md).

| Key | Default | Description |
|---|---|---|
| `storage.skip_retention_mode_check` | `false` | Skip bucket-immutability validation at startup / `storage check`. `retention_mode` still parses and still gates `--worm`. Use when the principal can't be granted management-plane IAM. |
| `storage.compression.algorithm` | `zstd` | Backend-tier compression (post-dedup, per-chunk on upload): `none` / `lz4` / `zstd`. S3/GCS/Azure only. |
| `storage.compression.level` | `3` | Zstd level 1–22. Ignored for `lz4` / `none`. |
| `storage.upload.max_concurrent` | `0` | In-flight uploads per backend. `0` = auto-scale to `min(16, parallelism × 4)`. |
| `storage.upload.retry_max_attempts` | `10` | Retries per upload (exponential backoff 1 s → 30 s). |
| `storage.upload.backpressure_max_wait_seconds` | `60` | Max seconds a chunk-seal blocks on the per-backend pool budget before surfacing SCSI NOT READY. Range 1–600. Present on both products. |
| `storage.backends` | empty | Named backend map — see below. |

`storage.backends` is a map whose keys — `primary`, `cold-archive`, and so
on — are the names that cartridges and volumes bind to at create time. The
`type:` field inside each entry discriminates `local` / `s3` / `gcs` /
`azure`. An empty map is valid and the daemon boots fine; a cartridge or
volume create that references an undefined backend fails at operation time.
Per-type fields (`bucket`, `region`, `prefix`, `project_id`,
`storage_account`, `container`, `root_dir`, `retention_mode`,
`disk_cache_size_gb`, `endpoint_url`) and the `auth:` block are
documented in [`AUTH.md`](AUTH.md); the S3-compatible provider matrix
is in [`S3_BACKENDS.md`](S3_BACKENDS.md).

### `http`

The HTTP listener serves the health, metrics, and status endpoints.

| Key | Default | Description |
|---|---|---|
| `http.listen` | `0.0.0.0:9090` | HTTP server listen address. |
| `http.tls.cert_file` | `""` | PEM server cert chain. Empty → plaintext HTTP. If `cert_file` + `key_file` are set but the files are missing, the daemon auto-generates a self-signed pair on first boot. |
| `http.tls.key_file` | `""` | PKCS#8 private key matching `cert_file`. Required when `cert_file` is set. |
| `http.tls.client_ca_file` | `""` | PEM CA bundle. When set, the listener requires a client cert signed by it (mTLS). |
| `http.tls.extra_sans` | `[]` | Extra SANs (DNS names or IPs) baked into the auto-generated self-signed cert, beyond the built-in hostname / `localhost` / `127.0.0.1` / `::1` set. Ignored when a CA-issued cert is supplied. Editing it takes effect on the next `system regenerate-cert`. |

### `telemetry`

The `telemetry` section controls the OpenTelemetry plumbing. Prometheus
pull is always wired at `GET /metrics` on the `http:` listener and
requires no configuration here. The `telemetry.otlp` block adds an opt-in
OTLP push exporter on top of that. This section is present in
`thurvtl.yaml`; `thurvsa.yaml`'s reference exposes metrics via the `http:`
listener only and carries no `telemetry:` block. Full design:
[`TELEMETRY.md`](TELEMETRY.md).

| Key | Default | Description |
|---|---|---|
| `telemetry.otlp.enabled` | unset | Set `true` to push; `false` keeps the block but disables export. |
| `telemetry.otlp.endpoint` | `http://localhost:4317` | Collector / SaaS endpoint. gRPC `:4317`, HTTP `:4318/v1/metrics`. |
| `telemetry.otlp.protocol` | `grpc` | `grpc` or `http` (alias `http_protobuf`). |
| `telemetry.otlp.interval_seconds` | `30` | Push cadence. |
| `telemetry.otlp.headers` | unset | Per-request headers (e.g. bearer token / API key). |

### `audit`

The audit section configures the append-only, BLAKE3-chained event journal
at `<data_dir>/audit/`, which rolls daily at UTC midnight. Full design:
[`AUDIT.md`](AUDIT.md).

| Key | Default | Description |
|---|---|---|
| `audit.enabled` | `true` | **VSA only.** Disable only for ephemeral dev runs. VTL auditing is always on and has no `enabled` key. |
| `audit.dir` | `<data_dir>/audit` | Audit directory override. |
| `audit.compress_rotated` | `true` | **VTL only.** zstd-compress rotated daily files. |
| `audit.retention_days` | `90` | **VTL only.** Days of rotated history kept locally before pruning. Must be ≥ 40. |

### `keystore`

The keystore section configures the pluggable DEK keystore, which backs
at-rest encryption for VSA volumes and VTL cartridges. Encryption is opted
into per volume or per cartridge at create time, not in the YAML conffile —
the YAML just makes the named backends available. Per-provider `auth:`
schema: [`AUTH.md`](AUTH.md) § keystore backends.

| Key | Default | Description |
|---|---|---|
| `keystore.backends` | empty | Named keystore-backend map. Each entry's `type:` is `local` / `awskms` / `vault` / `azurekv` / `gcpkms` / `kmip`. At-rest encryption is opt-in per volume/cartridge via `--encrypt --keystore NAME` at create time. |

### `alerting`

The alerting section configures first-party email and webhook alerting,
which is off by default. Full design and worked examples:
[`ALERTING.md`](ALERTING.md).

| Key | Default | Description |
|---|---|---|
| `alerting.enabled` | `false` | Master switch. `true` requires a non-empty `sinks`. |
| `alerting.dedup_window_seconds` | `300` | Window within which repeats of the same `(class, dedup_key)` collapse to one. |
| `alerting.chap_failures_threshold` | `3` | CHAP-failure alerts fire after this many failures from one user in a window. `0` disables. |
| `alerting.events.backend_reachability` | `false` | Per-class on/off — storage-backend reachability. |
| `alerting.events.audit_failure` | `true` | Per-class on/off — audit-log append failures. |
| `alerting.events.disk_cache_backpressure` | `false` | Per-class on/off — disk-cache watermark / backpressure timeout. |
| `alerting.events.chap_failures` | `true` | Per-class on/off — repeated CHAP login failures. |
| `alerting.sinks` | `[]` | Sink list. Each sink is `type: email` (SMTP) or `type: webhook` (Tera-templated HTTP POST). Sink fields and worked examples are in [`ALERTING.md`](ALERTING.md). |

---

## Daemon-managed state files

The JSON files under `<data_dir>/` are mutated exclusively through the
CLI — they must never be hand-edited. The daemon re-reads the credential
files (iSCSI users, NVMe/TCP PSKs) on every login, so a credential change
takes effect immediately without a restart.

### `library.json` — VTL

`library.json` holds the chassis topology in two stanzas: `declared`
mirrors the YAML `library:` block at the last successful reconcile
(`num_storage_slots`, `num_drives`, `lto_generation`, optional
`firmware`); `minted` is daemon-owned and set once at first
materialization (`chassis_serial`, the four SMC element-address
bases) — immutable for the life of the library. Plus the partition
layout. The daemon materializes the file on first start from the
YAML, and on every subsequent start diffs the YAML against
`declared` and reconciles (grow always succeeds; shrink refuses if
any cartridge would be orphaned). Partition layout is the only piece
operators still mutate imperatively, via
`thurvtl library partition {create,modify,…}` (daemon-down). Full
schema: [`SPEC.md`](SPEC.md) § Library Topology.

### `inventory.json` — VTL

`inventory.json` holds the per-slot and per-drive cartridge inventory:
which barcode sits where, each cartridge's home slot, and drive-load
state. The daemon updates it on changer moves, and `thurvtl changer`
operations update it from the CLI. Schema alongside `library.json` in
[`SPEC.md`](SPEC.md).

### `iscsi-users.json` — both

`iscsi-users.json` holds the CHAP user list and the singleton mutual-CHAP
target credential. VTL users may carry a `partition:` binding for
partition-fenced sessions; VSA ignores that field. The file is managed by
`<product> iscsi users {add,remove,disable,enable,rotate,list}` and
`iscsi target {set,clear,show}` — see [`CLI.md`](CLI.md). The YAML
conffile carries only `iscsi.auth.method` and
`iscsi.auth.allowed_algorithms`; the credentials themselves live here.

### `nvmetcp-psks.json` — VSA

`nvmetcp-psks.json` holds the TLS-PSK host-identity list for NVMe/TCP.
It is managed by `thurvsa nvmetcp psks
{add,remove,disable,enable,rotate,list}` and is re-read on every TLS
handshake. PSK generation and wiring: [`AUTH.md`](AUTH.md) §
NVMe/TCP TLS-PSK and [`NVMETCP.md`](NVMETCP.md).

---

## The daemon environment file

`/etc/thurvtl/thurvtl.env` and `/etc/thurvsa/thurvsa.env` are optional
`KEY=VALUE` files that the packaged systemd unit loads via
`EnvironmentFile=-`. They are the on-prem path for supplying storage
credentials through environment variables — the variable names that a
backend's `auth:` block references are set here. The daemon picks up
changes on restart. The full recipe — variable names per provider, file
permissions, and the interaction with each provider's default-credential
chain — is in [`AUTH.md`](AUTH.md).
