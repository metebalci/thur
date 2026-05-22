# Test Coverage

Thur's automated testing is **two-tier**:

- **Unit + integration tests** — 1,299 per-crate Rust tests run by
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
over the `cargo test` suite — a snapshot of 2026-05-22. Reproduce it
with `scripts/coverage.sh`:

```bash
scripts/coverage.sh            # per-file table (cargo llvm-cov --summary-only)
scripts/coverage.sh --crates   # per-crate line % against the tier floors
scripts/coverage.sh --gate     # same, but exit 1 if any crate is below floor
scripts/coverage.sh --zero     # source files with no #[cfg(test)] block
```

**Overall: 55% line, 59% region.** That single figure needs the
two-tier context to read correctly: `cargo llvm-cov` instruments only
the `cargo test` suite, and the end-to-end shell suites run the
compiled daemon as a black box — they are *not* instrumented. So:

- The **logic crates** — storage, SCSI/NVMe command sets, dedup,
  crypto, chunk pool — are what the unit/integration suite is
  responsible for, and they sit at **75–95%**.
- The **daemon and CLI crates** read **1–29%** here: their request
  paths are covered by the end-to-end suites instead, which this
  measurement does not capture. The low number reflects what
  `cargo llvm-cov` can see, not how well that code is exercised.

### `shared/` — cross-product crates

| Crate | Tests | Line cov | Coverage focus |
|---|---:|---:|---|
| `shared-admin-audit` | 0 | 0% | `system.audit.*` job handlers — reached via the daemon audit verbs |
| `shared-admin-client` | 13 | 32% | admin Unix-socket dialer, NDJSON job-stream consumer |
| `shared-admin-http` | 21 | 80% | admin HTTP listener, TLS config, self-signed cert gen / regen |
| `shared-admin-iscsi` | 0 | 0% | cross-product iSCSI admin handlers — reached via both daemons |
| `shared-admin-proto` | 12 | 95% | admin-socket wire types (`JobEvent`, `JobAccepted`) round-trips |
| `shared-admin-server` | 3 | 44% | job registry / emitter / NDJSON event streaming |
| `shared-alerting` | 18 | 49% | email + webhook sinks, Tera templating, per-class dedup |
| `shared-audit` | 33 | 85% | BLAKE3 hash chain, append / verify / rotate, tail cursor |
| `shared-cli` | 0 | 0% | CLI UX helpers — reached via the shipped CLIs |
| `shared-cli-alerting` | 0 | 0% | `system alerting` CLI — reached via the CLIs + scripts |
| `shared-cli-iscsi` | 0 | 8% | `iscsi users` / `target` CLI — reached via the CLIs + scripts |
| `shared-cli-system` | 0 | 0% | `system` CLI verbs — reached via the CLIs + scripts |
| `shared-cloud` | 91 | 54% | S3 / GCS / Azure / Local backends, retry, compression |
| `shared-cloud-bench` | 0 | 0% | cloud benchmark engine — reached via `system cloud benchmark` |
| `shared-crypto` | 9 | 95% | AES-256-GCM encrypt / decrypt, IV derivation |
| `shared-dedup-stats` | 3 | 100% | dedup exclusive / shared byte split |
| `shared-health` | 2 | 100% | `/health` liveness handler |
| `shared-iscsi` | 45 | 46% | iSCSI transport, CHAP auth, session + unit-attention |
| `shared-keystore` | 94 | 69% | six DEK keystore backends, wrap / unwrap round-trips |
| `shared-naming` | 15 | 94% | per-product identity strings |
| `shared-pool` | 45 | 90% | content-addressed chunk pool, insertion, GC iteration, budget |
| `shared-telemetry` | 3 | 67% | OpenTelemetry instrument plumbing |
| `shared-upload-worker` | 4 | 73% | cloud-upload pipeline primitives |
| `shared-verify-core` | 0 | 85% | pool + cloud verify sweeps — exercised via the `core-*` verify tests |

### `scsi/` — SCSI command sets

| Crate | Tests | Line cov | Coverage focus |
|---|---:|---:|---|
| `scsi-sbc` | 166 | 92% | SBC-3 block dispatch: data-path opcodes, reservations, VPD, sizing |
| `scsi-smc` | 14 | 49% | SMC-3 changer dispatch, element-address topology |
| `scsi-spc` | 51 | 91% | SPC-4 baseline: sense, INQUIRY / VPD, mode pages, REPORT LUNS, PR |
| `scsi-ssc` | 94 | 48% | SSC-4 tape dispatch, log / MAM / encryption pages |

### `nvme/` — NVMe stack

| Crate | Tests | Line cov | Coverage focus |
|---|---:|---:|---|
| `nvme-base` | 27 | 90% | wire-format primitives: SQE / CQE, Identify, Fabrics, registers |
| `nvme-nvm` | 10 | 71% | NVM Command Set dispatch, fused compare-and-write |
| `nvme-tcp` | 61 | 87% | NVMe/TCP transport state machine, PDU codec, R2T flow |

### `core/` — product cores

| Crate | Tests | Line cov | Coverage focus |
|---|---:|---:|---|
| `core-block` | 116 | 88% | SBC-3 block core: page table, write-back cache, volume manifests |
| `core-mediachanger` | 209 | 80% | SMC-3 medium changer + library inventory + library-wide verify |
| `core-stream` | 95 | 76% | LTO cartridge primitives: indexes, FastCDC, AES-GCM, prefetch |

### Product daemons + CLIs

Low here by construction — these crates are the integration surface
covered by the **end-to-end suites** below, which `cargo llvm-cov` does
not instrument. The number is a measurement artifact, not a gap.

| Crate | Tests | Line cov | Coverage focus |
|---|---:|---:|---|
| `vsa-cli` | 5 | 2% | VSA CLI command modules — see `vsa/scripts/` |
| `vsa-daemon` | 26 | 29% | VSA daemon wiring, admin handlers — see `vsa/scripts/` |
| `vtl-cli` | 3 | 1% | VTL CLI command modules — see `vtl/scripts/` |
| `vtl-daemon` | 11 | 6% | VTL daemon wiring, iSCSI / admin paths — see `vtl/scripts/` |

A crate can show coverage without owning tests: `shared-verify-core`
has no tests of its own yet is 85% covered, because `core-mediachanger`
and `core-block`'s verify tests exercise it. The 0% crates — the CLI
helper crates, `shared-admin-audit`, `shared-admin-iscsi`,
`shared-cloud-bench` — are thin cross-product glue reached only through
their consumers and the end-to-end suites.

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

## Known gaps

Not every low number is an instrumentation artifact. These crates have
real unit tests but meaningful untested branches — the first place more
unit tests would pay off:

- `scsi-ssc` (48%) — SSC-4 tape command dispatch
- `scsi-smc` (49%) — SMC-3 changer command dispatch
- `shared-iscsi` (46%) — iSCSI transport / session management
- `shared-cloud` (54%) — cloud-backend error + retry paths

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
