# Admin Guide

Operating Thur VTL and Thur VSA in production. Read top-to-bottom the
first time; thereafter it's a reference. New here? Start with the
[`../QUICKSTART.md`](../QUICKSTART.md) for the five-minute happy path,
then come back.

The how-it-works internals (wire spec, conformance tables, storage
engine, dedup, backpressure) live in the
[reference set](../reference/); this guide is the operator's view.

## Start here

1. [`CONCEPTS.md`](CONCEPTS.md) — the mental model: chunk pool, dedup,
   backend-as-source-of-truth, disk cache, thin provisioning, backpressure.
2. [`INSTALLATION.md`](INSTALLATION.md) — packages, containers, signature
   verification, group membership, running the daemon.
3. [`CONFIGURATION.md`](CONFIGURATION.md) — every conffile and YAML key;
   the same reference `config defaults` prints.

## Storage backends

- [`AUTH.md`](AUTH.md) — storage-backend credentials: per-backend `auth:`
  blocks, default chains, the daemon env file.
- [`S3_BACKENDS.md`](S3_BACKENDS.md) — the S3-compatible provider matrix
  (Backblaze B2, Wasabi, Hetzner, OVHcloud, …).

## Connecting hosts

- [`CONNECTING.md`](CONNECTING.md) — iSCSI + NVMe/TCP login, rescan,
  advertised address.
- [`ISCSIADM.md`](ISCSIADM.md) — the `iscsiadm` initiator cheatsheet.

## Operating

- **Thur VTL:** [`CARTRIDGE.md`](CARTRIDGE.md) — cartridge lifecycle,
  WORM, legal hold, encryption.
- **Thur VSA:** [`VSA_OPERATIONS.md`](VSA_OPERATIONS.md) — volumes,
  snapshots, clones, in-place restore, online resize, encryption.
- [`CLI.md`](CLI.md) — the CLI surface, daemon-mode rules, and the
  long-running job protocol.

## Securing

- [`NETWORK_SECURITY.md`](NETWORK_SECURITY.md) — admin HTTP TLS, the
  web-admin password, NVMe/TCP TLS-PSK and DH-HMAC-CHAP.
- [`ENCRYPTION.md`](ENCRYPTION.md) — at-rest encryption and the DEK
  keystore backends (local / KMS / Vault / Key Vault / KMIP).

## Observability

- [`AUDIT.md`](AUDIT.md) — the append-only, hash-chained audit log.
- [`TELEMETRY.md`](TELEMETRY.md) — Prometheus / OTLP metrics, dashboards,
  PromQL alert rules.
- [`ALERTING.md`](ALERTING.md) — email / webhook alert sinks.

## Resilience

- [`DISASTER_RECOVERY.md`](DISASTER_RECOVERY.md) — cross-region restore,
  cartridge migrate / archive, restore-archive.
- [`PRODUCTION_READINESS.md`](PRODUCTION_READINESS.md) — the go-live
  checklist.

## When something's wrong

- [`TROUBLESHOOTING.md`](TROUBLESHOOTING.md) — symptom-first.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — what's tested, what's expected
  to work, and the divergences from physical LTO.

## Kubernetes

- [`CSI.md`](CSI.md) — the Thur VSA CSI driver.
