# Production Readiness Checklist

Work through this before putting either application in front of real data.
Each item links to the doc with the detail. Thur is alpha software with
no stability guarantee — verify your backups independently regardless of
this list.

## Storage & durability

- [ ] **Backend is a real, durable object store** (S3 / GCS / Azure /
      S3-compatible), not `local`, unless `local` is itself on redundant,
      backed-up storage. The backend is the source of truth.
      [`CONFIGURATION.md`](CONFIGURATION.md), [`S3_BACKENDS.md`](S3_BACKENDS.md).
- [ ] **Credentials are scoped and resolvable.** Per-backend `auth:` (or
      a deliberate default chain), least-privilege IAM, verified at boot
      (`system storage check`). [`AUTH.md`](AUTH.md).
- [ ] **Bucket lock state matches `retention_mode`** if you use WORM /
      compliance — the daemon refuses to start on a mismatch.
- [ ] **Disk-cache budget sized** for the working set per backend, with
      headroom; confirm uploads drain (cache not pinned near full).
      [`../reference/STORAGE.md`](../reference/STORAGE.md).

## Capacity & backpressure

- [ ] **Backend bandwidth ≥ sustained host write rate.** This is the
      hard constraint — if it isn't met, backpressure surfaces SCSI NOT
      READY under load. [`../reference/BACKPRESSURE.md`](../reference/BACKPRESSURE.md).
- [ ] **Thin-provisioning over-subscription is monitored** against real
      backend usage (`system stats`); you have an alert before it bites.

## Security

- [ ] **CHAP (iSCSI) / TLS-PSK (NVMe/TCP)** enabled, with per-identity
      admission grants for VSA volumes. [`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).
- [ ] **At-rest encryption** enabled where required, with the DEK keystore
      on a real KMS/HSM (not the local sidecar) for sensitive data, and a
      tested `key migrate` runbook. [`ENCRYPTION.md`](ENCRYPTION.md).
- [ ] **Admin HTTP listener secured** — web-admin password set and TLS
      enabled on the network-facing listener. [`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).
- [ ] **Admin socket access controlled** — only trusted operators in the
      application's group (the socket is peer-cred-authed, mode 0660).
- [ ] **Conffile permissions** correct (root:`<application>`, daemon-readable);
      the postinst sets these on package installs.

## Observability

- [ ] **Prometheus / OTLP wired**, dashboards imported, and the PromQL
      alert rules in place (pool near full, backpressure waits, permanent
      backend errors). [`TELEMETRY.md`](TELEMETRY.md).
- [ ] **Alerting sinks configured** (email / webhook) for the five event
      classes. [`ALERTING.md`](ALERTING.md).
- [ ] **Audit log reviewed and verifiable** — `system audit verify`
      passes; rotated files retained per your policy. [`AUDIT.md`](AUDIT.md).

## Resilience & DR

- [ ] **DR path tested**, not just planned: cross-region bucket
      replication enabled out-of-band, and a `library restore` (VTL) /
      backend rebind rehearsed on a spare host. [`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md).
- [ ] **Restores actually verified** — for VTL, restore a backup and diff
      it; for VSA, mount a clone and check. Backups you haven't restored
      are not backups.
- [ ] **WORM / legal-hold policy** decided and applied where mandated
      (VTL). [`CARTRIDGE.md`](CARTRIDGE.md).

## Operations

- [ ] **systemd customizations via `systemctl edit`** so upgrades don't
      clobber them; daemon **not** left auto-started before it is
      configured. [`INSTALLATION.md`](INSTALLATION.md).
- [ ] **Co-resident port/socket overrides** set if both applications share a
      host (default iSCSI/HTTP ports collide).
- [ ] **Upgrade / rollback plan** — pinned package channel, change
      reviewed (alpha: on-disk and wire formats may change without a
      migration path). [`../dev/RELEASING.md`](../dev/RELEASING.md).
- [ ] **`system verify` clean** (chunk pool + page-table / library
      integrity) before go-live.
