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

The release-cut process is in [`RELEASING.md`](RELEASING.md); the
workspace crate map is in [`WORKSPACE.md`](WORKSPACE.md).
