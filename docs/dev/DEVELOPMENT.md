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
`test-pipeline-layers.sh`, `test-lifecycle-cartridge-migrate.sh`, `test-keystore.sh`,
the two legal-hold suites (`test-legal-hold-lifecycle.sh`,
`test-tiering-legal-hold-interaction.sh`), and `test-monte-carlo.sh`
against a non-local backend) exercise real
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

### Legal-hold suites: Object-Lock bucket + IAM

`test-legal-hold-lifecycle.sh` and `test-tiering-legal-hold-interaction.sh`
carry a requirement the other real-backend suites don't, and it trips
people up because S3 overloads the word "retention." Three *independent*
things live under S3 Object Lock:

- **Object Lock** — the bucket-level master switch. Turning it on does not,
  by itself, make anything immutable.
- **Retention** (WORM) — "no delete or overwrite until date T", in
  GOVERNANCE or COMPLIANCE mode. Applied per object, or as a bucket-wide
  *default retention rule*. This is what the conffile's `retention_mode:`
  maps to.
- **Legal hold** — a per-object on/off flag that blocks deletion until it
  is flipped off. Needs Object Lock *enabled*; needs no retention.

The legal-hold suites exercise the third thing, so the backend is declared
`retention_mode: none` (we are not testing WORM) while still sitting on an
Object-Lock-enabled bucket (so `PutObjectLegalHold` works). That
combination only holds on a bucket with **Object Lock enabled and NO
default retention rule**:

```bash
aws s3api create-bucket \
  --bucket <name> --region <region> \
  --create-bucket-configuration LocationConstraint=<region> \
  --object-lock-enabled-for-bucket   # and DO NOT follow with
                                     # put-object-lock-configuration —
                                     # that is what adds a default rule
```

The daemon reports a bucket as "locked" only when it finds a default
retention *rule* (`shared/object-store/src/s3.rs::lock_state`), so an
Object-Lock bucket with no default rule reads back as `Off` — which matches
the declared `retention_mode: none` and lets the daemon start. Add a
default retention rule and two things break: (1) the daemon's startup
lock-state check sees `none`-vs-locked and refuses to start (the safety net
for "you declared none but the bucket is actually WORM"), and (2) every
written object is auto-stamped with retention, so the test can't delete its
own data afterward (cleanup is best-effort and warns about the debris).

IAM matters too, and policies here are scoped *per bucket* — being able to
reach an existing WORM bucket does **not** carry over to a brand-new
legal-hold bucket, which must be added to the principal's policy
explicitly. On that bucket the test principal needs:

- `s3:ListBucket` (bucket ARN); `s3:GetObject` / `s3:PutObject` /
  `s3:DeleteObject` (object ARN) — the daemon's upload / download / cleanup.
- `s3:GetObjectLegalHold` / `s3:PutObjectLegalHold` (object ARN) — the hold
  read/set the suites exercise.
- `s3:GetBucketObjectLockConfiguration` (bucket ARN) — recommended but
  optional: without it the startup lock-state check can't verify and
  downgrades to a warning (the daemon still starts).

Object-Lock buckets are versioned, so a `rm`-style purge leaves noncurrent
versions behind; a thorough cleanup also wants `s3:ListBucketVersions` +
`s3:DeleteObjectVersion`. A quick smoke that the principal is wired up:
`aws s3api head-bucket --bucket <name>` should not 403.

Optional: `private/thur.env` (KEY=VAL per line). Every script gates the
source on `[[ -r ... ]]` and auto-loads it under `set -a` before
self-elevation. Use it to persist credentials across shells; skip it if
your shell already exports them from your dotfiles or a credential
manager.

`private/` is gitignored — it carries live storage credentials and bucket
coordinates. Override paths with `THURV{TL,SA}_SOURCE_BACKENDS` /
`THURVSA_SOURCE_KEYSTORES` if your fixtures live elsewhere. Pick a
backend per run with `THURV{TL,SA}_TEST_BACKEND=<name>` matching an entry
in the YAML. All non-real-backend tests (`test-smoke.sh`,
`test-*-conformance.sh`, `test-backup-workflow.sh`,
`test-fs.sh`) run against an
inline local backend and need no `private/` setup.

### S3 IAM policy (AWS)

The S3-backed tests all run as one IAM principal (the `AWS_*` creds
above). The daemon and the test scripts together touch a small, fixed
set of S3 operations; the policy below is the complete least-privilege
set. Scope the resource ARNs to whichever bucket(s) your
`private/storage-backends.yaml` points at — one `…:::bucket` ARN for the
bucket-level actions and one `…:::bucket/*` for the object-level ones.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "ThurTestsBucket",
      "Effect": "Allow",
      "Action": [
        "s3:ListBucket",
        "s3:ListBucketVersions",
        "s3:GetBucketObjectLockConfiguration"
      ],
      "Resource": ["arn:aws:s3:::your-test-bucket"]
    },
    {
      "Sid": "ThurTestsObjects",
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:DeleteObjectVersion",
        "s3:GetObjectLegalHold",
        "s3:PutObjectLegalHold",
        "s3:BypassGovernanceRetention"
      ],
      "Resource": ["arn:aws:s3:::your-test-bucket/*"]
    }
  ]
}
```

What each action is for:

| Action | Level | Needed by |
|--------|-------|-----------|
| `s3:ListBucket` | bucket | the daemon's `list_objects` (manifest assembly, `storage check`) and the scripts' `aws s3 ls` assertions. Also authorizes `HeadBucket`, which `verify_storage_creds` uses as a pre-flight (there is no separate `s3:HeadBucket` action). |
| `s3:GetBucketObjectLockConfiguration` | bucket | the daemon probes each backend's Object-Lock state at startup to validate `retention_mode` — on every S3 backend, regardless of mode. |
| `s3:GetObject` | object | `download_chunk` / `download_manifest`, the `chunk_exists` `HeadObject` probe (HeadObject is authorized by `s3:GetObject`, not a distinct action), and the scripts' `aws s3 cp` ciphertext checks. |
| `s3:PutObject` | object | chunk and manifest uploads. |
| `s3:DeleteObject` | object | GC / eviction, the `storage check` data-plane probe, and the scripts' `aws s3 rm` prefix cleanup. |
| `s3:GetObjectLegalHold` | object | read at **cartridge load** (the daemon snapshots hold state from the storage sentinel) and by `system tiering plan` / `run-now` — so it's needed by any test that loads a cartridge off S3, not just the legal-hold tests. |
| `s3:PutObjectLegalHold` | object | `cartridge legal-hold set` / `clear`. |
| `s3:ListBucketVersions`, `s3:DeleteObjectVersion`, `s3:BypassGovernanceRetention` | bucket / object / object | **cleanup only** — not used by the daemon or the test scripts. They let you purge leftover objects from an Object-Lock (governance) test bucket before the default retention window expires (a versioned `aws s3 rm` only writes delete-markers; the locked versions need a version-targeted delete with governance bypass). Drop them if you never test against a governance/compliance bucket. Note: `BypassGovernanceRetention` only overrides GOVERNANCE mode — COMPLIANCE-locked objects cannot be bypassed by anyone until expiry.

Deliberately **not** in the policy, because the backend never calls them:
multipart upload (`s3:AbortMultipartUpload` and friends — each chunk is a
single `PutObject`), `s3:CopyObject`, object tagging, presigned URLs, and
per-object retention (`s3:GetObjectRetention` / `s3:PutObjectRetention` —
WORM rides the bucket's *default* retention rule, set out of band, not a
per-object retention call).

The Object-Lock and legal-hold actions only bite on buckets that actually
have Object Lock enabled, but `s3:GetBucketObjectLockConfiguration` and
`s3:GetObjectLegalHold` are still needed against a plain
`retention_mode: none` bucket — the startup lock probe runs everywhere
(and reads back "off"), and the cartridge-load hold read runs on every
load (and reads back "not held"). GCS and Azure backends authorize through
their own IAM / RBAC rather than this JSON; grant the equivalent object
read/write/delete there, plus the object-lock / legal-hold roles if you
run the hold tests against them.

The release-cut process is in [`RELEASING.md`](RELEASING.md); the
workspace crate map is in [`WORKSPACE.md`](WORKSPACE.md).
