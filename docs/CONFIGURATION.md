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
| `nvmetcp-dhchap.json` | `<data_dir>/` | VSA | JSON | `thurvsa nvmetcp dhchap` |
| `admin-password.json` | `<data_dir>/` | both | JSON | daemon — Argon2id hash of the web-admin password (set via `<product> system set-admin-password`). Holds only the hash, never the plaintext; absent = no password configured. Enforced only when `http.auth.method: Password`, in which case absent fails the listener closed; the default `http.auth.method: None` serves the protected routes open. |
| `reservations.json` | `<data_dir>/` | both | JSON | daemon — persisted SCSI/NVMe PERSISTENT RESERVE state (PTPL); written on every APTPL/CPTPL-set reservation change, reloaded at start. No CLI verb; never hand-edited (a corrupt file is ignored and the daemon starts with empty reservation state). |

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

### `transports` — VSA only

| Key | Default | Description |
|---|---|---|
| `transports` | `[iscsi]` | Host-facing data path(s). A list (or a bare scalar) of `iscsi` (SCSI-over-TCP listener) and/or `nvmetcp` (NVMe/TCP listener). List both (`[iscsi, nvmetcp]`) to bind both concurrently against the shared volume set + reservation state — a volume is then a SCSI LUN and an NVMe namespace (`nsid = lun + 1`) at once (issue #66). The default ports don't clash (3260 vs 4420). An empty list is rejected; duplicates are de-duped. A host reachable over both transports must be admitted on **both** (`iscsi-users.json` *and* `nvmetcp-psks.json`); the two admission fences are independent. A reservation taken over one transport fences the other (enforcement + pull reports are coherent; proactive cross-transport notification is out of scope). |

### `iscsi`

| Key | Default | Description |
|---|---|---|
| `iscsi.listen` | `0.0.0.0:3260` | iSCSI target listen portal(s). Accepts a single `"ip:port"` scalar, a list of bare `"ip:port"` strings, or a list of `{bind, advertise?, tpgt?}` objects — each entry binds its own listener (`bind`) and SendTargets advertises every entry as `TargetAddress=<advertise\|bind>,<tpgt>`, enabling multi-portal path redundancy without MC/S. Bare-string entries (and objects omitting `tpgt`) auto-assign sequential Target Portal Group Tags by input position (1, 2, …). Multiple portals sharing one TPGT (a group) is legal and is the prerequisite shape ALUA Target Port Groups will plug into; the same `bind` listed twice is rejected. The Login Response `TargetPortalGroupTag` echoes the arrival portal's TPGT (RFC 7143 §12.10). With no `advertise`, a wildcard `bind` (`0.0.0.0:*`, `[::]:*`) is substituted with the connection's actual local IP; set `advertise` (a full `ip:port`, emitted verbatim) when the bind isn't reachable by initiators — NAT, Docker bridge + published ports, reverse proxy, multi-homed host. A wildcard `advertise` is rejected at startup. |
| `iscsi.target_iqn` | `iqn.2025-10.com.metebalci:thurvtl` / `:thurvsa` | Target IQN advertised to initiators. |
| `iscsi.reservations.initiator_port` | `iqn-isid` | Which initiator-port identity SCSI-3 persistent reservations key by. `iqn-isid` (default): the full, spec-literal iSCSI port (initiator IQN + ISID) — models per-path (`mpathpersist`-style) registration; a host reclaims a reservation across a reconnect only if it reuses its ISID (Windows / VMware / session reinstatement do). `iqn`: key by IQN alone (ISID ignored) — a host reclaims across any reconnect / target restart even if its ISID changes (open-iscsi mints a fresh ISID per login), at the cost of collapsing all of a host's concurrent sessions to one registrant. NVMe/TCP is unaffected (keys by the host-stable HOSTID). See [`docs/SPEC.md`](SPEC.md) § Persistent reservations. |
| `iscsi.max_sessions` | `10` | Max concurrent iSCSI sessions. **VTL only.** |
| `iscsi.session_timeout_seconds` | `300` | Per-session inactivity timeout. **VTL only.** |
| `iscsi.auth.method` | `None` | `None` (unauthenticated) or `CHAP`. |
| `iscsi.auth.allowed_algorithms` | `[SHA3-256, SHA-256, SHA-1, MD5]` | Allowed CHAP digests, strongest first. Empty list falls back to all four. |

CHAP users and the mutual-CHAP target credential live in
`iscsi-users.json`, not in the YAML — see *Daemon-managed state files*.

### `nvmetcp` — VSA only

This section is consulted only when `nvmetcp` is listed in `transports`.

| Key | Default | Description |
|---|---|---|
| `nvmetcp.listen` | `0.0.0.0:4420` | NVMe/TCP listen address (IANA nvme-tcp port). |
| `nvmetcp.advertise` | _(unset)_ | Address advertised to hosts in the Discovery Log Page (TRADDR + TRSVCID), as a full `ip:port`. Defaults to the `listen` address — and with a wildcard bind the Discovery controller reflects the address each request landed on. Set this (emitted verbatim) when the bind isn't reachable by hosts — NAT, Docker bridge + published ports, reverse proxy, multi-homed host. Only affects discovery (`nvme discover` / `connect-all`); a direct `nvme connect` to a known address ignores it. A wildcard `advertise` is rejected at startup. |
| `nvmetcp.subnqn` | `nqn.2025-10.com.metebalci:thurvsa` | NVMe subsystem NQN. The TLS-PSK derivation binds to this — changing it rederives every per-host PSK. |
| `nvmetcp.tls.mode` | `disabled` | `disabled` (cleartext) or `psk` (TLS 1.3 with the two NVMe-TCP mandated cipher suites). |
| `nvmetcp.tls.identity_file` | `<data_dir>/nvmetcp-psks.json` | Path to the TLS-PSK host-identity file. |
| `nvmetcp.auth.mode` | `none` | `none` or `dhchap` (DH-HMAC-CHAP in-band host auth, NVMe Base §8.13). Orthogonal to `tls.mode`: `dhchap` + `psk` = "dhchap+tls". |
| `nvmetcp.auth.identity_file` | `<data_dir>/nvmetcp-dhchap.json` | Path to the DH-HMAC-CHAP host-secret file. |
| `nvmetcp.discovery.enabled` | `true` | Bind a Discovery controller (well-known NQN `nqn.2014-08.org.nvmexpress.discovery`) so `nvme discover` / `nvme connect-all` work. Default on whenever `nvmetcp` is enabled; set `false` to drop the listener. The listener is always cleartext + unauthenticated (the NVMe analog of iSCSI SendTargets); the Discovery Log record advertises whether the I/O subsystem requires TLS (tracks `tls.mode`). Volume names do not leak — admission stays at the I/O Connect. |
| `nvmetcp.discovery.listen` | `0.0.0.0:8009` | Discovery listen address (IANA-registered NVMe discovery port; no clash with the I/O listener on 4420 or iSCSI on 3260). |

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
| `disk_cache.recent_seal_pin_seconds` | `0` | Pin chunks touched within this many seconds against LRU eviction (counters verify-after-write churn). `0` = pure LRU. Tune empirically from the `cache_miss_after_eviction_seconds` histogram (see `ghost_ring_size`). |
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
| `storage.check_interval_seconds` | `0` | Periodic backend-reachability ticker interval. `0` = off (reachability only checked on `system storage check`). When set, each daemon probes every backend on this interval (small list/write/read/delete per backend) and fires `backend_reachability` failure/recovery. Set conservatively (300+); each tick does real backend I/O. Both products. |
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

### `tiering` — VTL only

A cartridge binds to one backend at create time. Tiering is the
operator-driven way to re-home it later under a rule instead of by
hand: a list of placement policies, evaluated against the live
inventory. The section is off by default (no policies), and nothing
ever moves on its own. The workflow is three daemon-routed verbs:
`thurvtl system tiering plan` previews the migrations the policies
would trigger (read-only); `system tiering run-now` applies a freshly
re-evaluated plan, migrating each matching cartridge through the same
primitive as `cartridge migrate` and auditing each as
`cartridge.tiered`; `system tiering status` prints a one-line summary
(policy count + pending moves).

| Key | Default | Meaning |
|-----|---------|---------|
| `tiering.policies` | empty | Ordered list of placement policies. The first policy whose predicates all match a cartridge decides its placement. |
| `tiering.policies[].predicates.barcode_prefix` | unset | Match cartridges whose barcode starts with this string. |
| `tiering.policies[].predicates.lto_generation` | unset | Match cartridges of exactly this LTO generation. |
| `tiering.policies[].predicates.worm` | unset | Match cartridges with this WORM flag (`true` / `false`). |
| `tiering.policies[].migrate_to` | **required** | Target backend name. Must exist under `storage.backends`. |

Predicates within a policy are ANDed; at least one predicate is
required (a zero-predicate "match everything" rule is rejected at
startup, as is a `migrate_to` that names an undefined backend — both
are reported with every offending policy index at once). Only the
three predicates above are supported: each is O(1) to evaluate and
survives a disaster-recovery restore. An age-based predicate is
deliberately omitted — the only "last write" signal available today
is a local-only index that is zero-filled on restore, so it cannot be
trusted across DR.

Cartridges under a cloud-native legal hold are always excluded from
tiering, with no per-policy opt-in: a hold has no host-visible signal
and no cross-backend transfer path, so relocating a held cartridge
would silently drop the hold. `plan` reads each move candidate's hold
state from its backend and reports held cartridges separately.

### `http`

The HTTP listener serves the health, metrics, and status endpoints,
plus the read-only Web UI when enabled.

| Key | Default | Description |
|---|---|---|
| `http.listen` | `0.0.0.0:9090` | HTTP server listen address. |
| `http.tls.cert_file` | `""` | PEM server cert chain. Empty → plaintext HTTP. If `cert_file` + `key_file` are set but the files are missing, the daemon auto-generates a self-signed pair on first boot. |
| `http.tls.key_file` | `""` | PKCS#8 private key matching `cert_file`. Required when `cert_file` is set. |
| `http.tls.client_ca_file` | `""` | PEM CA bundle. When set, the listener requires a client cert signed by it (mTLS). |
| `http.tls.extra_sans` | `[]` | Extra SANs (DNS names or IPs) baked into the auto-generated self-signed cert, beyond the built-in hostname / `localhost` / `127.0.0.1` / `::1` set. Ignored when a CA-issued cert is supplied. Editing it takes effect on the next `system regenerate-cert`. |
| `http.auth.method` | `None` | Whether the protected route group (`/sessions`, `/info`, `/ui`, read-only `/api/v1`) requires the shared web-admin password. `None` (default) serves them open — for an isolated / trusted network, the same posture `iscsi.auth.method` defaults to. `Password` requires the password (set via `system set-admin-password`) over HTTP Basic; no password configured then fails closed (503). `/health` + `/metrics` stay open regardless. Pair `Password` with `http.tls` so the secret isn't sent in clear. |
| `http.webui.enabled` | `true` | Serve the read-only Web UI (issue #5): the static console at `/ui/` plus the read-only `/api/v1` GET subset, both under the same gate as `/sessions` + `/info` (open by default; see `http.auth.method`). `false` keeps only `/health` `/metrics` `/sessions` `/info`. Mutations are never exposed. |
| `http.webui.asset_dir` | `""` | Directory to serve the `/ui` bundle from. Empty → the bundle embedded in the binary. Point it at the packaged `/usr/share/<product>/webui/` (or any directory) to restyle without a rebuild — edit the CSS custom-property tokens in `app.css`; a file missing from the directory falls back to the embedded copy. Path traversal out of the directory is rejected. |

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
| `alerting.events.orphaned_objects` | `true` | Per-class on/off — storage objects left behind by a failed best-effort delete (VTL `cartridge migrate` source-delete; orphaned until GC). |
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
partition-fenced sessions; VSA ignores that field. VSA users
**must** carry a `volumes:` field (a non-empty array of volume
names) — admission is mandatory: every CHAP session is fenced to
that subset. REPORT LUNS, INQUIRY, TUR, and READ CAPACITY against
non-admitted LUNs return peripheral-qualifier 0x3 (no LU here). VTL
ignores the `volumes` field. Sessions without CHAP
(`iscsi.auth.method: None`) skip admission and see every volume —
pairing admission with the auth layer mirrors NFS export-list
behaviour.

The file is managed by `<product> iscsi users
{add,remove,disable,enable,grant,revoke,rotate,list}` and
`iscsi target {set,clear,show}` — see [`CLI.md`](CLI.md). `add`
takes one or more `--volume NAME` (required for VSA); `grant` /
`revoke` mutate the list post-creation. The YAML conffile carries
only `iscsi.auth.method` and `iscsi.auth.allowed_algorithms`; the
credentials themselves live here.

### `nvmetcp-psks.json` — VSA

`nvmetcp-psks.json` holds the TLS-PSK host-identity list for NVMe/TCP,
and the per-host volume admission set. Each entry **must** carry a
`volumes:` field (a non-empty array of volume names) when TLS-PSK
is on — admission is mandatory: every TLS-authenticated host is
fenced to that subset. Identify CNS=0x02 (Active NS List), CNS=0x00
(Namespace), and per-NSID I/O against non-admitted namespaces
return Invalid Namespace. Plaintext mode (`nvmetcp.tls.mode:
Disabled`) skips admission entirely and connections see every
namespace — same shape as iSCSI no-CHAP.

Managed by `thurvsa nvmetcp psks
{add,remove,disable,enable,grant,revoke,rotate,list}`. The file is
re-read on every TLS handshake and once post-Connect for the
admission lookup, so operator edits take effect on the next *new*
connection without restart. PSK generation and wiring:
[`AUTH.md`](AUTH.md) § NVMe/TCP TLS-PSK and
[`NVMETCP.md`](NVMETCP.md).

### `nvmetcp-dhchap.json` — VSA

`nvmetcp-dhchap.json` holds the DH-HMAC-CHAP host-secret list for
NVMe/TCP in-band authentication (`nvmetcp.auth.mode: dhchap`), plus the
per-host volume admission set. Each entry carries a `dhchap_key` (the
host's `DHHC-1:...` secret from `nvme gen-dhchap-key`), an optional
`dhchap_ctrl_key` (a controller secret enabling bidirectional / mutual
auth), `disabled`, a mandatory non-empty `volumes` array, and the
rotation-grace pair `previous_dhchap_key` / `previous_expires_at`.
Admission works exactly like the TLS-PSK file: the authenticated host is
fenced to its `volumes`, and non-admitted namespaces return Invalid
Namespace. With `auth.mode: none` no in-band auth runs.

Managed by `thurvsa nvmetcp dhchap
{add,remove,disable,enable,grant,revoke,rotate,set-ctrl-key,clear-ctrl-key,list}`.
The file is re-read on every Connect, so operator edits take effect on
the next *new* connection without restart. Secret generation and wiring:
[`AUTH.md`](AUTH.md) § NVMe/TCP DH-HMAC-CHAP and
[`NVMETCP.md`](NVMETCP.md).

### `admin-password.json` — both

`admin-password.json` holds the single shared web-admin password that
gates the network-facing HTTP listener — the prerequisite for the Web
UI. It carries exactly two fields: `phc`, an Argon2id PHC string (the
self-describing `$argon2id$...` hash, OWASP-baseline parameters), and
`updated_at`, an RFC 3339 timestamp of the last change. Only the hash
is ever written; the plaintext is hashed server-side and never lands on
disk. The file is mode 0640, written by atomic rename, and owned by the
daemon — there is no packager-installed default and no postinst entry,
so an unprovisioned host simply has no file. An absent file means no
password is configured, and the gate **fails closed**: the protected
half of the HTTP listener (everything but `/health` and `/metrics`)
answers `503` with a challenge so operators can tell "unset" from
"wrong". Once set, the hash is hot-swapped into the live verifier and
takes effect immediately, with no restart.

The password is set with `<product> system set-admin-password`
(daemon-routed — the daemon owns the file). The verb prompts twice with
no echo, or reads the per-product environment variable
`THURVTL_ADMIN_PASSWORD` / `THURVSA_ADMIN_PASSWORD` for non-interactive
provisioning; the plaintext travels over the local peer-cred admin
socket and is hashed server-side. See [`CLI.md`](CLI.md).

This feature adds **no new YAML key** — the HTTP Basic realm and the
synthetic username are hardcoded constants, so the annotated YAML
reference is intentionally unchanged. Because Basic credentials are
base64, not encrypted, enable the admin HTTP TLS listener (`http.tls.*`)
so they are not sent in clear.

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
