# Thur ROADMAP

Open work and forward-looking ideas for Thur VTL and Thur VSA. The
roadmap is divided into two halves by a hard divider:

- **Committed** (above the divider) — work that is planned and
  intended to ship, organized in the order `/next` surfaces it:
  - **FIXES** — bugs, cleanups, and technical debt that are actively
    intended for resolution.
  - **FEATURES** — new functionality on deck.
  - **BLOCKERS** — items that must land before tagging the named
    milestone (beta / rc / ga), each tagged with which milestone
    they block.

  Delete an entry when it ships — don't move it to a "Shipped" section.
- **Not committed** (below the `# Not committed` divider) — Ideas,
  Parked, and Declined. `/next` does not surface anything below it.

Shipped detail lives in `git log` plus the design docs under `docs/`.

Lifecycle: an idea graduates by moving up across the divider into
FEATURES (or FIXES / BLOCKERS). A committed entry that loses its case
drops back to Parked. Explicit rejections go to Declined.

---

## FIXES

### Raise unit-test coverage on the low-coverage crates
`docs/TESTCOVERAGE.md` (2026-05-22 `cargo llvm-cov` snapshot) flags four
crates with real unit-test gaps — meaningful untested branches, not an
instrumentation artifact:

- `scsi-ssc` (48% line) — SSC-4 tape command dispatch
- `scsi-smc` (49%) — SMC-3 changer command dispatch
- `shared-iscsi` (46%) — iSCSI transport / session management
- `shared-cloud` (54%) — cloud-backend error + retry paths

Add unit tests for the untested branches in each. The daemon / CLI
crates also read low (1–29%), but that is by construction — their
request paths are covered by the `vtl/scripts/` + `vsa/scripts/`
end-to-end suites, which `cargo llvm-cov` does not instrument; that is
not a gap to close here.

---

## FEATURES

### Storage & cache

#### Policy-driven multi-cloud tiering (cartridge migration between backends)
A cartridge is bound to one backend for life, so hot / warm / cold
layouts can't express "after 90 days idle, move to cold storage"
without export / re-import.

Sketch: `cloud.tiering.policies[]` block with predicates
(`age_days_since_last_write`, `barcode_prefix`, `lto_generation`,
`worm`, `legal_held`) and an action (`migrate_to: <backend-name>`).
Daily worker walks inventory + manifests, evaluates, enqueues
migrations. Migration = chunk-by-chunk re-upload to the target via
`ChunkStore`, then manifest rewrite (flips `cartridge.backend` and
per-chunk location), then source-side deletion gated on cloud-side
success. Refuse while loaded, on WORM mismatch, or on legal hold
(unless policy opts in). Audit `cartridge.tiered` with
from / to / chunk count / bytes.

Touches: new `core/mediachanger/src/tiering.rs`, `cloud_config.rs`
(schema), `cartridge.rs` (manifest-rewrite helper), `thurvtl`
(`system tiering {status, run-now, plan}`), `docs/SPEC.md`.

### Cloud-backend ecosystem

#### S3-compatible backend matrix
The `s3` backend type targets AWS S3 + MinIO / Wasabi today. Survey
other providers (Hetzner, OVHcloud, IONOS, Backblaze B2, Exoscale SOS)
on auth quirks, Object Lock / legal-hold parity, multipart limits,
request pricing, minimum-retention penalties. Output: per-provider
compatibility table in `docs/SPEC.md` + tested `thurvtl.yaml`
snippets. Likely no code changes for basic R/W; WORM / legal hold may
need per-provider gates.

### Auth & admin surface

#### Admin auth: LDAP / Active Directory
The admin surface is split today: mutations go through the Unix socket
(SO_PEERCRED authed, local processes only) and reads come from the TCP
HTTP listener (no auth, for Prometheus scrape only). Neither works for
a remote multi-admin install.

Add an LDAP / AD bind-and-search authenticator + admin-user model.
Wire shape: HTTP `Authorization: Basic` or session-cookie against the
YAML-configured directory. Per-admin `username` populates
`AuditActor::rest(user, addr)` (defined at
`shared/audit/src/audit.rs:94-140`, currently unused — adding LDAP is
what consumes it).

Scope this round:
- LDAP / AD bind-and-search only (single directory, no failover).
- Single role: any LDAP-authenticated admin can do anything the Unix
  socket can today. RBAC is a separate follow-up — the Ideas entry
  "Admin auth: AD / LDAP / M365 SSO" folds in the SAML/OIDC + RBAC
  layer.
- Wraps both daemons; trait abstraction in a new shared crate (mirror
  the `shared-admin-iscsi` pattern).
- TLS on the admin HTTP listener is the transport prerequisite (so the
  LDAP cred doesn't ride plaintext); already shipped via
  `shared-admin-http`, so this is unblocked.

Out of scope (deferred): SAML / OIDC, RBAC, multi-admin separation of
duty, HSM-backed admin tokens.

### Management UX

#### Web UI v1
Browser frontend on top of `/api/v1/*`: library inventory, cartridge
browser, op history, configuration panel. Hosted on the existing TCP
HTTP listener (the Unix socket's peer-credential auth doesn't
translate to remote browsers — auth comes from "Admin auth: LDAP / AD"
above, a hard prerequisite). REST mutating handlers should populate
`AuditActor::rest(user, addr)` (`shared/audit/src/audit.rs:94-140`,
unused until the LDAP work lands).

Scope of v1:
- Read-only dashboards: library + cartridge inventory (VTL), volume
  list + per-volume status (VSA), FETB usage, audit log tail, recent
  jobs.
- Mutating forms for the highest-friction operator paths:
  add/remove CHAP user, add/remove volume (VSA) or cartridge (VTL),
  set legal hold, trigger `cloud check`.
- One static-asset bundle served by the daemon (no separate web
  server); SPA built ahead of time, dropped into `dist/webui/` like
  the completion scripts.

Out of scope for v1: graphical config editor (operators edit YAML +
restart), multi-tenant UI partitions, real-time event streaming
(polling is fine), mobile-responsive polish (best-effort only).

#### Grafana dashboards & alert rules
Reference dashboards for the existing `/metrics` endpoint and a
documented alert ruleset (cache >90%, upload queue >100, validation
failures, session disconnects). We expose the metrics; consumer wiring
is BYO today. Pairs with — does not replace — the first-party
email/webhook alerting (shipped, see
[`docs/ALERTING.md`](docs/ALERTING.md)); this is for shops already
running Prometheus + Alertmanager.

### Ecosystem

#### User & admin documentation
Installation, configuration, troubleshooting, architecture deep-dive,
SCSI + iSCSI compatibility matrix, production-readiness checklist.
CLAUDE.md is dense internal context; user-facing docs are the missing
layer.

#### Published performance benchmarks
Reproducible throughput / latency / dedup-ratio numbers against
representative workloads (tar of `/etc`, tar of DB dumps, mixed ISO
blobs). Needs a benchmark harness — current smoke tests don't report
numbers.

---

## BLOCKERS

Each entry is tagged with the milestone it blocks. The bucket is about
release-gating urgency, not work shape — entries can be fixes or
features by nature.

### `[beta]` GCS compression metadata
`shared/cloud/src/gcs.rs::download_chunk` keys decompression off
`self.compression_config.algorithm` (set once at daemon start from
YAML); S3 (`s3.rs:268` upload, `s3.rs:427-465` download) and Azure use
per-object metadata as the source of truth. GCS does not.

**Failure mode**: operator uploads with `algorithm: zstd`, switches
YAML to `lz4` (or `none`), restarts daemon. Pre-switch downloads
mis-decode → corruption / panic.

**Why deferred**: the `google-cloud-storage` crate (both the community
yoshidan and the official googleapis crate) exposes `download_object`
without custom metadata; reading metadata requires a separate
`get_object_metadata` RPC, doubling the GET count on the chunk read
path. Acceptable for self-hosted alpha; load-bearing once operators
carry buckets across config tweaks.

**Two paths to ship**: (1) shadow the S3 pattern by issuing the extra
`get_object_metadata` RPC (clean, one extra GET per download), or
(2) bundle the metadata into a header prefix on the object body (no
extra RPC, bespoke wire format). Path (1) is the right maintainability
tradeoff unless GCS GET pricing makes path (2) materially cheaper at
scale.

Must land before tagging anything beyond alpha.

### `[rc/ga]` Validate `disk_cache.recent_seal_pin_seconds` default
Knob shipped (default `0`, pin disabled). When set > 0, eviction skips
any pool chunk whose most recent `lru.idx` touch (seal or read) is
inside the window, on top of the existing `LocalOnly` pin. Targets the
verify-after-write pattern (Veeam / NetBackup re-reading freshly
written data within seconds). Symmetric across VTL + VSA
(`core_{ssc,sbc}::DiskCacheManager`).

The default `0` matches today's tested behavior with zero migration
risk. Before tagging RC / GA, run a workload trace
(test-backup-workflow against Veeam / NetBackup, plus a tight cache_gb
under sustained mixed write+read) and decide whether to bump the
default — `0` if traces show no verify-after-write churn, or a low
non-zero (60s / 300s) if the cache-miss-then-re-download cost is
consistently load-bearing. Document the chosen number's reasoning in
the release notes that flip it. Touches
`vtl/cli/src/commands/defaults_reference.yaml`,
`vsa/cli/src/commands/defaults_reference.yaml`,
`vtl/daemon/src/main.rs` `default_recent_seal_pin_seconds`,
`vsa/daemon/src/config.rs` `default_recent_seal_pin_seconds`.

---

# Not committed

Everything below this divider is forward-looking. `/next` does not
surface it as work — see the lifecycle note at the top.

---

## Ideas

Forward-looking feature catalog — the "where we *could* go" list. Live
possibilities only. When an idea graduates to "we're going to build
this," move it up into FEATURES (or FIXES / BLOCKERS).

### TLS-protected admin HTTP
`/metrics` + `/health` are bare TCP HTTP today; mutations go through
the Unix socket. Add mTLS with client certs.

### OpenAPI spec for the admin API
There is no machine-readable spec for the `/api/v1/*` admin surface
today — the handler signatures plus [`docs/CLI.md`](docs/CLI.md) and
[`docs/SPEC.md`](docs/SPEC.md) are the contract. Hand-write or
generate a `docs/openapi.yaml` so downstream consumers (web UI,
third-party automation) work off the spec instead of re-deriving the
wire format from source. Pairs with the "Web UI v1" feature.

### Snapshots + clones (VSA)
Copy-on-write at the page-cache layer. Clear fit for VSA volumes; the
VTL story (cartridge-level snapshots) is unclear and probably not
worth it given single-writer tape semantics.

### Replication
Async cross-site replication. Cloud tiering already supplies most of
the building blocks; the missing piece is a "this volume mirrors to
that backend with N-minute RPO" config surface. (VTL's cartridge-level
variant is under Parked → Cartridge replication.)

### VMware VAAI for VSA
COMPARE AND WRITE (ATS) already shipped; add WRITE SAME with UNMAP and
XCOPY. Unlocks the "VMware certified" path.

### Reference Grafana dashboards
Ship curated dashboards in `dist/grafana/` over the Prometheus metrics
both daemons already expose.

### CSI driver for Kubernetes (VSA)
A Container Storage Interface driver lets Kubernetes provision and
attach VSA volumes to pods on demand: a `PersistentVolumeClaim`
creates a VSA volume, attached to the scheduled node over
iSCSI / NVMe-TCP, with no operator in the loop. Makes VSA a
first-class storage class inside a k8s cluster.

### Source-side dedup target (VTL) — Veritas OST
We do pure target-side dedup today: the backup app sends all bytes, we
chunk on receipt. Source-side plugins instead have the backup *client*
compute fingerprints, ask the target "got these chunks?", and send
only novel ones (5–20% of original wire). Same on-disk dedup ratio
either way; the win is backup-window time and WAN cost. Realistic
target: **Veritas OST** (NetBackup) — competing source-side protocols
are vendor-proprietary and legally fenced, so OST is the one a third
party can implement. OST requires Veritas SDK integration + NetBackup
certification per release: multi-month engineering plus ongoing cert
cadence.

### Admin auth: AD / LDAP / M365 SSO
Local-only admin (Unix-socket peer-cred) is the current model. Add
directory-backed admin login — LDAP / AD bind, then SAML / OIDC for
M365 / Entra ID SSO — with RBAC roles so there's no single super-user.
(The LDAP / AD bind half is committed — see FEATURES → "Admin auth:
LDAP / Active Directory"; this entry is the SAML/OIDC + RBAC layer on
top.)

### ARM64 release builds
x86_64 only today. Add ARM64 builds for Apple Silicon dev boxes,
Ampere Altra / AmpereOne boards, and AWS Graviton. Not Raspberry
Pi 5 — too RAM-constrained for dedup + page cache.

---

## Parked — awaiting concrete request

Not declined — the case is just too weak to build speculatively. Each
entry captures the gating question and will graduate to FEATURES when
the triggering signal arrives.

### Cache warmup from cloud on startup
Optional pre-pull of recent / hot chunks at daemon boot to cut
first-read latency on a cold restart. Gated behind a config flag —
usefulness depends on workload. Skip for the local backend.

### Cross-cartridge prefetch warming (speculative)
When one tape pulls a chunk into the pool, hint that sibling tapes
sharing the same hash are likely-hot. Operates at the chunk-pool
layer, so it helps both plaintext and encrypted workloads. Revisit if
dedup hit rates on real workloads justify the prefetcher complexity.

### Speculative cloud-native optimizations
S3 Express One Zone for hot reads, CloudFront / CDN for read-heavy
workloads, Lambda for serverless manifest ops. No concrete request;
revisit only if a real deployment asks.

### Cartridge replication
Shape undecided. Possible interpretations, each with different
mechanics:
- **Cross-site DR mirror** — replicate manifest + chunks to a second
  bucket / second Thur VTL, kept in sync as new chunks seal. Closest
  primitive: bucket-level cross-region replication (already
  configurable out-of-band). App-level on top, or just document the
  bucket-level path?
- **Pair-cartridge replication (3-2-1 hygiene)** — `replicate_to:
  BARCODE` link where every WRITE on the primary also lands on the
  replica. Backup-software-friendly: the operator sees two distinct
  cartridges. Sync vs async tradeoff.
- **Active / active library** — probably overkill; tape semantics are
  single-writer.

Pick a use case before designing. Likely first: DR mirror or
compliance pairing.

### Index record layout review
Opportunistic pass over `BlockRec` (16 B per LBA) and `ChunkRec`
(64 B per chunk) for: field ordering vs pwrite locality, type widths
wider than invariants need, reserved-byte slack (27 B in `ChunkRec`
today), flag-bit consolidation so manifest-backup delta pages dirty
fewer hot pages. Not blocking; re-open if a workload makes
record-layout cost measurable.

### Streaming-write FastCDC follow-ups (priority: low)
End-to-end FastCDC default is ~592 MiB/s, already past LTO-10 native
(400 MB/s). `fixed-128` is still ~50% faster end-to-end. Closing the
gap needs either a SIMD batched Gear chain (needs AVX-512 to win
consistently — bit-exact reference tests are in place) or
worker-thread off-load (parking near AES-GCM and BLAKE3 — re-profile
under realistic load before committing). `examples/perf_chunker.rs` +
`examples/perf_write.rs` kept as regression checks.

### DEK rotation (VSA + VTL re-encryption)
Per-volume / per-cartridge DEK rotation means re-encrypting every
chunk. The case is unusually weak:

- **Compromise response** — doesn't help. If the DEK leaked, the
  attacker already has ciphertext + key; rotating tomorrow doesn't
  un-leak yesterday's bytes.
- **KEK rotation** — already a no-op for VSA. We don't pin a
  `key_version`, so the next wrap call uses the KMS / Vault current
  primary. Azure KV pinning is the explicit-opt-out path.
- **Operator wants a different wrap target** — handled by
  `thurvsa volume key migrate`, no chunk rewrite.
- **AES-GCM key-lifetime ceiling** — at 64 KiB pages with random IVs,
  the NIST-recommended ~2³² messages per key works out to >256 TiB
  *per volume*, well above any realistic single-volume corpus.
- **VTL specifically** — LTO AME is host-driven via SPOUT page `0x10`;
  the daemon never persists the DEK. There is no daemon-side DEK to
  rotate.

Triggering signal: a regulation or compliance audit that *explicitly
mandates* periodic re-encryption of stored ciphertext on a fixed
schedule — not just key rotation, but rewriting ciphertext under a new
key. Until then, the wrap-target migrate verb answers every
operational ask here.

### KMIP integration for VTL
LTO tape encryption is host-driven by spec: the backup app fetches the
DEK from a KMIP server and pushes it to the drive via SPOUT page
`0x10`. VTL implements that path today (session-scoped, never
persisted) — the canonical model already works.

A "thurvtl daemon talks KMIP itself" mode would only help an operator
with KMIP infrastructure whose backup software doesn't speak KMIP.
Even then, the daemon would have to invent policy for *which key
applies to which write* (per-barcode? per-partition? library-wide?),
since the host stops signalling via SPOUT — a new policy surface, not
just a codec call.

The TTLV codec already lives in `shared/keystore/src/kmip_wire.rs`
from the VSA work, so the protocol lift is small. The gating question
is policy, not protocol.

Triggering signal: a deployment that names KMIP-managed tape keys as a
hard requirement *and* a concrete answer to the key-selection policy
question.

### SCSI surface gaps (drive LUN ≥ 1)
Drive-LUN coverage against the SSC-4 / LTO-7/8 surface is otherwise
complete. Residual gaps, none host-visible today:

- **PROUT (Persistent Reservations Out)** — deferred deliberate
  divergence. Thur VTL is single-initiator-per-LUN by construction, so
  PR machinery is overhead without functional payoff.
- **Media Initialization** — cosmetic, deferred.

### Release packaging follow-ups
None blocking; pick up on request.
- **Hosted apt / yum repo** — declined for alpha. A public repo is its
  own supply-chain attack surface; revisit when an operator asks for
  apt / dnf repo access.
- **Reproducible builds** — today's chain proves "maintainer signed
  these bytes" but operators can't independently verify "binary came
  from this source." Byte-identical rebuilds require pinning every
  build tool (rustc pinned via `rust-toolchain.toml`;
  cargo-deb / cargo-generate-rpm are `cargo install --locked` but
  `--locked` doesn't pin transitive `cargo install` deps), stripping
  non-deterministic metadata (`SOURCE_DATE_EPOCH`, build IDs, archive
  timestamps, `--remap-path-prefix`), and verifying byte-equality
  across two independent builder containers. 1-2 days of engineering
  plus per-dep-bump vigilance.
- **Signed CHANGELOG / SBOM** — CycloneDX SBOM via `cargo-cyclonedx`
  is a 1-line addition to `release/release.sh` on request.
- **arm64 builds** — x86_64 only today. Re-open on request; the ARM64
  release builds idea above already names the Apple Silicon / Ampere /
  Graviton use case (not RPi — too RAM-constrained for dedup + page
  cache).

---

## Considered, declined

Decisions captured to prevent re-litigation. Re-open only if the
underlying assumption changes (named in each entry).

### LTO-9 emulation
Validator caps at LTO-8 (`== 8`); 1.0.0 GA ships without LTO-7
cartridge creation or LTO-9. REPORT DENSITY SUPPORT still advertises
the LTO-8 + LTO-7 RO pair for real-drive parity. Full reasoning + the
work to add it (lookup tables, RAO opcodes, Initial Capacity Scaling,
SSC-5 conformance shift, the LTO-8 → LTO-9 cartridge-convert verb) in
[`docs/LTO-9.md`](docs/LTO-9.md). Re-open when someone asks for it
with a concrete reason (procurement spec, RAO-aware workflow,
certification matrix).

### WORM tamper detection (WTRE / EOPD)
LTO-7+ defines a WORM-cartridge tamper-detection model: the drive
maintains a Medium Identifier Cartridge (MIC) record on the tape;
Mode Page `0x10/0x01` carries WTRE (WORM Tamper Read Enable) and EOPD
(Erase-on-Permanent-Defect) bits that gate how the drive reacts to
MIC vs data-layout inconsistency.

Thur VTL already enforces "no overwrite of committed WORM data" at the
cartridge layer — the integrity guarantee operators actually care
about. WTRE / EOPD covers the residual case where someone removes a
physical WORM cartridge from a real LTO drive, modifies it on a
non-WORM-aware drive, and reinserts it. That attack surface doesn't
exist in a virtual library: there's no out-of-band write path.

Re-open only if a backup product explicitly checks Mode Page
`0x10/0x01` WTRE / EOPD bits and refuses the medium on mismatch.

### Byte-level FastCDC (full shift invariance)
The current FastCDC implementation is page-aligned: chunk boundaries
fall on byte positions inside the feed stream, but per-feed windowing
means inserting one byte at the start of a stream shifts *all*
subsequent boundaries.

Full shift invariance — where inserting / deleting bytes only
re-chunks the affected window — would require a content-defined
rolling hash applied byte-by-byte across the entire stream, not just
within each feed. The change is invasive (touches every feed call
site) and the dedup payoff is marginal: backup workloads append, they
don't insert. Synthetic-fulls of mutating filesystems are already
handled well by the page-aligned model in practice.

Re-open only if benchmarks against a real synthetic-full workload show
dedup ratios materially below what backup-software-side dedup achieves
on the same data.
