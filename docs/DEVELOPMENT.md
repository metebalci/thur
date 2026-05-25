# Development

```bash
cargo build [--release]       # binaries in target/{debug,release}/
cargo test
cargo fmt && cargo clippy

# Run a daemon in the foreground from the build tree
RUST_LOG=info ./target/release/thurvtld --config thurvtl.yaml
```

`cargo test` runs the workspace suite — 1,299 unit and integration
tests across the 38 crates. Measured with `cargo llvm-cov`, the storage
and protocol crates (storage engines, SCSI / NVMe command sets, dedup,
crypto, chunk pool) carry **75–95% line coverage**; the daemon and CLI
integration surface is covered separately by the end-to-end conformance
suites under `vtl/scripts/` and `vsa/scripts/` (`test-smoke.sh`,
`test-*-conformance.sh`, and backup / filesystem workflow tests) — each
script's header documents its prerequisites and what it covers.
[`TESTCOVERAGE.md`](TESTCOVERAGE.md) has the per-crate coverage
breakdown, the methodology, and the suite catalogue.

## Real-backend integration tests

A handful of scripts under `vtl/scripts/` and `vsa/scripts/` (the
`test-*-storage.sh` and `test-fs-storage-failures.sh` suites, plus
`test-pipeline-layers.sh`, `test-cartridge-migrate.sh`, `test-keystore.sh`,
and `test-monte-carlo.sh` against a non-local backend) exercise real
S3 / GCS / Azure / AIStor / MinIO connections end-to-end. They need:

- `private/storage-backends.yaml` — one entry per backend (bucket /
  prefix / region / endpoint / `auth:`). Same shape as the daemon's
  `storage.backends:` block.
- `private/keystore-backends.yaml` — same shape for `test-keystore.sh`'s
  per-backend DEK wrap/unwrap matrix.
- Credentials exported in your shell — `AWS_*`, `GOOGLE_*`, `AZURE_*`,
  plus any `auth: env` names referenced from the YAML
  (`AISTOR_*`, `WASABI_*`, …). The scripts self-elevate via `sudo` and
  forward those env vars explicitly by name pattern, so they survive
  the privilege hop as long as they're `export`ed.

Optional: `private/thur.env` (KEY=VAL per line). Every script gates the
source on `[[ -r ... ]]` and auto-loads it under `set -a` before
self-elevation. Use it to persist credentials across shells; skip it if
your shell already exports them from your dotfiles or a credential
manager.

`private/` is gitignored — it carries live cloud credentials and bucket
coordinates. Override paths with `THURV{TL,SA}_SOURCE_BACKENDS` /
`THURVSA_SOURCE_KEYSTORES` if your fixtures live elsewhere. Pick a
backend per run with `THURV{TL,SA}_TEST_BACKEND=<name>` matching an entry
in the YAML. All non-real-backend tests (`test-smoke.sh`,
`test-*-conformance.sh`, `test-backup-workflow.sh`,
`test-iscsi-fs-workflow.sh`, `test-nvme-fs-workflow.sh`) run against an
inline local backend and need no `private/` setup.

The release-cut process is in [`RELEASING.md`](RELEASING.md); the
workspace crate map is in [`WORKSPACE.md`](WORKSPACE.md).
