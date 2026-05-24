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

Two measurement modes, each with its own `scripts/coverage.sh` entry
point. Snapshots below are 2026-05-24.

```bash
scripts/coverage.sh                    # per-file table (unit tests only)
scripts/coverage.sh --crates           # per-crate line % vs floor (unit tests)
scripts/coverage.sh --gate             # --crates, exit 1 below floor
scripts/coverage.sh --integrated       # unit + shell-suite daemon runs, 10-20 min
scripts/coverage.sh --integrated-gate  # --integrated, exit 1 below floor
scripts/coverage.sh --zero             # source files with no #[cfg(test)] block
```

**Unit tests only: 70.8% line, 73.3% region.**
**Integrated (unit + shell suites): 75.4% line.**

The two numbers split the responsibility cleanly: unit tests own the
logic crates (storage, SCSI/NVMe, dedup, crypto, chunk pool — these
sit at **80-95%** in either mode). The shell suites under
`vtl/scripts/` and `vsa/scripts/` drive the daemons through real
SCSI/NVMe-TCP traffic; only the integrated mode captures their
contribution to the product daemon/CLI crates, where the lift is
**+20-25 percentage points**:

| Crate | Unit only | Integrated | Δ |
|---|---:|---:|---:|
| `vsa/daemon` | 35% | 61% | +26 |
| `vtl/cli` | 39% | 60% | +21 |
| `vsa/cli` | 36% | 52% | +16 |
| `vtl/daemon` | 32% | 32% | 0 (iSCSI handler is mostly unhit even under sudo'd workloads — see `iscsi/handler.rs`) |
| `shared/admin-server` | 65% | 87% | +22 |
| `core/mediachanger` | 83% | 90% | +7 |

### `shared/` — cross-product crates

Per-crate numbers are from the **integrated** run (unit + shell suites);
columns flag the tier per `scripts/coverage-report.py`.

| Crate | Line cov | Tier | Coverage focus |
|---|---:|---|---|
| `shared-admin-audit` | 64% | shared | `system.audit.*` job handlers — reached via the daemon audit verbs |
| `shared-admin-client` | 70% | shared | admin Unix-socket dialer, NDJSON job-stream consumer |
| `shared-admin-http` | 80% | shared | admin HTTP listener, TLS config, self-signed cert gen / regen |
| `shared-admin-iscsi` | 81% | shared | cross-product iSCSI admin handlers — reached via both daemons |
| `shared-admin-proto` | 95% | shared | admin-socket wire types (`JobEvent`, `JobAccepted`) round-trips |
| `shared-admin-server` | 87% | **control-plane critical** | admin socket bind, peer-cred extractor, NDJSON job streaming |
| `shared-alerting` | 73% | shared | email + webhook sinks, Tera templating, per-class dedup |
| `shared-audit` | 88% | **critical** | BLAKE3 hash chain, append / verify / rotate, tail cursor |
| `shared-cli` | 74% | shared | CLI UX helpers — reached via the shipped CLIs |
| `shared-cli-alerting` | 57% | shared | `system alerting` CLI — reached via the CLIs + scripts |
| `shared-cli-iscsi` | 56% | shared | `iscsi users` / `target` CLI — reached via the CLIs + scripts |
| `shared-cli-system` | 61% | shared | `system` CLI verbs — reached via the CLIs + scripts |
| `shared-cloud` | 80% | **critical** | S3 / GCS / Azure / Local backends, retry, compression |
| `shared-cloud-bench` | 36% | shared (exception) | cloud benchmark engine — reached via `system cloud benchmark` |
| `shared-crypto` | 95% | **critical** | AES-256-GCM encrypt / decrypt, IV derivation |
| `shared-dedup-stats` | 100% | **control-plane critical** | dedup exclusive / shared byte split |
| `shared-health` | 100% | shared | `/health` liveness handler |
| `shared-iscsi` | 87% | **critical** | iSCSI transport, CHAP auth, session + unit-attention |
| `shared-keystore` | 79% | **critical** | six DEK keystore backends, wrap / unwrap round-trips |
| `shared-naming` | 94% | shared | per-product identity strings |
| `shared-pool` | 91% | **critical** | content-addressed chunk pool, insertion, GC iteration, budget |
| `shared-telemetry` | 66% | shared | OpenTelemetry instrument plumbing |
| `shared-upload-worker` | 89% | **control-plane critical** | cloud-upload PUT + HEAD-probe primitive |
| `shared-verify-core` | 85% | **control-plane critical** | pool + cloud verify sweeps — exercised via the `core-*` verify tests |

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

Both columns reproduce the integration surface from two angles: the
unit-test-only number (`scripts/coverage.sh --crates`) is what
CI's quick gate sees; the integrated number is what
`scripts/coverage.sh --integrated` produces after driving the shell
suites against instrumented binaries. The 30% floor is calibrated to
the unit-only number — once integrated is wired into CI, the floor
gets raised in lockstep.

| Crate | Unit only | Integrated | Coverage focus |
|---|---:|---:|---|
| `vsa-cli` | 36% | 52% | VSA CLI command modules — see `vsa/scripts/` |
| `vsa-daemon` | 35% | 61% | VSA daemon wiring, admin handlers — see `vsa/scripts/` |
| `vtl-cli` | 39% | 60% | VTL CLI command modules — see `vtl/scripts/` |
| `vtl-daemon` | 32% | 32% | VTL daemon wiring; iSCSI handler (`iscsi/handler.rs`, `iscsi/session.rs`) stays mostly unhit because the conformance suites exercise SSC/SMC dispatch into `scsi-ssc` / `scsi-smc` rather than the wrapper |

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
| Critical — data-path | `core/*`, `scsi/*`, `nvme/*`, `shared/crypto`, `shared/pool`, `shared/iscsi`, `shared/audit`, `shared/keystore`, `shared/cloud` | 80% |
| Critical — control-plane | `shared/admin-server`, `shared/verify-core`, `shared/upload-worker`, `shared/dedup-stats` | 80% |
| Standard — shared | all other `shared/*` crates | 50% |
| Products | `vtl/daemon`, `vtl/cli`, `vsa/daemon`, `vsa/cli` | 30% |

The two critical tiers are split by failure mode, not by criticality
level: **data-path** bugs corrupt or lose on-disk / cloud data
silently; **control-plane** bugs cause silent operational failures
(admin socket down, integrity check skipped, alert never fires) or
unrecoverable backups. Both tiers carry the same 80% floor.

The product daemons and CLIs carry the lower 30% floor because their
request paths are exercised by the end-to-end shell suites, which the
**`--crates`** mode does not capture. Running **`--integrated`**
captures them and lifts the four product crates to 50-60%; that mode
needs sudo for the kernel-initiator suites and takes 10-20 minutes,
so it isn't yet the CI gate.

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
  the line. The `googleapis/google-cloud-rust` SDK ships an official
  client-level mocking pattern that could close this gap with a
  moderate `GcsBackend::new` refactor; tracked in
  [`ROADMAP.md`](../ROADMAP.md).

## Active targets

Crates currently below their integrated-mode floor — listed in priority
order:

1. **`shared/keystore` (79% integrated, 80.6% `--crates`)** — small
   measurement drift vs `cargo llvm-cov`'s native number; not
   actionable until the broader integrated-mode profraw merge bug is
   resolved (see § Known measurement issues below). The proper close
   path is the `azurekv.rs` (49%) and `gcpkms.rs` (30%) mocked-client
   tests once the SDK refactor lands (same path as the gcs.rs note
   above).
2. **`shared/cloud-bench` (36% < 50%)** — documented exception; no
   action expected.

### Known measurement issues

The `--integrated` runner's `llvm-profdata merge` step globs
`target/llvm-cov-target/thur-*.profraw`, but the unit-test profraws
produced by `cargo llvm-cov --no-report --workspace` end up in a
location that doesn't survive the shell-suite phase by the time the
merge runs — only the shell-suite-spawned daemon profraws are
consolidated. This depresses every crate's integrated-mode number,
not just `shared/keystore`. The `--crates` mode is unaffected and
remains the authoritative unit-mode gate for CI.

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
| `test-crash-audit-append.sh` | kill -9 mid audit-burst → restart → chain re-verifies |
| `test-crash-chunk-seal.sh` | kill -9 mid tape stream → restart → acked blocks survive |
| `test-many-cartridge-lifecycle.sh` | soak: create/list/stats N cartridges (THURVTL_SOAK=1) |
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
| `test-crash-audit-append.sh` | kill -9 mid audit-burst → restart → chain re-verifies |
| `test-crash-page-flush.sh` | kill -9 after host fsync → restart → every byte survives |
| `test-iscsi-multi-initiator.sh` | two initiators + PR matrix, RESERVATION CONFLICT sense |
| `test-multi-volume-dedup.sh` | soak: create/list/stats/gc N volumes (THURVSA_SOAK=1) |
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

### Product-agnostic suites — `scripts/`

| Script | Covers |
|---|---|
| `test-coresident-smoke.sh` | both daemons co-resident: disjoint ports, admin sockets, audit dirs |

## Continuous integration

Two GitHub Actions workflows run on every push:

- **`ci.yml`** — `cargo fmt --check`, `cargo clippy --workspace
  --all-targets`, `cargo build --workspace --all-targets`, and
  `cargo test --workspace` (the full per-crate suite above).
- **`deny.yml`** — `cargo deny` for advisories, license policy, and
  banned / duplicate dependencies.
