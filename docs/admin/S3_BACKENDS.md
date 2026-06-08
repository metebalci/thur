# S3-compatible backend matrix

This document surveys the S3-compatible object-storage providers that
can be wired into the daemon as a `storage.backends` entry of type `s3`,
typically with an `endpoint_url` and region override. The data here
was gathered in May 2026, and provider feature sets move — re-verify
anything load-bearing before relying on it in production. The scope is
deliberately the providers *beyond* AWS S3 and MinIO, since those two
are already covered elsewhere.

The reason a survey is needed at all is that "S3-compatible" is a
marketing claim, and it does not survive contact with WORM cartridges
or Legal Hold. The table below grades each provider against the
specific features VTL actually depends on, rather than against the
broad compatibility claim.

## TL;DR

| Provider          | SigV4 | Object Lock | Legal Hold      | Notes                                                                    |
| ----------------- | ----- | ----------- | --------------- | ------------------------------------------------------------------------ |
| AWS S3            | Yes   | Gov + Comp  | Native          | Reference. Already supported.                                            |
| MinIO             | Yes   | Gov + Comp  | Native          | Dev default; on-prem option. Already supported.                          |
| Backblaze B2      | Yes   | Gov + Comp  | Native          | Full feature parity.                                                     |
| Wasabi            | Yes   | Comp only*  | Native          | 90-day minimum-storage-duration billing trap; no governance mode.        |
| Hetzner           | Yes   | Yes         | Native          | EU only. Must enable Object Lock at bucket creation.                     |
| OVHcloud          | Yes   | Gov + Comp  | Native          | EU/CA; "Standard" class only — Cold/High-Perf classes lack Object Lock.  |
| IONOS             | Yes   | Gov + Comp  | Native          | EU. Object Lock auto-enables versioning; not retrofittable.              |
| Exoscale SOS      | Yes   | Yes         | Native          | EU/CH. Object Lock free; retention semantics light on docs.              |
| Cloudflare R2     | Yes   | Bucket-lock | None (as of survey) | Bucket-level lock, not per-object; **not** S3 Object Lock semantics. Legal Hold absent — **fails the WORM + legal-hold gates**. |

\* Wasabi names its lock mode "Compliance". Its API surface is the SPC
`COMPLIANCE` retention mode plus Legal Hold — and that is all. There is
no `GOVERNANCE` escape hatch, so a locked object cannot be deleted
until its retention period expires.

## What Thur VTL needs from the backend

The requirements below are drawn from the code that actually talks to
the backend — `shared/object-store/src/s3.rs`,
`core/mediachanger/src/legal_hold.rs`, and the WORM cartridge gate in
`core/stream/src/cartridge/mod.rs`. Six features matter:

1. **SigV4 auth.** SigV2 and older signatures are unsupported, but this
   is not a real constraint — SigV4 is universal. The per-provider
   variations in bucket sub-resource URLs are absorbed by the existing
   backend's `endpoint_url` plus region rewriting.
2. **PUT / GET / HEAD / DELETE / LIST.** This is the baseline object
   API, and every provider in the survey has it.
3. **Multipart upload.** Required for any chunk of about 5 MiB or
   larger. VTL's default 8 MiB FastCDC chunk size crosses that
   threshold immediately, so multipart is not optional in practice.
4. **`PutObjectLegalHold` + `GetObjectLegalHold`.** Needed the moment
   an operator issues `cartridge legal-hold set`. The
   `manifest-latest.json` sentinel depends on these being atomic: the
   set walks chunks, then index pages, then the sentinel last, and
   `legal-hold status` by default reads only the sentinel.
5. **Object Lock (governance or compliance).** Required for WORM
   cartridges. The daemon refuses to start if the `retention_mode`
   declared in the conffile does not match the bucket's actual lock
   state.
6. **HEAD-before-PUT atomicity.** The dedup probe issues a HEAD before
   uploading. Under high concurrency, a weakly-consistent provider
   could answer "not found" for an object that is mid-upload from
   another worker, which would let two PUTs race. AWS, GCS, and Azure
   give read-your-writes consistency for HEAD; smaller providers may
   not.

A few capabilities are *not* needed by VTL, but still bear on the
choice of backend because they can quietly break it:

- Glacier-style tiering and minimum-retention "delete penalty" tiers
  break the GC model — a migrated tape incurs the penalty. Stay on the
  always-warm tier.
- Per-region replication is opt-in at the bucket level and configured
  out-of-band. The daemon does not care about it either way; an
  operator who wants cross-region DR configures it on the bucket
  themselves.
- "Cold" storage classes that require a restore-before-read step
  violate the read path's latency assumptions.

## Per-provider notes

### Backblaze B2

Backblaze B2 reaches full feature parity with AWS for VTL's purposes.

- The S3 endpoint is a per-region variant such as
  `https://s3.us-west-001.backblazeb2.com`; B2's own native API
  endpoint is separate.
- Object Lock is the full S3 API, including both `governance` and
  `compliance` modes, with `s3:PutObjectLegalHold` at parity. It must
  be enabled at bucket creation.
- Multipart uploads take a 5 MB minimum part and a 5 GiB maximum, and
  objects top out at 10 TB.
- Regions available: US-West, US-East, and EU-Central.
- The upshot is full WORM and legal-hold parity.

### Wasabi

Wasabi works, but with two caveats — one on lock modes and one on
billing.

- The S3 endpoint follows `https://s3.<region>.wasabisys.com`.
- Object Lock exists in compliance mode only — surfaced as
  "Immutability: Compliance" in the UI — and Legal Hold works through
  the standard `PutObjectLegalHold`. There is **no governance mode**,
  so a deployment that needs `retention_mode: governance` rules Wasabi
  out.
- The billing trap is a **90-day minimum storage duration on
  Pay-as-you-go, or 30-day on Reserved**. Deleting a chunk before that
  window still bills for the remaining days. That pairs poorly with
  cartridge ERASE / FORMAT MEDIUM workflows and with `system gc`
  evicting unreferenced chunks, so size the disk-cache and dedup scope
  to keep chunk churn down.
- Regions: US-East, US-Central, US-West, EU-Central, AP-Northeast, and
  AP-Southeast.

### Hetzner Object Storage

Hetzner is a solid EU-residency option.

- The S3 endpoint follows `https://<region>.your-objectstorage.com`,
  with regions such as `fsn1`, `nbg1`, and `hel1`.
- Object Lock is the full S3 API and must be enabled at bucket creation
  — there is no retrofit. Legal Hold works through the standard
  `PutObjectLegalHold`.
- It is EU-only, in Germany and Finland, which makes it a fit for
  GDPR-bound deployments that want EU data residency.
- Multipart is standard S3, with a 5 MB minimum part.

### OVHcloud Object Storage

OVHcloud works for WORM, but only if the bucket is pinned to the right
storage class.

- The S3 endpoint follows `https://s3.<region>.io.cloud.ovh.<tld>`,
  where the TLD varies between `.us`, `.net`, and `.eu`.
- Object Lock supports both governance and compliance modes via
  `aws s3api put-object-retention`, and Legal Hold is at parity.
- **The class caveat is the catch:** Object Lock is available on the
  "Standard" class *only*. The "High Performance" and "Cold Archive"
  classes do NOT support it, so a bucket holding WORM cartridges must
  be pinned to Standard.
- Regions: US (BHS, VIN), CA, multiple EU regions, and APAC.

### IONOS S3 Object Storage

IONOS is another EU-only option with full WORM support.

- The S3 endpoint follows `https://s3.eu-central-3.ionoscloud.com`,
  among other EU regions.
- Object Lock supports both governance and compliance modes, with
  Legal Hold at parity.
- Enabling Object Lock auto-enables versioning, and that is
  irreversible at the bucket level — Object Lock cannot be retrofitted
  onto a bucket that was created without it.
- It is EU-only.

### Exoscale SOS

Exoscale SOS has the features but thinner documentation, so verify
before committing.

- The S3 endpoint follows `https://sos-<region>.exo.io`, with regions
  such as `ch-gva-2`, `ch-dk-2`, and `de-fra-1`.
- Object Lock is present and Legal Hold is supported. The
  documentation is lighter on retention-mode specifics than AWS, B2, or
  Wasabi, so confirm the exact `retention_mode: compliance` semantics
  against a test bucket before relying on them.
- Regions: Switzerland, Germany, Austria, and Bulgaria — EU/CH
  residency.

### MinIO

MinIO is already documented in the codebase as a dev and on-prem
target. It offers the full S3 API, including Object Lock in both
governance and compliance modes and Legal Hold. The access key needs
the `s3:PutObjectLegalHold` permission. For development, the
`scripts/docker-compose.yml` spins up a single-node MinIO; production
deployments run an operator-managed multi-node distributed-MinIO
setup.

### Cloudflare R2 — does **not** clear the bar

R2 is the one provider in the survey that fails the feature gate, and
it fails on the compliance features specifically.

- The S3 endpoint follows `https://<account-id>.r2.cloudflarestorage.com`.
- R2 has "bucket locks", but they operate at the bucket or prefix level
  with a retention duration. **Per-object S3 Object Lock semantics are
  not provided.**
- **Legal Hold is absent** — there is no `PutObjectLegalHold`
  equivalent.
- The consequence is that WORM cartridges (`retention_mode: governance`
  or `compliance`) and `cartridge legal-hold set` are both unusable on
  R2. An operator wiring R2 must keep cartridges off WORM and must
  never invoke legal-hold, because the daemon's startup retention-mode
  reconcile refuses on any mismatch.
- The verdict: R2 is fine for *non-compliance* cartridges — no WORM, no
  legal-hold — and against `legal_hold` and `retention_mode` it should
  refuse the same way the `local` backend does.

## Implementation hooks needed

For every provider that passes the feature gate — that is, all of them
except R2 — the integration work is light:

- Basic read/write needs no code changes beyond setting `endpoint_url`
  and the region. The existing `S3Backend` in `shared/object-store/src/s3.rs`
  already absorbs the variant endpoint shapes through `endpoint_url`.
- The WORM `retention_mode` reconcile already runs at startup, in
  `shared/object-store/src/s3.rs` via `GetObjectLockConfiguration`, and it
  should work against any provider whose Object Lock surface mirrors
  AWS. Still, spot-test each one — some providers return slightly
  different XML for `GetObjectLockConfiguration`.
- The Legal Hold path in `core/mediachanger/src/legal_hold.rs` uses
  the raw S3 `PutObjectLegalHold`, which works on every listed provider
  except R2.
- The HEAD-before-PUT dedup probe — `upload_chunk_inert` in
  `shared/upload-worker/src/inert.rs` — depends on read-your-writes
  consistency. AWS, GCS, Azure, B2, and Hetzner advertise that
  guarantee; Wasabi and OVH claim strong consistency. Either way, it is
  worth spot-testing under concurrency.

R2 would need real code, and only if demand materializes:

- Add an `r2` backend variant (or extend `s3` with a `flavor: r2`
  flag) that refuses `retention_mode != none` at config-load time,
  refuses `legal-hold set / clear`, and returns a specific error that
  points the operator at this matrix.

## Provider-specific gotchas

Pulling the per-provider catches into one place:

- **AWS, B2, Hetzner, IONOS, OVHcloud, Exoscale:** Object Lock must be
  enabled at bucket-creation time. Retrofitting it onto an existing
  bucket means creating a new bucket and copying.
- **Wasabi:** the 90-day (Pay-as-you-go) / 30-day (Reserved)
  minimum-storage-duration billing means a high-churn workload runs a
  permanent ghost-bill for objects it has already deleted. Stress-test
  cartridge churn before committing.
- **OVH Cold Archive class:** its restore-before-read latency is
  incompatible with the always-warm read assumption, so pin the bucket
  to the Standard class.
- **Hetzner:** EU-only, with no US region option.
- **R2:** Object Lock is bucket-level rather than per-object, and Legal
  Hold is absent — so refuse both features against it.

## Open follow-ups

A few loose ends remain:

- `test-backup-storage` accepts a `THURVTL_TEST_BACKEND` env var that
  picks a conffile entry. The cheapest way to validate this matrix
  against running infrastructure is to add per-provider smoke targets —
  one entry per provider in a development `thurvtl.yaml` — and run the
  existing backup-storage cases against each.
- The `s3` backend already handles the AWS, MinIO, and Wasabi endpoint
  overrides, and the other providers should slot in with `endpoint_url`
  alone. Confirm each provider's SigV4 region-string quirks during the
  smoke run.
- If R2 support is requested, decide whether the right shape is a
  separate `r2` backend variant or a `flavor: r2` flag on `s3`. Either
  way, the Object-Lock and Legal-Hold refusal logic is the same.

## Sources

- [Hetzner Object Storage — Legal Hold](https://docs.hetzner.com/storage/object-storage/howto-protect-objects/protect-object-lock-legal-hold/)
- [Backblaze B2 — Object Lock](https://www.backblaze.com/docs/cloud-storage-object-lock)
- [Backblaze B2 — Enable Object Lock via S3 API](https://www.backblaze.com/docs/cloud-storage-enable-object-lock-with-the-s3-compatible-api)
- [Wasabi — Object Lock with the S3 API](https://docs.wasabi.com/apidocs/object-lock-with-the-wasabi-s3-api)
- [Wasabi — Minimum Storage Duration Policy](https://docs.wasabi.com/docs/how-does-wasabis-minimum-storage-duration-policy-work)
- [OVHcloud — Managing Object Immutability with Object Lock (WORM)](https://help.ovhcloud.com/csm/en-public-cloud-storage-s3-managing-object-lock?id=kb_article_view&sysparm_article=KB0047399)
- [IONOS — Object Lock](https://docs.ionos.com/cloud/storage-and-backup/ionos-object-storage/settings/object-lock)
- [Exoscale — Simple Object Storage Overview](https://community.exoscale.com/community/storage/simple-object-storage-overview/)
- [MinIO — Object Locking and Immutability](https://docs.min.io/enterprise/aistor-object-store/administration/object-locking-and-immutability/)
- [Cloudflare R2 — Bucket Locks](https://developers.cloudflare.com/r2/buckets/bucket-locks/)
- [Cloudflare R2 — S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/)
