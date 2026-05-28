# CLI Surface

This document covers how the two CLIs are structured, how they discover
the admin socket, how individual commands are classified by their required
daemon state, and how long-running operations are handled. Top-level
orientation is in [`../CLAUDE.md`](../CLAUDE.md) § CLI Surface.

## thurvtl

```
library    init / info / modify / monitor / self-test / restore / restore-archive / partition
           partition list / create / modify / delete
cartridge  create / archive / migrate / import / export / list / info / legal-hold / key
changer    inventory / move / load / unload [--force]
drive      status / self-test
system     gc / verify / stats / daemon-health
           audit {tail,export,verify,verify-offline,rotate} / storage {check,benchmark}
           regenerate-cert / alerting {list,test}
iscsi      users {add,remove,disable,enable,rotate,list} / target {set,clear,show}
config     defaults / systemd-unit / completion
```

The daemon is managed via systemd (`systemctl {start,stop,status,enable}
thurvtld`); the CLI has no daemon-lifecycle commands. For
development or debugging, the daemon binary can be run directly.
`thurvtl <subcommand> --help` is the source of truth for flags —
clap drives both `--help` and the shipped completion scripts.

## thurvsa

```
volume     create / list / info / destroy / modify / key migrate
system     storage benchmark / gc / stats / verify / regenerate-cert / alerting {list,test}
           daemon-health / audit {tail,export,verify,verify-offline,rotate}
iscsi      users {add,remove,disable,enable,rotate,list} / target {set,clear,show}
nvmetcp    psks {add,remove,disable,enable,rotate,list}
config     defaults / systemd-unit / completion
```

The `volume` commands (`create`, `list`, `info`, `destroy`, `modify`) are
daemon-routed only — they talk to `/run/thurvsa/admin.sock` and refuse
with a clear "start the daemon" message when the socket is unreachable.
`volume create` resolves `--backend` daemon-side: you may omit it when
exactly one `storage.backends:` entry exists. `system storage benchmark` is
daemon-down — it parses the YAML conffile's `storage.backends:` block,
constructs each named backend, and drives parallel upload, download, and
delete operations via `shared-object-store-bench`. `config` is pure-local.

The `iscsi` and `nvmetcp` nouns handle credential lifecycle and are
covered in detail in
[`#cred-lifecycle`](#chap--psk-credential-lifecycle) below.

## Design principle: one daemon-mode per command

Every CLI command has **exactly one** daemon-mode — daemon-up,
daemon-down, or pure-local — and that mode is a property of the command
itself, not a runtime decision based on whether a socket happens to
answer.

The reason this matters is that the two paths are not equivalent.
Daemon-routed writes are audited and serialized through the writer task.
Daemon-down writes use privdrop and mutate the file directly, emitting no
audit row. An operator looking at the audit log cannot tell which path was
taken if the command is allowed to try one and fall back to the other. The
mode is therefore fixed at design time. If the daemon's required state
does not match — the socket is down when it is needed, or up when it
should be down — the command refuses with a clear message rather than
silently proceeding via the wrong path.

## Daemon-required vs daemon-down (thurvtl)

Most CLI commands are live operations that route through the daemon's
admin Unix socket at the canonical path `/run/thurvtl/admin.sock` (mode
0660, peer-cred-authed). The systemd unit's `RuntimeDirectory=thurvtl`
directive in `release/thurvtld.service` provisions the parent
directory at boot (0750 root:thurvtl) and tears it down on stop.
Development runs may override the path via the `THURVTL_ADMIN_SOCKET`
env var. The canonical path is binding for both the daemon
(`admin::admin_socket_path`) and the CLI
(`AdminClient::auto_discover`) — there is no fallback to
`<data_dir>/admin.sock`, so daemon-routed commands work without reading
the 0640 conffile.

- **Daemon-down (partition layout + DR + storage benchmark + offline
  key/cert ops):** `library restore`, `library restore-archive`,
  `library partition {list,create,modify,delete}`,
  `cartridge key {migrate,show}`, `system storage benchmark`,
  `system regenerate-cert`.
  Each of these has a specific reason to require the daemon to be
  stopped. (Chassis topology — `num_slots` / `num_drives` /
  `lto_generation` — is YAML-declared and reconciled by the daemon at
  start; no imperative `library init` / `library modify` verb exists.
  `library bounds` is a daemon-routed read that surfaces the
  safe-shrink envelope before editing the YAML.)
  `library restore` discovers cartridges in a
  storage backend's `manifests/` prefix and seeds the local data directory
  for cross-region DR. `system storage benchmark` validates a bucket
  before the daemon starts. `system regenerate-cert` rewrites the admin
  HTTP self-signed cert and key in place; it refuses while the admin
  socket answers, and the new cert only takes effect after a restart.
  The `cartridge key *` verbs unwrap key material and rewrite manifests
  while the daemon is stopped, so the live path cannot pick up a
  half-rewritten file. These
  commands read `thurvtl.yaml` for `data_dir`; `main.rs` calls
  `read_minimal()` only when `Cli::is_daemon_down()` returns true.
  `library restore` and `system storage benchmark` additionally parse the
  `storage.backends:` block to resolve `--backend NAME`. Run these with
  `sudo` on a packaged install so that the 0640 conffile is readable and
  the operator has write access under `data_dir`. The privdrop in
  `vtl/cli/src/privdrop.rs` then drops privileges before any I/O — euid
  0 → `setgid` → `initgroups` → `setuid` to `--user`, defaulting to
  `thurvtl` — so `sudo` does not leave root-owned files behind. The
  cross-region DR runbook is in [`SPEC.md`](SPEC.md) § Cross-region DR.

- **Daemon-routed (live):** all cartridge ops except `key` (including
  `archive`, `migrate`, `import`, `export`, and `legal-hold`);
  `library restore-archive`; all changer ops (including `unload
  --force`); `drive status` and `drive self-test`; `library info`,
  `library monitor`, and `library self-test`; the live `system` ops
  (`gc`, `verify`, `stats`, `daemon-health`, `audit
  {tail,export,verify,rotate}`, `storage check`, and `alerting`);
  and all `iscsi` verbs. These commands never read
  `thurvtl.yaml` — the CLI connects to `/run/thurvtl/admin.sock` (or
  `$THURVTL_ADMIN_SOCKET`) directly. Membership in the `thurvtl` group
  is the only gate. `{library,drive} self-test` runs the same SPC-4
  diagnostic that the iSCSI SEND DIAGNOSTIC handler invokes and stamps
  the `DiagnosticStore` ring so that a subsequent host RECEIVE DIAGNOSTIC
  RESULTS sees the CLI-issued probe. These commands refuse with "is the
  daemon running?" if the socket is not reachable. (`system audit
  verify-offline` is the lone exception — it walks a copied audit
  directory and needs no daemon.)

- **Pure local (no data):** `config defaults / completion / systemd-unit`.

`drive rewind` and `drive position` are not CLI verbs — they are
host-issued SCSI operations, issued by backup software via `mt-st`
against the iSCSI device.

## Long-running ops — job protocol

The live `system` auditors, sweeps, and self-tests ride a two-step job
protocol on the admin socket rather than holding the HTTP connection open
for the entire operation:

1. `POST /api/v1/jobs/<kind>` registers and starts a worker, returning
   `{job_id, kind, started_at}`.
2. `GET /api/v1/jobs/{id}/events` streams NDJSON `JobEvent` lines
   (`log`, `progress`, `result`, `done`) until the terminal `Done`.

Job kinds (`vtl/daemon/src/admin/job_dispatch/mod.rs`): `system.gc`,
`system.verify`, `system.stats`, `system.cloud_check` (legacy name; CLI verb is `system storage check`),
`system.audit.{tail,export,verify,rotate}`,
`system.{library,drive}.self_test`, `system.alerting.test`,
`cartridge.{migrate,archive}`, `library.restore_archive`.
`thurvsad`'s `job_dispatch/mod.rs` mirrors the subset that applies to
block storage: `system.{gc,stats,verify}`,
`system.audit.{tail,export,verify,rotate}`, `system.alerting.test`.

The CLI client (`AdminClient::run_job`) renders log lines as they arrive
and exits with the daemon's reported code. The job plumbing itself lives
in `shared-admin-server` (`JobRegistry`, `JobEmitter`, `JobHandle`); the
per-kind dispatch closure lives in
`vtl/daemon/src/admin/job_dispatch/*.rs`. Finished jobs persist for
approximately 5 minutes (300 s retention TTL) so that a CLI reconnect
can replay the full transcript.

## Read-only auditors worth knowing

- **`system verify`** — walks the library and inventory, then every
  cartridge's `manifest.json`, `chunks.idx`, and `blocks-p<N>.idx`.
  It is the inverse of GC: rather than asking which chunks are alive, it
  asks whether the chunks the manifests claim to exist are actually
  present, with the right size and within-bounds block records. A storage
  sweep is on by default and can be skipped with `--skip-storage`; when
  active it HEADs every `CloudOnly` and `Both` chunk, every index-page
  object, and the `manifest-latest.json` sentinel. Implementation:
  `core/mediachanger/src/verify.rs`. On VSA the same verb walks each
  volume's `pages.idx` (header integrity + every referenced chunk
  present in the pool) and runs the same backend HEAD sweep; the local
  pool + storage sweeps are the shared `shared-verify-core`,
  implementation `core/block/src/verify.rs`.

- **`system stats`** — dedup analytics. Walks `chunks.idx`, groups
  chunks by `(backend, namespace)`, and reports logical bytes, unique
  pool bytes, dedup ratio, per-cartridge exclusive vs shared chunk
  counts, and a location breakdown. The backend HEAD-skip rate is exposed
  through Prometheus (`thurvtl_chunk_storage_head_*_total`) rather than
  being re-walked here, since it is a runtime signal rather than state
  that exists on disk. On VSA the same verb walks each volume's
  `pages.idx` instead, sizes chunks from the local pool, and omits the
  location breakdown (`pages.idx` records no local/backend tag); the
  dedup math is shared (`shared-dedup-stats`).

## GC

`system gc [--dry-run] [--storage]` walks every `manifest.json`, groups
`chunks[].hash` by `(backend, namespace)`, sweeps the local pool and
every local-scope namespace, and deletes anything not in the live set.
Orphan namespace directories are reclaimed in the same pass. `--storage`
extends the sweep to backend chunk keys and stale index-page objects (any
page whose index is >= `index_epoch[label].pages`). Cartridges with no
local manifest are skipped. Both products expose `system gc` with the
same flags.

## CHAP / PSK credential lifecycle

Both products' iSCSI CHAP file (`<data_dir>/iscsi-users.json`) and
VSA's NVMe-TCP PSK file (`<data_dir>/nvmetcp-psks.json`) are read fresh
on every iSCSI Login or TLS handshake. This means that operator edits
via the CLI verbs take effect on the next new session without requiring
a daemon restart or a SIGHUP.

```
thurvtl iscsi users  {list,add,remove,disable,enable,rotate}
thurvtl iscsi target {show,set,clear}
thurvsa iscsi users  {list,add,remove,disable,enable,rotate}
thurvsa iscsi target {show,set,clear}
thurvsa nvmetcp psks {list,add,remove,disable,enable,rotate}
```

Every verb is daemon-routed only — each edit travels through the
daemon's writer task, which serializes the file update and emits an
audit row atomically. When the admin socket is unreachable the verb
refuses with a clear "start the daemon" message rather than attempting
a direct file mutation.

**Schema additions (all `#[serde(default)]`, no version bump):**

- `disabled: bool` — entry kept but skipped at lookup time. Distinct
  from `remove` so it preserves audit-history continuity and can be
  re-enabled without re-sharing the credential.
- `previous_{password,interchange_key}` + `previous_expires_at` —
  rotation grace window. Both old and new authenticate while
  `previous_expires_at` is in the future; only the current after. No
  daemon-side timer; expiry evaporates at the next lookup.

**Verb shapes worth knowing:**

- `add NAME --password VALUE` / `--password-stdin` (mutex). The stdin
  variant reads one line, strips trailing CR/LF, never echoes. VSA
  **requires** at least one `--volume NAME` (repeatable) on every
  `add` — admission is mandatory. Names must currently resolve to a
  volume. VTL has no `--volume` flag (the analogous per-partition
  fence uses `--partition NAME`). Admission is captured at login,
  so volumes created after a session logs in remain invisible until
  re-login. The corresponding `thurvsa nvmetcp psks add --host-nqn
  N --key K --volume V [...]` verb carries the same mandatory
  admission for NVMe-TCP hosts (the join key is host NQN instead
  of CHAP user).
- `grant USER --volume V [...]` (VSA only) — adds volumes to the
  user's allow-list. Idempotent set-union. Refuses unknown volume
  names. Existing sessions don't see the change until re-login.
  Mirror: `nvmetcp psks grant --host-nqn N --volume V [...]`.
- `revoke USER --volume V [...]` (VSA only) — removes volumes from
  the user's allow-list. Refuses if it would leave the user with
  zero admitted volumes — use `remove` or `disable` for full
  cutoff. Existing sessions don't see the change until re-login.
  Mirror: `nvmetcp psks revoke --host-nqn N --volume V [...]`.
- `rotate NAME --password NEW [--grace 24h]` — sets new as current, old
  as `previous_password`, with `previous_expires_at = now + grace`.
  `--grace` accepts humantime durations. Refuses if a rotation is
  already in progress (`409 Conflict`).
- `rotate NAME --cancel` — drops the new credential, restores the
  previous as sole current. Errors if no rotation is in progress.
- `target set --username U --password V` (or `--password-stdin`) — the
  mutual-CHAP target identity. No grace-window rotation today.
- `target clear` — nulls both fields. Mutual-CHAP requests fail with
  "Target password not configured" afterward.

**Audit ops** (every verb is daemon-routed, so each one emits a row):

```
iscsi.users.{add, remove, disable, enable, grant, revoke,
             rotate.start, rotate.cancel, rotate.commit}
iscsi.target.{set, clear}
nvmetcp.psks.{add, remove, disable, enable, grant, revoke,
              rotate.start, rotate.cancel, rotate.commit}
```

`rotate.commit` fires once per swept entry whenever any mutating verb
observes an expired `previous_*` pair — so the grace-window cleanup is
operator-observable in the audit log without requiring a background sweep
task.
