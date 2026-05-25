# Cartridge Lifecycle

This document is the design and behavior reference for cartridge creation,
WORM enforcement, and legal hold. The CLI flags are documented in the
binary's `--help` output. Cross-backend and cross-region operations
(`cartridge migrate`, `cartridge archive`, `library restore-archive`,
`library restore`) are covered in [`SPEC.md`](SPEC.md) §§ Cartridge
migration / Cartridge archive / Restore-archive / Cross-region DR.

## Pre-creation required

Unlike traditional tape drives, the daemon does not allow backup software to
create cartridges on demand. The operator must pre-provision each cartridge
before backup software can load it:

```
thurvtl cartridge create BARCODE \
  [--lto-generation N] [--chunk-size-mb MB] \
  [--chunking fixed|fastcdc] [--multi N] \
  [--backend NAME] [--dedup local|global] [--worm]
```

The LTO generation defaults to the library's `lto_generation`, but can be
overridden per-cartridge to stage a different generation. Capacities are:
LTO-7 6 TB, LTO-8 12 TB. A write that reaches the generation cap fails with
"medium full."

### `--multi N`

Increments the trailing decimal-digit suffix while preserving
zero-padded width (`TAPE001 --multi 4` → `TAPE001..TAPE004`). Errors
before any creation if the barcode lacks a numeric suffix, the suffix
would overflow its width, or there aren't enough free slots.

### `--backend NAME`

Binds the cartridge to a named entry in `storage.backends`. Required
when ≥2 backends are configured; optional (and inferred) with exactly
one. The chosen name is sticky for life — every chunk upload, manifest
backup, prefetch, and refetch routes through this backend.

### `--dedup local|global`

`global` (the default, configurable via `cli.cartridge_dedup`) puts
the cartridge in the shared per-backend pool — cross-cartridge dedup
fires. `local` namespaces every chunk under the cartridge's barcode —
no sharing across cartridges (compliance / tenant isolation,
per-cartridge cleanup). Sticky for life; both modes content-address by
BLAKE3, only the scope of sharing differs. Full model in
[`DEDUP.md`](DEDUP.md).

## WORM

`--worm` makes the cartridge **WORM** (Write Once Read Many): writes
are only allowed at end-of-data; ERASE / FORMAT MEDIUM / ALLOW
OVERWRITE are refused outright. Sticky for life.

The chosen backend must have `retention_mode: governance` (mutable) or
`retention_mode: compliance` (irrevocable) — the bucket-level Object
Lock / retention policy enforces WORM storage-side.

Enforcement layers:

- **Cartridge layer** (`cartridge.rs`): WORM-aware refusal in
  `write_data` / `write_filemark` / `erase` / `apply_format_medium` /
  `set_allow_overwrite` → `SmcError::WormViolation` → SCSI CHECK
  CONDITION + DATA PROTECT (key 0x07) + ASC/ASCQ 0x30/0x0C
  ("WRITE PROTECTED — WORM MEDIUM").
- **SCSI surface**: WORMM bit in INQUIRY VPD page 0xB0
  (Sequential-Access Device Characteristics) reflecting the loaded
  cartridge.
- **Cloud layer**: `retention_mode != none` triggers Object Lock /
  retention policy on the bucket; the bucket auto-applies retention to
  every PUT (the daemon never sets per-object retention — too
  divergent across providers). The daemon validates the configured
  `retention_mode` against the bucket's actual lock state at startup,
  in both directions.

### Provider bucket setup

A WORM cartridge needs a backend whose bucket has provider-side
immutability turned on. That is **two layers** on every cloud — enable
the feature, *then* set a default retention rule. Skipping the second
leaves "lock enabled, no rule"; the daemon reports that as `Off`, and a
`retention_mode: governance` config then fails to start.

**AWS S3** — Object Lock can only be enabled at bucket creation:

```bash
aws s3api create-bucket --bucket your-bucket-worm --region us-east-1 \
  --object-lock-enabled-for-bucket
aws s3api put-object-lock-configuration --bucket your-bucket-worm \
  --object-lock-configuration '{"ObjectLockEnabled":"Enabled",
    "Rule":{"DefaultRetention":{"Mode":"GOVERNANCE","Days":2555}}}'
```

`GOVERNANCE` → `COMPLIANCE` for irrevocable retention. AWS minimum is
`Days: 1`. Versioning auto-enables with Object Lock; leave it on.

**GCS** — a bucket-level retention policy:

```bash
gcloud storage buckets update gs://your-bucket-worm --retention-period=2555d
# optionally lock it (irrevocable, equivalent to compliance):
gcloud storage buckets update gs://your-bucket-worm --lock-retention-policy
```

**Azure** — container-level immutability inside a storage account.
Leave **Hierarchical namespace OFF** when creating the account (ADLS
Gen2 is a different API surface the daemon doesn't speak):

```bash
az storage container immutability-policy create \
  --account-name your-storage-account --container-name worm \
  --resource-group your-rg --period 2555      # days
```

Azure WORM additionally needs `subscription_id` + `resource_group` on
the backend config and AAD auth — the immutability policy lives on the
ARM management plane, which SAS credentials can't reach.

**IAM grants.** Every cloud-backed cartridge needs object
read/write/delete + bucket list on its backend — `s3:PutObject` /
`s3:GetObject` / `s3:DeleteObject` / `s3:ListBucket`, or
`roles/storage.objectAdmin` on GCS, or **Storage Blob Data
Contributor** on Azure. WORM adds the management-plane read the
boot-time check uses (`s3:GetBucketObjectLockConfiguration`, or
**Storage Account Contributor** on Azure). Legal hold needs more again
— see § Legal hold → Permissions.

If the environment can't grant the management-plane read, set
`storage.skip_retention_mode_check: true` to skip the boot-time
lock-state probe. `retention_mode` still parses and `--worm` still
refuses `retention_mode: none` backends, but you lose the safety net
that catches "declared WORM, never actually locked the bucket" — so
verify the bucket policy out of band.

## Legal hold

`thurvtl cartridge legal-hold set|clear|status BARCODE` is a thin
wrapper over the cloud provider's per-object hold primitive:

- S3 `PutObjectLegalHold`
- GCS `eventBasedHold`
- Azure `Set Blob Legal Hold` (raw REST — `azure_storage_blobs`
  doesn't expose it)

The storage backend is the source of truth for which keys are held; the
daemon keeps no on-disk "is held" flag in the manifest. `set` walks
every chunk + manifest backup + index page the cartridge references on
its bound backend and applies the primitive.

The `manifests/<barcode>/manifest-latest.json` key is the
**sentinel**:

- `set` applies hold to every chunk + versioned manifest backup +
  index page *first* and the sentinel *last*.
- `clear` releases the sentinel *first* and the body *after*.

This ordering makes `manifest-latest.json` a definitive runtime answer
to "is this cartridge held?" — `legal-hold status` reads it in one
round-trip; `--full` sweeps every key to verify the body matches.

Refused against the local backend.

The audit log records every set/clear with operator identity, optional
`--id` label, optional `--reason`, and per-key success/fail counts.

Implementation: `core/stream/src/legal_hold.rs` (orchestration —
`apply_cartridge_legal_hold`, `read_cartridge_held`,
`manifest_latest_sentinel_key`) + `find_drive_for_loaded_cartridge` in
`core/mediachanger/src/legal_hold.rs` + per-backend
`set_object_legal_hold` / `get_object_legal_hold` on `ObjectStoreBackend`.

### Host-visible write-protect

The daemon reads the sentinel once at drive-load time (iSCSI MOVE
MEDIUM 0xA5 post-hook in `protocol.rs`) and stamps a volatile
`legal_held` flag on the loaded `Cartridge`.

While the flag is set, the five host write opcodes (`write_data`,
`write_filemark`, `erase`, `apply_format_medium`, `set_allow_overwrite`)
return `SmcError::LegalHoldViolation` → SCSI CHECK CONDITION + DATA
PROTECT (key 0x07) + ASC/ASCQ 0x27/0x00 ("WRITE PROTECTED") — plain
code, not the WORM-specific 0x30/0x0C, since this is operator-applied
preservation rather than sticky-at-create write-once semantics.

The flag is volatile (never persisted), cleared on UNLOAD via `Drop`;
the next load re-reads the sentinel.

### Hold-while-loaded refusal

To keep the load-time snapshot coherent for the cartridge's in-memory
lifetime, `legal-hold set` and `legal-hold clear` refuse if the
cartridge is currently in any drive. The CLI reads
`<data_dir>/library/inventory.json` via
`find_drive_for_loaded_cartridge` and exits non-zero with an "unload
from drive N first" message; the audit log records the refusal.

WORM semantics are unchanged: hold on a WORM cartridge is allowed
(storage-side preservation past Object Lock retention is a real use
case), but the WORM SCSI gate fires first so the volatile legal-hold
flag never reaches the write path on WORM.

### Auto-hold on upload

The event-driven upload worker calls `read_cartridge_held` once per
upload request; if the cartridge is held it re-applies the per-object
hold to every freshly-PUT chunk, every freshly-PUT index-page object
(`manifests/<barcode>/<label>/page-<NNNNNN>.dat`), and the new manifest
backup objects (versioned key + refreshed `manifest-latest` sentinel,
sentinel-last to mirror the explicit `set` ordering).

With hold-while-loaded refusal in place this worker is a safety net for
the residual race: an operator flipping the bucket-level hold via
`aws-cli` *while the cartridge is loaded* (the daemon won't see it
until the next unload+reload).

A held cartridge whose sentinel doesn't yet exist (first upload) is
treated as not-held; hold-application failures on individual objects
are logged but do not fail the upload.

### Permissions

- **S3** — Object-Lock-enabled bucket + IAM `s3:PutObjectLegalHold` /
  `s3:GetObjectLegalHold`.
- **GCS** — `storage.objects.update` / `storage.objects.get`
  (`roles/storage.objectAdmin`).
- **Azure** — immutable-storage container + AAD auth + `Storage Blob
  Data Owner` role (Contributor is *not* enough; SAS auth can't mint
  AAD tokens for the data plane, and storage-account shared-key auth
  was dropped 2026-05-10 alongside the migration to Microsoft's
  official `azure_storage_blob` SDK).

## At-rest encryption (appliance-side)

Opt-in per-cartridge AES-256-GCM. Independent of host-driven AME
(SSC-4 SECURITY PROTOCOL OUT, key supplied by the backup app). With
both on, AME runs per-block at write time and the appliance-side layer
wraps the entire sealed chunk — modest CPU cost,
zero-plaintext-at-rest even if the backup app forgets to enable AME.

The per-cartridge DEK is wrapped by an entry under `keystore.backends:`
in the YAML conffile. Six backends ship (`local`, `awskms`, `vault`,
`azurekv`, `gcpkms`, `kmip`) — the same set VSA uses. See
[`AUTH.md`](AUTH.md) § *VTL keystore backends* for the keystore
lifecycle, AME composition rules, and the dedup tradeoff
(cross-cartridge dedup is defeated when at-rest is on — the same
tradeoff VSA pays on encrypted volumes).

### Encrypting a cartridge

`cartridge create --encrypt --keystore NAME` mints a per-cartridge
AES-256-GCM DEK wrapped by `NAME`; both flags are required together. A
cartridge created without `--encrypt` stays plaintext — nothing in
`thurvtl.yaml` enables encryption. The choice is sticky once created —
the manifest's `encryption.keystore_backend` captures it.

```bash
# Encrypted cartridge:
sudo -u thurvtl thurvtl cartridge create LTO0001 --encrypt --keystore kms-prod

# Inspect a cartridge's at-rest metadata:
sudo -u thurvtl thurvtl cartridge key show LTO0001
```

### Migrating to a different keystore

`cartridge key migrate BARCODE --to NEW_BACKEND` moves the DEK
wrap-target from one keystore to another. Cartridge bytes are NOT
re-encrypted — only the wrap-target moves. **Daemon-down.** Stop the
daemon, run the migrate, restart so the in-memory DEK cache reloads.

```bash
sudo systemctl stop thurvtld
sudo -u thurvtl thurvtl cartridge key migrate LTO0001 --to kms-dr
sudo systemctl start thurvtld
```

Mirrors VSA's `volume key migrate` semantics (same wrap-target
fingerprint check, same `--purge-local` flag for sweeping the sidecar
after a `local → external` move).
