# Audit Log

The audit log is an append-only journal of every state-changing operation the
daemon performs. Each entry is hash-chained to the one before it, so
post-hoc tampering — removing an entry, altering a field, reordering records
— produces a detectable break in the chain. The wire and file format
(filenames, JSONL schema, `prev_hash`/`entry_hash` calculation, CLI exit
codes) is specified in [`SPEC.md`](SPEC.md) § Audit Log. This document
covers how the daemon implements that design and the operational workflow for
administrators.

## Files

- `<data_dir>/audit/audit-YYYY-MM-DD.jsonl` — daily-rotated journal.
- `<data_dir>/audit/chain.state` — caches `(last_seq, last_hash,
  last_file)` for O(1) startup tail-verify.
- `<data_dir>/audit/pending/<sortable>.json` — CLI daemon-down
  queue. `library partition *` and `library restore` write a
  `PendingAuditEntry` here; the daemon drains it on startup via
  `AuditLog::replay_pending()`, appending each into the live chain
  in filename (≈ submission timestamp) order. (Chassis topology
  changes are no longer queued — the daemon's reconcile engine
  emits `library.materialize` / `library.reconcile` /
  `inventory.move_medium` rows directly during start-up.)
- `<data_dir>/audit/pending/failed/` — quarantine for queued
  entries the daemon couldn't append (malformed JSON, chain broken).
  Replay continues with the rest; operator inspects out of band.

The audit chain operates under a single-writer model: once the daemon is
running, it owns the chain exclusively. The `system audit` CLI subcommand
(`tail` / `export` / `verify` / `rotate`) routes all requests through the
daemon's admin job stream rather than touching `chain.state` or
`audit-*.jsonl` directly. This means no cross-process file locking is
needed.

### Producer / writer split

Only **one** task calls `AuditLog::append` directly: a dedicated
writer launched via `thur_core::spawn_audit_writer` after
startup-time sync writes (`replay_pending`, `daemon.start`) complete.
Every other emitter — iSCSI handlers, admin endpoints, the
rate-limit flush, gc — holds a cheaply-cloned `AuditChannel` and
pushes via non-blocking `try_append`.

The writer drains a bounded mpsc (1024 entries; `AUDIT_CHANNEL_CAPACITY`)
FIFO into the chain, so producers never contend on the chain mutex. From
the perspective of a SCSI WRITE handler, appending an audit entry is a
sub-microsecond `try_send` — it never blocks waiting for disk I/O.

When the channel is full (1024 entries in flight and the writer behind
on disk), the excess pushes are dropped. These drops are counted as
`<product>_audit_queue_drops_total` and warn-logged. This is a
deliberate policy: dropping one audit entry is preferable to stalling a
SCSI WRITE command. Shutdown guarantees that everything queued up to and
including `daemon.stop` is flushed, via a `Shutdown(oneshot)` sentinel.

Code: `shared/audit/src/audit.rs` (chain core),
`shared/audit/src/audit_channel.rs` (producer / writer task).
Daemon hookup in each daemon's `main.rs`. The `system.audit.*` job
handlers are cross-product in `shared/admin-audit`; the `system audit`
CLI subcommand is cross-product in `shared/cli-system/src/audit.rs`.
The daemon-down audit-queue helper (used by `library partition *` and
`library restore`) stays VTL-only in `vtl/cli/src/audit_helper.rs`.

## Tamper-evidence

The chained mode is the only mode — there is no opt-out.

Each entry's hash is computed as `blake3(canonical JSON of entry minus
entry_hash)`. The next entry's `prev_hash` equals that
value, forming the chain. The `chain.state` cache makes daemon-startup
tail-verification O(1); the daemon refuses to start if the tail hash
mismatches.

When an operator needs to recover from a genuine break, they run
`thurvtl system audit rotate --accept-break`. This writes an
`audit.chain_reset` entry whose `prev_hash` is a sentinel of the form
`blake3:reset:<old_hash>`, so the discontinuity remains permanently
visible in the log and cannot be silently elided. `params.trigger`
carries `"break_recovery"`.

The `AuditMode` enum slot is preserved (single variant `TamperEvident`).

## Rotation

The log rotates daily at UTC midnight. Compression of rotated files is
available via `audit.compress_rotated: true` (default).

## Retention

`audit.retention_days` defaults to **90**. There is no `enabled`
knob — audit is unconditionally on.

## CLI surfaces

Both `thurvtl` and `thurvsa` expose the identical `system audit` verb
set — the CLI and the daemon-routed job handlers both live in shared
crates, so the two products cannot drift.

- `system audit tail [-f]`
- `system audit export --format jsonl|csv [--from] [--to]`
- `system audit verify` — daemon-routed; exit 0 valid, 1 break, 2 io
- `system audit verify-offline --dir PATH [--json]` — no daemon
  required. Walks every JSONL entry under `dir`, recomputing the
  BLAKE3 chain. Exit 0 valid, 1 chain break, 2 IO/parse error. Use
  after the audit directory is copied off-host (cold backup).
- `system audit rotate --accept-break` — operator-acknowledged
  chain reset.

## What gets logged

Cartridge create/import/export, daemon-side chassis bring-up
(`library.materialize` on first start, `library.reconcile` on
subsequent YAML diffs, `inventory.move_medium` per auto-evacuated
drive), load/unload/move, gc, daemon start/stop, boot-time
orphan-upload recovery (`storage.orphan_scan_started` /
`storage.orphan_scan_completed`). **Read paths are NOT logged.**

### SCSI-layer events

Wired through the iSCSI server's `Option<AuditChannel>`:

- `iscsi.move_medium` — SCSI MOVE MEDIUM (drive load/unload by
  backup software), with src/dst element addrs and the unloaded
  barcode.
- `iscsi.encryption.set_key` / `iscsi.encryption.clear_key` —
  SECURITY PROTOCOL OUT page 0x0010. Metadata only — key bytes are
  never logged.
- `iscsi.drive_compression` — MODE SELECT page 0x0F DCE bit.
- `iscsi.chap.success` / `iscsi.chap.failure` — CHAP auth, with
  failure reason (`invalid_response`, `verify_error`,
  `skipped_security_stage`).

Every iSCSI entry's actor is `kind:"iscsi"` with the initiator's
IQN as `user` and the peer ip:port as `addr`. Audit-append failures
on the SCSI path are logged and swallowed — never tear down the
session.

### Rate-limited failure paths

A misbehaving initiator — one presenting a wrong CHAP secret or
sending a broken PREVENT/ALLOW sequence — can generate the same
failure event on every retry, flooding the chain with near-duplicate
entries. A small allowlist of host-driven failure operations is
rate-limited in `vtl/daemon/src/iscsi/protocol.rs::audit_append`:

| Op | Bucket key |
|---|---|
| `iscsi.chap.failure` | `(op, peer, chap_user, reason)` |
| `iscsi.move_medium` (only when `params.refused` set) | `(op, peer, refused_reason)` |

The window is 60 seconds. The first event in a window is appended
normally; subsequent events in that window are counted rather than
written. When the window expires, a single rollup entry — same op,
`result:"error"`, `params:{suppressed_count, window_seconds, key}`
— is appended by the flush task running on a 10-second cadence. Daemon
shutdown drains all in-flight windows before writing `daemon.stop`.

Lifecycle and one-shot events (`cartridge.create`, `daemon.start`,
`gc.run`, drive load/unload Ok paths, CHAP success,
encryption set/clear, drive-compression toggle) bypass the limiter.
The policy lives in `ratelimit_key_for(op, actor, params)` — the
single source of truth for which operations opt in; adding a new
flood-prone path requires only one additional match arm.

Implementation: `shared/audit/src/audit_ratelimit.rs`
(`AuditRateLimiter`, fail-open on mutex poison so a poisoned limiter
biases toward chain noise, not chain blindness).
