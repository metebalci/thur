# Troubleshooting

Symptom-first. Start with the daemon's own logs — almost every failure
names its cause and the remediation there:

```bash
sudo journalctl -u thurvtld -f        # or thurvsad
thurvtl system daemon-health          # daemon-routed health summary
```

## The daemon won't start

The daemon validates its world at boot and **refuses to start** rather
than come up half-broken. Common causes:

- **Backend credentials / bucket / permissions.** On startup it checks
  each `storage.backends:` entry's credentials, bucket existence, and
  read/write/delete permission. A failure aborts the boot and names the
  backend. Validate ahead of time without starting:
  `thurvtl system storage check`. Credential resolution is in
  [`AUTH.md`](AUTH.md).
- **`retention_mode` mismatch.** If a backend declares
  `retention_mode: governance|compliance` but the bucket's actual lock
  state differs (in either direction), the daemon refuses. Fix the bucket
  config or the YAML so they agree.
- **Library shrink would orphan data** (VTL). On restart the daemon
  reconciles the YAML `library:` block against the persisted
  `library.json`. Grow always succeeds; a shrink that would orphan a
  cartridge or a loaded drive is refused. Run `thurvtl library bounds`
  (daemon up) to see the safe-shrink envelope, or raise the counts back.
- **Missing required key.** `data_dir` is required for both products; VTL
  also requires the `library:` block.

## A CLI command refuses

Each command has exactly one daemon-mode — daemon-up, daemon-down, or
pure-local — and refuses in the wrong state (it never silently falls
back). See [`CLI.md`](CLI.md) § daemon-mode.

- **"start the daemon" / socket unreachable** — a daemon-routed command
  (most of them) needs the daemon running and the socket reachable. Check
  `systemctl status`, and that you are in the product group (so you can
  read `/run/<product>/admin.sock`, mode 0660) — see
  [`INSTALLATION.md`](INSTALLATION.md) § group membership.
- **"daemon is running, refusing"** — a daemon-down command (e.g.
  `library restore`, `volume key migrate`) refuses while the daemon is
  alive. Stop the daemon first.
- **"another instance holds the lock"** — the `<data_dir>/.daemon.lock`
  PID lockfile is held. Stale locks auto-clear; if it persists, confirm
  no daemon is running and remove it.

## A host can't see the devices

1. **Discovery / login succeeded?** Re-run discovery and `--login`; check
   `iscsiadm -m session` / `nvme list`. Cheatsheet: [`ISCSIADM.md`](ISCSIADM.md).
2. **Authentication.** Under CHAP / TLS-PSK a bad secret fails the login;
   repeated failures raise a CHAP-failure alert. Check the credentials
   and that the initiator is configured with them.
3. **Admission (VSA).** With CHAP / TLS-PSK on, a session sees **only**
   the volumes granted to its identity. A missing grant = see-nothing.
   `thurvsa iscsi users grant USER --volume NAME` /
   `nvmetcp psks grant`. With auth off, sessions see everything.
4. **New / cloned LUN not appearing.** Rescan the live session —
   `iscsiadm -m session --rescan` or `nvme ns-rescan /dev/nvme0`. No
   relogin needed.
5. **Stale partition table after recreate.** The kernel caches the old
   table; force a re-read: `sudo blockdev --rereadpt /dev/sdb`.

Connection detail and the advertised-address gotcha (containers / NAT)
are in [`CONNECTING.md`](CONNECTING.md).

## Writes stall or return errors

- **SCSI NOT READY (0x04/0x07) / NVMe namespace-not-ready.** This is
  backpressure: host writes are outpacing backend uploads, the disk-cache
  budget filled, and the daemon parked the write past its deadline. The
  lever is **backend bandwidth** — it must keep up with the sustained
  host write rate. Watch the backpressure-wait and cache-utilization
  metrics; raise `disk_cache` headroom only buys time, not throughput.
  See [`../reference/BACKPRESSURE.md`](../reference/BACKPRESSURE.md).
- **Disk cache consistently near full.** Either uploads aren't draining
  (check backend reachability / throttling) or the budget is too small
  for the working set. `system stats` shows the picture.
- **Permanent backend errors in logs** (Auth / Authz / NotFound /
  RegionMismatch). The retry layer short-circuits these — any non-zero
  rate means broken credentials or config drift, not a transient blip.

## Storage credential errors (cheatsheet)

| Symptom | Cause / fix |
|---|---|
| `403 Forbidden` on first write | Valid creds, missing `s3:PutObject` (or equivalent). Check the IAM policy. |
| `InvalidAccessKeyId` on MinIO/Wasabi after setting AWS creds | The process-global chain is winning. Use a per-backend `auth:` block. |
| `SAS URL is set … AAD env vars are being ignored` (Azure) | Stale `AZURE_*` env vars. Remove them or move to per-backend `auth:`. |
| GCS `Bearer token has expired` after long uptime | ADC token not refreshing. Use `service_account_key_file` (file creds refresh). |

Full credential model: [`AUTH.md`](AUTH.md).

## Monitoring and alerts

Wire Prometheus / OTLP and the email/webhook alerts so problems surface
before a host notices: [`TELEMETRY.md`](TELEMETRY.md) (incl. PromQL alert
rules) and [`ALERTING.md`](ALERTING.md). Verify the audit chain with
`system audit verify`.

## Disaster recovery

Bringing a host up from a cold mirror, or moving cartridges between
backends, is in [`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md).
