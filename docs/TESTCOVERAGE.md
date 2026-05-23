# Test Coverage

Thur's automated testing is **two-tier**:

- **Unit + integration tests** — ~2,000 per-crate Rust tests run by
  `cargo test`. These cover the storage engines, the SCSI / NVMe
  command sets, dedup, crypto, and the cloud chunk pool — the
  correctness-critical logic.
- **End-to-end conformance suites** — shell suites under `vtl/scripts/`
  and `vsa/scripts/` that drive a *running daemon* over real iSCSI /
  NVMe-TCP. These cover the daemon wiring and the CLI — the integration
  surface unit tests can't reach.

Plus an in-process self-test built into each daemon binary, and CI on
every push.

## Line coverage

The figures below are **line coverage measured by `cargo llvm-cov`**
over the `cargo test` suite — a snapshot of 2026-05-23. Reproduce it
with `scripts/coverage.sh`:

```bash
scripts/coverage.sh            # per-file table (cargo llvm-cov --summary-only)
scripts/coverage.sh --crates   # per-crate line % against the tier floors
scripts/coverage.sh --gate     # same, but exit 1 if any crate is below floor
scripts/coverage.sh --zero     # source files with no #[cfg(test)] block
```

**Overall: 70% line, 72% region.** That single figure needs the
two-tier context to read correctly: `cargo llvm-cov` instruments only
the `cargo test` suite, and the end-to-end shell suites run the
compiled daemon as a black box — they are *not* instrumented. So:

- The **logic crates** — storage, SCSI/NVMe command sets, dedup,
  crypto, chunk pool — are what the unit/integration suite is
  responsible for, and they sit at **80–95%**, every one over its
  80% floor.
- The **daemon and CLI crates** read **30–40%** here, just above
  their 30% floor: their request paths are covered by the end-to-end
  suites instead, which this measurement does not capture.

### `shared/` — cross-product crates

| Crate | Line cov | Coverage focus |
|---|---:|---|
| `shared-admin-audit` | 64% | `system.audit.*` job handlers — reached via the daemon audit verbs |
| `shared-admin-client` | 70% | admin Unix-socket dialer, NDJSON job-stream consumer |
| `shared-admin-http` | 80% | admin HTTP listener, TLS config, self-signed cert gen / regen |
| `shared-admin-iscsi` | 76% | cross-product iSCSI admin handlers — reached via both daemons |
| `shared-admin-proto` | 95% | admin-socket wire types (`JobEvent`, `JobAccepted`) round-trips |
| `shared-admin-server` | 53% | job registry / emitter / NDJSON event streaming |
| `shared-alerting` | 54% | email + webhook sinks, Tera templating, per-class dedup |
| `shared-audit` | 88% | BLAKE3 hash chain, append / verify / rotate, tail cursor |
| `shared-cli` | 74% | CLI UX helpers — reached via the shipped CLIs |
| `shared-cli-alerting` | 57% | `system alerting` CLI — reached via the CLIs + scripts |
| `shared-cli-iscsi` | 56% | `iscsi users` / `target` CLI — reached via the CLIs + scripts |
| `shared-cli-system` | 55% | `system` CLI verbs — reached via the CLIs + scripts |
| `shared-cloud` | 80% | S3 / GCS / Azure / Local backends, retry, compression |
| `shared-cloud-bench` | 36% | cloud benchmark engine — reached via `system cloud benchmark` |
| `shared-crypto` | 95% | AES-256-GCM encrypt / decrypt, IV derivation |
| `shared-dedup-stats` | 100% | dedup exclusive / shared byte split |
| `shared-health` | 100% | `/health` liveness handler |
| `shared-iscsi` | 87% | iSCSI transport, CHAP auth, session + unit-attention |
| `shared-keystore` | 81% | six DEK keystore backends, wrap / unwrap round-trips |
| `shared-naming` | 94% | per-product identity strings |
| `shared-pool` | 91% | content-addressed chunk pool, insertion, GC iteration, budget |
| `shared-telemetry` | 66% | OpenTelemetry instrument plumbing |
| `shared-upload-worker` | 74% | cloud-upload pipeline primitives |
| `shared-verify-core` | 85% | pool + cloud verify sweeps — exercised via the `core-*` verify tests |

### `scsi/` — SCSI command sets

| Crate | Line cov | Coverage focus |
|---|---:|---|
| `scsi-sbc` | 92% | SBC-3 block dispatch: data-path opcodes, reservations, VPD, sizing |
| `scsi-smc` | 83% | SMC-3 changer dispatch, element-address topology |
| `scsi-spc` | 94% | SPC-4 baseline: sense, INQUIRY / VPD, mode pages, REPORT LUNS, PR |
| `scsi-ssc` | 83% | SSC-4 tape dispatch, log / MAM / encryption pages |

### `nvme/` — NVMe stack

| Crate | Line cov | Coverage focus |
|---|---:|---|
| `nvme-base` | 92% | wire-format primitives: SQE / CQE, Identify, Fabrics, registers |
| `nvme-nvm` | 92% | NVM Command Set dispatch, fused compare-and-write |
| `nvme-tcp` | 87% | NVMe/TCP transport state machine, PDU codec, R2T flow |

### `core/` — product cores

| Crate | Line cov | Coverage focus |
|---|---:|---|
| `core-block` | 88% | SBC-3 block core: page table, write-back cache, volume manifests |
| `core-mediachanger` | 83% | SMC-3 medium changer + library inventory + library-wide verify |
| `core-stream` | 80% | LTO cartridge primitives: indexes, FastCDC, AES-GCM, prefetch |

### Product daemons + CLIs

These crates are the integration surface covered by the **end-to-end
suites** below, which `cargo llvm-cov` does not instrument. The
numbers below reflect only what the unit-test layer can see — the
real exercise lives in the shell suites.

| Crate | Line cov | Coverage focus |
|---|---:|---|
| `vsa-cli` | 36% | VSA CLI command modules — see `vsa/scripts/` |
| `vsa-daemon` | 32% | VSA daemon wiring, admin handlers — see `vsa/scripts/` |
| `vtl-cli` | 39% | VTL CLI command modules — see `vtl/scripts/` |
| `vtl-daemon` | 31% | VTL daemon wiring, iSCSI / admin paths — see `vtl/scripts/` |

A crate can show coverage without owning many tests of its own:
`shared-verify-core` has no inline tests yet is 85% covered, because
`core-mediachanger` and `core-block`'s verify tests exercise it.
Several thin cross-product crates (`shared-admin-audit`,
`shared-admin-iscsi`, the `shared-cli-*` family) clear their 50%
floor entirely on the strength of their consumers' tests — what they
add directly to the workspace is structural glue.

## Coverage floors

Every crate has a **line-coverage floor**, and every non-trivial source
file must carry at least one `#[cfg(test)]` block. `scripts/coverage.sh`
measures both; `--zero` lists files with no test block, minus the
reviewed-trivial paths in `scripts/coverage-exempt.txt` (pure re-export
`lib.rs` files and bare type / enum / const / error definitions).

| Tier | Crates | Floor |
|---|---|---|
| Critical — core | `core/stream`, `core/mediachanger`, `core/block` | 80% |
| Critical — protocol | `scsi/*`, `nvme/*` | 80% |
| Critical — shared | `crypto`, `pool`, `iscsi`, `audit`, `keystore`, `cloud` | 80% |
| Standard — shared | all other `shared/*` crates | 50% |
| Products | `vtl/daemon`, `vtl/cli`, `vsa/daemon`, `vsa/cli` | 30% |

The product daemons and CLIs carry the lower 30% floor because their
request paths are exercised by the end-to-end shell suites, which
`cargo llvm-cov` does not instrument — the unit-test floor covers only
their pure logic (config parsing, registries, job-argument handling).

### Structural sub-floor exceptions

- **`shared/cloud-bench` (36% < 50%)** — the bench engine
  (`bench_cell` / `parallel_delete` / the print helpers) is a
  throughput probe whose minimum useful transfer is **1 GiB per
  cell**, allocated in memory and pushed through a real `CloudBackend`.
  Validation, options, config loading, PRNG, error variants are unit-
  tested; the bench loop is reached via the `system cloud benchmark`
  integration path.
- **`shared/cloud` gcs.rs (5%)** — pulls `shared/cloud` close to its
  80% floor: `gcs.rs` (307 lines) is bound to the
  `google-cloud-storage` SDK, which has no endpoint override that
  the in-crate `wiremock` rig can drive (s3.rs and azure.rs both
  use reqwest-based clients that accept a custom endpoint URL).
  The GCS backend is exercised by `vsa/scripts/test-iscsi-fs-cloud.sh`
  against a real GCS bucket; every other file in `shared/cloud` is
  at 80%+ per-file and the crate as a whole sits at 80% — just on
  the line.

## End-to-end suites

Run from the repo root; each script's header documents its
prerequisites and exactly what it covers. The `*-conformance` suites
drive a real initiator (`open-iscsi`, `sg3-utils`, `libiscsi`,
`nvme-cli`) against a running daemon; the workflow suites perform a
full backup / filesystem round-trip.

### Thur VTL — `vtl/scripts/`

| Script | Covers |
|---|---|
| `test-smoke.sh` | basic end-to-end bring-up |
| `test-iscsi-conformance.sh` | iSCSI login / CHAP / transport conformance |
| `test-scsi-conformance.sh` | SMC-3 + SSC-4 SCSI command conformance |
| `test-backup-workflow.sh` | end-to-end backup + restore (local backend) |
| `test-backup-cloud.sh` | end-to-end backup + restore (real cloud backend) |
| `test-backup-cloud-failures.sh` | cloud failure-path handling |
| `test-backup-cloud-resume.sh` | boot-time orphan-upload recovery |
| `test-cartridge-migrate.sh` | cartridge migrate + archive between backends |
| `test-dr-restore.sh` | cross-region disaster-recovery restore |
| `test-pipeline-layers.sh` | dedup / compression / encryption layer matrix |

### Thur VSA — `vsa/scripts/`

| Script | Covers |
|---|---|
| `test-smoke.sh` | basic end-to-end bring-up |
| `test-iscsi-conformance.sh` | iSCSI login / CHAP / transport conformance |
| `test-scsi-conformance.sh` | SBC-3 SCSI command conformance |
| `test-nvmetcp-conformance.sh` | NVMe/TCP transport + NVM Command Set conformance |
| `test-iscsi-fs-workflow.sh` | filesystem round-trip over iSCSI (local backend) |
| `test-iscsi-fs-cloud.sh` | filesystem round-trip over iSCSI (real cloud backend) |
| `test-nvme-fs-workflow.sh` | filesystem round-trip over NVMe/TCP (local backend) |
| `test-nvme-fs-cloud.sh` | filesystem round-trip over NVMe/TCP (real cloud backend) |
| `test-fs-cloud-failures.sh` | cloud failure-path handling |
| `test-keystore.sh` | DEK keystore wrap / unwrap / migrate per backend |
| `test-kmip-pykmip.sh` | `kmip` keystore backend against a local PyKMIP server |
| `test-pipeline-layers.sh` | dedup / compression / encryption layer matrix |

## In-process self-test

Each daemon binary has a `--test` mode that runs an in-process smoke
sequence and exits, without binding iSCSI or touching the operator's
data directory:

```bash
thurvtld --test    # cartridge / library / S3 / prefetch / upload-worker
thurvsad --test    # volume bring-up / data-path / SYNCHRONIZE CACHE / sparse-page
```

It is a fast confidence check that the core read / write / cloud paths
work on the host — useful right after install or in a constrained
environment where the full shell suites can't run.

## Continuous integration

Two GitHub Actions workflows run on every push:

- **`ci.yml`** — `cargo fmt --check`, `cargo clippy --workspace
  --all-targets`, `cargo build --workspace --all-targets`, and
  `cargo test --workspace` (the full per-crate suite above).
- **`deny.yml`** — `cargo deny` for advisories, license policy, and
  banned / duplicate dependencies.
