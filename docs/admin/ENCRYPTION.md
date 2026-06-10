# At-Rest Encryption & Keystores

Thur VTL cartridges and Thur VSA volumes can be encrypted at rest under
a pluggable Data Encryption Key (DEK) keystore. This document covers the
keystore backends and the encrypt / inspect / migrate workflows for both
applications. Storage-backend credentials are in [`AUTH.md`](AUTH.md);
network and admin-listener security is in
[`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).

## VSA keystore backends

VSA volumes can be encrypted at rest with AES-256-GCM. Each volume gets
its own Data Encryption Key (DEK), and that DEK does not protect
itself — it lives wrapped inside a **pluggable keystore**, chosen at
`volume create` time. Six keystore backends ship, ranging from a
plaintext local file to enterprise HSMs:

- `local` (default) — on-disk plaintext sidecar at
  `<data_dir>/keys/<volume_uuid_hex>.key` (mode 0600). Protects
  ciphertext in storage backends and the local pool against bucket-leak /
  cold-disk theft, but **not** against a compromised thurvsad
  host.
- `awskms` — AWS KMS envelope encryption. The plaintext DEK is never
  persisted; the manifest carries the KMS-ciphertext blob. Encryption
  context binds every wrap to the volume UUID, so a stolen wrapped
  blob + KMS access cannot decrypt against another volume.
- `vault` — HashiCorp Vault Transit. Same envelope shape as KMS.
  `token` / `token_env` / `approle` / `approle_env` auth; AppRole
  lazily re-logs in on 401/403.
- `azurekv` — Azure Key Vault RSA wrap/unwrap. KV's RSA ops accept no
  service-side AAD, so the wrap path binds `volume_uuid` in an
  explicit JSON envelope; see § Azure-specific envelope below.
- `gcpkms` — GCP Cloud KMS symmetric encrypt/decrypt. KMS's native
  `additional_authenticated_data` carries `hex(volume_uuid)`; KMS
  rejects mismatching AAD on decrypt.
- `kmip` — KMIP 1.4+ `Encrypt` / `Decrypt` against an on-prem HSM or
  enterprise KMS (Thales CipherTrust, Entrust nShield / KeyControl,
  Fortanix DSM, Utimaco, Vault Enterprise KMIP, IBM SKLM, PyKMIP).
  Hand-rolled KMIP TTLV over mTLS, no upstream-crate dependency.
  AES-GCM AAD = `hex(volume_uuid)` is native in KMIP 1.4; IV /
  ciphertext / AEAD-tag come back as separate fields, stitched into a
  JSON envelope (see § KMIP-specific envelope below).

Named keystore backends live under **`keystore.backends:`** in the
VSA conffile (`/etc/thurvsa/thurvsa.yaml`), and the shipped conffile
carries a commented-out example per provider type. As with storage
backends, the workflow is edit-then-restart — there is no hot reload,
because the in-memory backend cache holds live SDK clients that are
built once at boot.

At-rest encryption is opt-in on a per-volume basis. Two flags turn it
on, and they are required together: `--encrypt` enables encryption and
`--keystore NAME` picks the backend that will wrap the DEK. A volume
created without `--encrypt` stays plaintext.

```bash
# Encrypt a volume — --encrypt and --keystore are a required pair.
thurvsa volume create vol1 --size 100G --encrypt --keystore kms-prod

# DEK source: 'daemon' (default) uses OsRng + backend wrap; 'backend'
# uses KMS GenerateDataKey / Vault transit/datakey (one fewer
# round-trip, HSM-grade RNG). Ignored for the local backend.
thurvsa volume create vol1 --size 100G --encrypt --keystore kms-prod \
    --dek-source backend

# Operator-supplied DEK (32 raw bytes = 64 hex chars, e.g.
# `openssl rand -hex 32 > vol.key`); the daemon wraps it via the
# selected keystore.
thurvsa volume create vol1 --size 100G --encrypt --keystore kms-prod \
    --key-file /tmp/vol.key
```

### `keystore.backends:` schema

```yaml
keystore:
  backends:
    local:
      type: local
    kms-prod:
      type: awskms
      key_id: alias/thurvsa-volumes
      region: us-east-1
      auth:
        type: env
        access_key_id_env: AWS_ACCESS_KEY_ID
        secret_access_key_env: AWS_SECRET_ACCESS_KEY
    vault-prod:
      type: vault
      address: https://vault.corp.example:8200
      transit_mount: transit
      transit_key: thurvsa-volumes
      auth: { type: token_env, env: VAULT_TOKEN }
    akv-prod:
      type: azurekv
      vault_uri: https://kv-prod.vault.azure.net
      key_name: thurvsa-kek
      auth:
        type: service_principal_env
        tenant_id_env: AZURE_TENANT_ID
        client_id_env: AZURE_CLIENT_ID
        client_secret_env: AZURE_CLIENT_SECRET
    gcp-prod:
      type: gcpkms
      key_name: projects/myproj/locations/global/keyRings/thur/cryptoKeys/thurvsa
      auth: { type: service_account_key_env, env: GOOGLE_APPLICATION_CREDENTIALS }
    kmip-prod:
      type: kmip
      endpoint: kms.corp.example:5696
      kek_uid: thurvsa-kek-1
      ca_bundle: { type: path, path: /etc/thurvsa/kmip-ca.crt }
      mtls:
        type: client_cert_env
        cert_path_env: KMIP_CLIENT_CERT
        key_path_env: KMIP_CLIENT_KEY
      credential:
        type: username_password_env
        username_env: KMIP_USERNAME
        password_env: KMIP_PASSWORD
```

This is the same schema as the pre-alpha.3 `keystore-backends.json`
file — only the location moved into the conffile and the `version`
envelope was dropped. Because JSON is valid YAML, old JSON entries copy
in 1:1.

Each keystore backend's `auth:` block mirrors the strict-override
semantics of the storage `S3Auth` and `AzureAuth` blocks in
[`AUTH.md`](AUTH.md): an explicit block pins the
credentials, and omitting it (where allowed) falls through to the
provider's default chain. The accepted shapes are:

- `awskms.auth`: `static` / `env` / `profile` (omit for the AWS SDK
  default chain).
- `vault.auth` (required): `token` / `token_env` / `approle` /
  `approle_env`.
- `azurekv.auth` (required): `service_principal` /
  `service_principal_env` — AAD-only (Key Vault accepts no SAS or
  account-key auth).
- `gcpkms.auth`: `service_account_key` (inline path) /
  `service_account_key_env` (path from named env var) / `adc`
  (Application Default Credentials chain — equivalent to omitting
  `auth`). ADC covers `GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth
  application-default login` user creds, and the GCE/GKE metadata
  server.
- `kmip.mtls` (required): `client_cert` (inline cert + key paths) /
  `client_cert_env` (paths from named env vars). PEM content stays on
  disk — never inlined.
- `kmip.credential` (optional): `username_password` /
  `username_password_env`. Layers a KMIP `Authentication` header on
  top of mandatory mTLS; some servers (Cosmian KMS, certain Thales
  configs) require both.
- `kmip.ca_bundle` (optional): `path` / `path_env` / `system_roots`.
  Trust store for KMIP server-cert verification; defaults to system
  roots.

### Provisioning the KEK per backend

The Key Encryption Key (KEK) — the key the keystore uses to wrap DEKs —
is provisioned out-of-band by the operator. The daemon never creates a
KEK; it only wraps and unwraps DEKs against one that already exists.
The permissions listed below are the runtime minimum, and they should
be granted scoped to the specific key resource rather than the whole
account.

#### `local`

The `local` backend needs no provisioning at all. On volume create the
daemon mints a DEK and stores it as a plaintext file at
`<data_dir>/keys/<volume_uuid_hex>.key`. This is suitable for tests and
air-gapped installs only — it is not a production posture, because the
key sits in the clear next to the data it protects.

#### `awskms`

This backend wraps against a symmetric CMK.

```bash
aws kms create-key --description "thurvsa volume DEK wrap" \
    --key-spec SYMMETRIC_DEFAULT --key-usage ENCRYPT_DECRYPT
aws kms create-alias --alias-name alias/thurvsa-volumes \
    --target-key-id <key-id-from-create>
```

The key needs three IAM permissions: `kms:Encrypt`, `kms:Decrypt`, and
`kms:DescribeKey`. The credentials the daemon uses to call KMS follow
the same chain as the S3 backend.

#### `vault`

This backend uses Vault's Transit secrets engine.

```bash
vault secrets enable transit                          # one-time per cluster
vault write -f transit/keys/thurvsa-volumes           # one key per logical KEK
```

The backend expects the default key type, `aes256-gcm96`. The token or
AppRole the daemon authenticates with needs the `update` capability on
`transit/encrypt/thurvsa-volumes` and `transit/decrypt/thurvsa-volumes`.

#### `azurekv`

This backend wraps against an RSA key in a Key Vault. EC keys cannot
wrap or unwrap, and symmetric AES-KW would require the Managed HSM
tier, so the backend calls **RSA-OAEP-256**, which is the path
available on the standard tier.

```bash
az keyvault create -n <vault> -g <rg> -l <region> \
    --enable-rbac-authorization
az keyvault key create --vault-name <vault> -n thurvsa-kek \
    --kty RSA --size 3072 --ops wrapKey unwrapKey
```

Then create a service principal and give it crypto access to the key —
either `Key Vault Crypto User` scoped to the key on an RBAC vault, or
`keys/wrapKey unwrapKey get` in a legacy access policy:

```bash
az ad sp create-for-rbac -n thurvsa-keystore
# capture: tenant, appId, password
az role assignment create --role "Key Vault Crypto User" \
    --assignee <appId> \
    --scope "$(az keyvault key show --vault-name <vault> -n thurvsa-kek --query id -o tsv)"
```

Finally, export the captured values as `AZURE_TENANT_ID`,
`AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET`, matching whatever `*_env`
names the keystore entry declares.

#### `gcpkms`

This backend wraps against a symmetric CryptoKey in Cloud KMS.

```bash
gcloud kms keyrings create thurvsa --location <region>
gcloud kms keys create thurvsa-volumes \
    --keyring thurvsa --location <region> \
    --purpose encryption          # algorithm: GOOGLE_SYMMETRIC_ENCRYPTION (default)
```

The key must use **purpose `encryption`** — that is, symmetric
encrypt/decrypt. The console also offers a "Raw symmetric encryption"
purpose, but that one routes through a different API path
(`rawEncrypt` / `rawDecrypt`) and will make wrap fail with
`InvalidArgument`, so it is the wrong choice here.

Grant the service account the
`roles/cloudkms.cryptoKeyEncrypterDecrypter` role, scoped to the key:

```bash
gcloud kms keys add-iam-policy-binding thurvsa-volumes \
    --keyring thurvsa --location <region> \
    --member serviceAccount:<sa>@<proj>.iam.gserviceaccount.com \
    --role roles/cloudkms.cryptoKeyEncrypterDecrypter
```

One detail about `key_name`: it is the CryptoKey resource path
**without** any `/cryptoKeyVersions/N` suffix, because both `decrypt`
and the health probe require the unversioned name.

Key destruction is worth understanding before you rely on this
backend. Running `gcloud kms keys versions destroy <ver> ...` moves the
version to `DESTROY_SCHEDULED`. After the destroy-scheduled duration —
which can be set anywhere from 24 hours to 120 days via
`--destroy-scheduled-duration`, defaulting to 30 days — GCP destroys
the key material for good. Any volume whose `wrapped_dek` was produced
by that destroyed version becomes permanently unrecoverable.

#### `kmip`

This backend wraps against an AES-256 KEK on a KMIP 1.4+ server. The
provisioning protocol is vendor-specific, but whatever the vendor, the
daemon needs three things:

- An AES-256 symmetric key with `Encrypt` + `Decrypt`
  cryptographic-usage attributes.
- The key's `Unique Identifier` (used as `kek_uid` in the entry).
- A client mTLS identity authorized to call `Encrypt` / `Decrypt` on
  that UID.

For a self-contained development rig, PyKMIP works:

```bash
pip install pykmip
pykmip-server -f /etc/pykmip/server.conf      # binds 0.0.0.0:5696
pykmip-client create-symmetric-key --algorithm AES --length 256
# stash the returned Unique Identifier as `kek_uid`
```

With PyKMIP, point `ca_bundle.path` at the server's self-signed cert —
PyKMIP is its own CA — and leave `credential` unset, since PyKMIP is
mTLS-only. Real enterprise servers are stricter and frequently demand
both mTLS *and* a `Username`/`Password` `credential`.

#### Azure-specific envelope (`azurekv`)

RSA-OAEP-256, the algorithm this backend calls, has no slot for
additional authenticated data. That is a problem, because the other
backends rely on AAD to guarantee that a stolen wrapped blob is
meaningless against any volume but the one it was wrapped for. To
preserve that same property without an AAD slot, the daemon wraps the
Key Vault ciphertext in an explicit envelope before persisting
`wrapped_dek`:

```json
{ "v": 1, "uuid": "<volume_uuid_hex>", "ct": "<base64-of-kv-output>" }
```

Here `uuid` is the lowercase-hex volume UUID. On unwrap the daemon
parses the envelope and refuses with `KeyStoreError::Authz` if `uuid`
does not match the call's `volume_uuid`. This envelope is itself
stored inside the manifest's base64-encoded `wrapped_dek`, so the
on-disk shape is base64-of-JSON-containing-base64. The whole scheme
costs roughly 80 extra bytes per manifest.

#### KMIP-specific envelope (`kmip`)

A KMIP `Encrypt` with AES-GCM returns three separate fields: the
ciphertext (`Data`), the server-generated IV (`IVCounterNonce`), and
the AEAD tag (`AuthenticatedEncryptionTag`). The daemon stitches those
three into a JSON envelope before writing the manifest:

```json
{
  "v": 1,
  "uuid": "<volume_uuid_hex>",
  "iv":   "<base64-of-IV>",
  "ct":   "<base64-of-ciphertext>",
  "tag":  "<base64-of-AEAD-tag>"
}
```

Here `uuid` is again the lowercase-hex volume UUID. Unlike the Azure
case, it is redundant — KMIP itself verifies the same value as AAD on
`Decrypt`. The envelope-level check is deliberate belt-and-suspenders:
a stolen `wrapped_dek` re-pasted onto another volume's manifest fails
with `KeyStoreError::Authz` before any bytes ever reach the KMIP
server. On unwrap the daemon splits the envelope, rebinds
`hex(volume_uuid)` as AAD, and sends one `Decrypt`. The envelope costs
roughly 120 extra bytes per manifest.

However the DEK is wrapped, the result lands in two manifest fields:
the `encryption` block gains `keystore_backend: <name>` (defaulting to
`"local"`) and `wrapped_dek: <base64>` (omitted for `local`, which uses
the sidecar instead). The upshot is that a stolen manifest is useless
on its own — without the matching KMS or Vault credentials it still
cannot decrypt the volume.

The full design — threat model, IV derivation, and how encryption
interacts with dedup — lives in
[`CONFORMANCE_SCSI.md`](../reference/CONFORMANCE_SCSI.md) § At-rest encryption.
Forward work on key custody is tracked in the issue tracker.

It is worth being explicit that keystore backends and storage-backend
authentication are orthogonal concerns. The storage credentials gate
access to the bucket itself; the keystore gates access to the bytes
inside it. Neither substitutes for the other.

### Migrating a volume between keystore backends

`thurvsa volume key migrate <NAME> --to <NEW_BACKEND>
[--purge-local]` moves a volume's DEK wrap-target from one keystore to
another. The important thing to understand is what does *not* change:
the plaintext DEK stays the same, only the wrap-target moves, and the
volume's data, chunks, and ciphertext all stay byte-identical.

**Daemon-up safe.** The migrate can run with the daemon live because
`manifest.json` is frozen at creation time — on the hot path the daemon
only ever mutates `runtime.json`. That means rewriting
`encryption.keystore_backend` and `wrapped_dek` out-of-band cannot race
the live `VolumeWriter`. Restart the daemon afterward so it picks up
the new binding.

```bash
# 1. Run the migrate. Local → external keeps the sidecar in place
#    by default (roll back by reverting the manifest).
sudo -u thurvsa thurvsa volume key migrate prod-01 --to kms-prod

# 2. Restart. Discovery uses the new backend; if unwrap fails (typo'd
#    key id, IAM not granted) the volume refuses to attach.
sudo systemctl restart thurvsad

# 3. Once confirmed, sweep the sidecar:
sudo -u thurvsa thurvsa volume key migrate prod-01 \
    --to kms-prod --purge-local       # idempotent if already on kms-prod
sudo systemctl restart thurvsad
```

**Rollback.** If the new backend turns out to be misconfigured — a
typo'd KMS key id, IAM not granted, an AAD service principal that lacks
`keys/unwrapKey` — the migration can be backed out by hand:

1. Stop the daemon, so it is not holding a stale in-memory unwrap.
2. Hand-edit `<data_dir>/volumes/<name>/manifest.json`: revert
   `encryption.keystore_backend` to the old name, and remove
   `encryption.wrapped_dek` if reverting to `local`.
3. The old sidecar at `<data_dir>/keys/<uuid>.key` is still in place —
   it is only deleted when `--purge-local` was passed.
4. Restart; discovery now uses the old backend.

The same migration works in reverse, with `--to local`. The CLI has
two guardrails worth knowing. It refuses a same-backend no-op, so
`--to current-backend` exits 1. And before doing any unwrap or wrap it
compares the source and destination wrap-target *fingerprints*: if two
differently-named entries actually resolve to the same external
location — two `local` entries with the same `data_dir`, or two
`awskms` entries on the same key ARN — it exits 1 with the resolved
fingerprint named in the error, rather than performing a meaningless
migration.

### Cloning an encrypted volume: shared DEK custody

`volume clone` of an encrypted volume (issue #86) does **not** mint a new
DEK. The clone inherits the source's *crypto identity* in its manifest's
`crypto_uuid` field — the value the keystore is addressed by (the wrap
context / AAD) and the IV is derived from — and copies the source's
`encryption` metadata (`keystore_backend` + `wrapped_dek`) verbatim. So
the whole family (source + clones + their snapshots) shares one DEK and
one keystore entry: for `local`, one sidecar at
`<data_dir>/keys/<crypto_uuid_hex>.key`; for the envelope backends, the
same `wrapped_dek` blob bound to `crypto_uuid`. No re-wrap, no
re-encrypt.

Because the DEK is shared, its lifecycle is **refcounted by scan**. There
is no persistent refcount — `volume destroy` removes the volume's on-disk
subtree first, then walks every surviving volume and snapshot manifest
(`crypto_identity_referenced`) and only calls `keystore.forget()` when no
other family member still keys its crypto identity on that DEK. So
destroying the source while a clone exists retains the DEK (for `local`,
the sidecar survives; for the envelope backends `forget` is a no-op
anyway); destroying the *last* family member is what finally forgets it.
If the scan itself fails, destroy conservatively keeps the DEK — a leaked
wrapped DEK is inert, whereas a wrongly-forgotten one would strand a
clone.

`volume key migrate`, `key export`, and `key import` all address the
keystore by `crypto_uuid` too, so they operate on the shared DEK
correctly. **`volume key migrate` refuses a crypto identity that is still
shared** (the same manifest walk): migrating would rewrap only one
member's manifest and, with `--purge-local`, forget a sidecar the rest of
the family still needs. Destroy the other members first, leaving a single
holder, before migrating a once-shared identity.

### Escrow: passphrase-sealed DEK export / import

`volume key migrate` assumes both keystores are *online* at the same
time. That assumption breaks for two real scenarios: cross-region DR,
where the recovery-side wrap-target does not exist yet, and
audit-compliant custody, where the DEK must live inside a sealed
envelope under a separate custody chain. Two verbs cover those cases:

```bash
# Daemon-down on the source side. Prompts twice for a passphrase
# (THURVSA_PASSPHRASE bypasses the prompt for automation).
sudo -u thurvsa thurvsa volume key export prod-01 \
    --to /mnt/escrow/prod-01.jwe

# On the recovery side: copy the manifest + the .jwe across, daemon-down:
sudo -u thurvsa thurvsa volume key import prod-01 \
    --from /mnt/escrow/prod-01.jwe --keystore kms-dr
sudo systemctl start thurvsad
```

**Envelope format.** The escrow file is a JWE Compact serialization
(RFC 7516 §3.1) using `alg = "PBES2-HS512+A256KW"` (RFC 7518 §4.8) and
`enc = "A256GCM"` (RFC 7518 §5.3). Key derivation is
PBKDF2-HMAC-SHA512 at a default of 600 000 iterations; an operator can
tune that with `--iter`, but the minimum is 100 000 and anything below
that is refused at decode time. The 32-byte DEK is first JSON-wrapped
as `{"dek":"<base64>","alg":"AES-256-GCM","v":1}`, and that JSON is the
payload the GCM encryption protects.

**Volume binding.** The protected header carries
`thur_volume_uuid: <hex>`. Because RFC 7516 §5.1 makes the
base64URL-encoded header the GCM AAD, tampering with the UUID
invalidates the GCM tag. Import goes one step further and
cross-checks the bound UUID against the local manifest's UUID,
refusing on any mismatch.

**Refusal semantics.** Export refuses if the volume is not encrypted,
or if the output path already exists. Import refuses on a UUID
mismatch, or if the target keystore already holds an unwrappable DEK
for that UUID — that situation is `migrate`'s job, not import's.
Finally, the failure modes are deliberately collapsed: a wrong
passphrase, tampered ciphertext, a tampered header, and a tampered
wrapped CEK all surface as the single outcome "decryption failed", so
the verb gives an attacker no oracle.

**Passphrase guidance.** Treat the passphrase and the envelope file as
two halves of a split secret. The passphrase belongs in a
human-custody vehicle — a password manager or a sealed safe — and the
envelope file belongs in a different custody, such as an immutable
bucket or a separate vault. For strength, aim for a diceware
passphrase of at least 6 words, or at least 14 random printable
characters.

**Interop.** The envelope is nothing more than plain JWE/PBES2, so any
spec-conformant JOSE library — Python `jwcrypto`, Node `jose`, Java
`nimbus-jose-jwt`, Go `go-jose` — can decode it. That makes it
possible to verify an escrow file on the recovery side without the
daemon at all:

```python
# pip install jwcrypto
from jwcrypto import jwe
import json, base64
with open("/mnt/escrow/prod-01.jwe") as f:
    blob = f.read().strip()
token = jwe.JWE()
token.deserialize(blob)
token.decrypt(jwe.JWK(kty="oct", k=base64.urlsafe_b64encode(
    b"<your passphrase>").rstrip(b"=").decode()))
payload = json.loads(token.payload)
# payload == {"dek": "<base64>", "alg": "AES-256-GCM", "v": 1}
```

## VTL keystore backends

VTL cartridges can be encrypted at rest using the same six keystore
backends VSA exposes — `local`, `awskms`, `vault`, `azurekv`,
`gcpkms`, and `kmip`. The wiring runs parallel to VSA's volume DEK
path, and as on VSA, at-rest encryption is **opt-in**.

A cartridge created without `--encrypt` stays plaintext; nothing in the
YAML turns encryption on globally. Passing `--encrypt` together with
`--keystore NAME` — again a required pair — makes the daemon mint a
freshly-generated per-cartridge AES-256-GCM DEK and wrap it against the
named backend.

This appliance-side encryption is independent of LTO host-driven AME,
the SSC-4 SECURITY PROTOCOL OUT path where the backup app supplies the
key. The two layers compose rather than conflict. AME encrypts each
block at write time, using a per-block IV of
`derive_iv(uuid, chunk_id, offset)` and the host's key. Then at
chunk-seal the appliance-side at-rest layer wraps the entire
AME-ciphertext chunk, using a per-chunk IV of
`derive_iv(uuid, chunk_id, 0)` and the daemon-managed DEK. The result
is true zero-plaintext-at-rest even if the backup app forgets to enable
AME.

```bash
# Encrypt a cartridge — --encrypt and --keystore are a required pair:
sudo -u thurvtl thurvtl cartridge create LTO0001 --encrypt --keystore kms-prod

# Inspect:
sudo -u thurvtl thurvtl cartridge key show LTO0001
```

**Wrapped DEK persistence.** This works exactly as it does for a VSA
volume manifest. Non-`local` backends store the wrapped DEK as base64
in the cartridge's `manifest.json` under `encryption.wrapped_dek`,
while the `local` backend keeps a plaintext sidecar at
`<data_dir>/keys/<cartridge_uuid_hex>.key` with mode 0600.

**Pool layout.** When at-rest encryption is on, both the local chunk
pool and the storage backend store **ciphertext**, under content hashes
that are themselves computed over the **ciphertext**. Two consequences
follow from that.

- Dedup is largely defeated. Cross-cartridge dedup fails because the
  same content under different DEKs produces different ciphertexts and
  therefore different hashes; within-cartridge dedup fails because
  chunk IDs are monotonic, so every chunk gets a fresh IV. This is the
  same tradeoff VSA pays — see [`DEDUP.md`](../reference/DEDUP.md).
- Storage objects are opaque on the wire. The flip side is that
  restoring a cartridge from a cold bucket onto a different host
  requires that host's daemon to reach the same keystore. Without it
  the chunk fetch still succeeds, but the decrypt fails and the
  cartridge refuses host reads.

**Boot-time DEK unwrap.** At startup the daemon walks
`<data_dir>/tapes/*/manifest.json` and, for every cartridge whose
manifest has `encryption: Some(...)`, asks the named keystore to
unwrap the wrapped DEK. The plaintext DEK is then cached in
`DriveManager` for the daemon's lifetime, so the synchronous SCSI MOVE
MEDIUM hot path never has to round-trip the keystore on a load. A
keystore that is unreachable at boot does **not** fail startup; instead
`load_cartridge` for the affected barcode simply refuses until the
keystore comes back.

### Migrating a cartridge between keystore backends

`thurvtl cartridge key migrate <BARCODE> --to <NEW_BACKEND>
[--purge-local]` moves a cartridge's DEK wrap-target between keystores.
It mirrors VSA's `volume key migrate`: the plaintext DEK is unchanged,
only the wrap-target moves, and the cartridge data stays
byte-identical.

**Daemon-down.** Here the VTL behavior diverges from VSA. This verb
refuses to run while the daemon is up, because VTL caches plaintext
DEKs at boot — an out-of-band manifest rewrite would race that
in-memory cache. The sequence is therefore: stop `thurvtld`, run
the migrate, restart.

The same wrap-target fingerprint check VSA uses applies here too: two
differently-named entries that resolve to the same external location
exit 1 with the resolved fingerprint named in the error.

### Testing a keystore backend end-to-end

`vsa/scripts/test-keystore.sh` is the keystore counterpart of
`test-fs-storage.sh`. Pick one entry from a
`keystore-backends.yaml` source file and the script runs three phases
against it: **wrap** (volume create stamps the manifest), **unwrap**
(a daemon restart makes discovery re-open the volume), and **migrate**
(the wrap-target is moved to a local fallback, restarted, and
verified).

```bash
# Drop real backend entries in $REPO/private/keystore-backends.yaml
# (and matching creds in $REPO/private/thur.env), then:
THURVSA_TEST_KEYSTORE=kms-prod ./vsa/scripts/test-keystore.sh --release
THURVSA_TEST_KEYSTORE=vault-prod ./vsa/scripts/test-keystore.sh --release
THURVSA_TEST_KEYSTORE=akv-prod ./vsa/scripts/test-keystore.sh --release
THURVSA_TEST_KEYSTORE=gcp-prod ./vsa/scripts/test-keystore.sh --release
THURVSA_TEST_KEYSTORE=kmip-prod ./vsa/scripts/test-keystore.sh --release
```

Per-backend KEK provisioning is covered by § Provisioning the KEK per
backend above — the script never creates KEKs itself. Two backends
need a note:

- **`local`** needs no setup, and phase 3 (migrate) is skipped for it
  because a `local`-to-`local` move is a degenerate no-op.
- **`vault`** can be run self-contained by starting a dev server
  (`vault server -dev -dev-root-token-id=<token>`) and pasting the root
  token into the entry's `auth.value`.

The script deliberately exercises only the wrap and unwrap path
against each backend type — roughly 10 seconds per run, and
credential-light. The heavier coverage of iSCSI fixtures and storage
chunks lives elsewhere, in `test-pipeline-layers.sh` row 3 (`+encrypt`
against the default `local` backend).

