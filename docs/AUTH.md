# Cloud Authentication

This document explains how Thur VTL and Thur VSA authenticate to their
cloud storage backends — S3, GCS, and Azure — and why credential
resolution is shaped the way it is. It is the authoritative reference
on the subject; other docs link here rather than repeat it.

Every cloud backend the daemon talks to is a named definition under
`cloud.backends:` in the daemon's YAML conffile, which is
`/etc/thurvtl/thurvtl.yaml` on the VTL side and
`/etc/thurvsa/thurvsa.yaml` on the VSA side. The workflow is the same
each time: edit the conffile, restart the daemon, and the backends
come up live. The installed conffile already carries a commented-out
example of every provider type, so the starting point is to uncomment
and adapt.

The top-level YAML looks like this:

```yaml
cloud:
  compression: { algorithm: zstd, level: 3 }
  upload: { max_concurrent: 0 }
  backends:
    primary:
      type: s3
      bucket: ...
      region: ...
      auth: { ... }
    archive:
      type: gcs
      bucket: ...
      project_id: ...
```

There are two ways to supply credentials, and they can be mixed freely
on a per-backend basis:

1. **Per-backend `auth:` block** — explicit credentials that strictly
   override everything else. This is the only way to run multiple
   S3-flavored providers (AWS S3 + MinIO + Wasabi) inside one daemon.
2. **Default credential chain** — the provider's own discovery order
   over env vars, instance identity, and provider CLI configs. The
   daemon falls back to this for any backend that has no `auth:` block.

The rule that decides between them is simple: a backend with an `auth:`
block ignores the chain entirely, and a backend without one walks the
chain.

## Per-backend `auth:` blocks

Any `cloud.backends:` entry may carry an `auth:` block that pins its
credentials. Adding that block is a **strict override** — once it is
present, the daemon ignores every env var, instance identity, and SDK
chain for that one backend. Other backends are untouched, so the
override is local to the entry it appears on.

The reason this matters is that cloud SDK credential chains are
process-global. The AWS chain, for example, reads `AWS_ACCESS_KEY_ID`
once per process. That works fine for a single backend, but it breaks
the moment two S3-flavored providers share one daemon: whichever
credentials win the chain race end up applying to every backend. A
per-backend `auth:` block sidesteps the problem by injecting explicit
credentials directly into the SDK, so the global chain is never
consulted for that backend.

### S3 (`type: s3`)

```yaml
cloud:
  backends:
    aws-prod:
      type: s3
      bucket: thurvtl-prod
      region: us-east-1
      auth:
        type: static
        access_key_id: "AKIA..."
        secret_access_key: "..."

    minio-onprem:
      type: s3
      bucket: backups
      region: us-east-1
      endpoint_url: "http://minio.local:9000"
      auth:
        type: env
        access_key_id_env: MINIO_KEY
        secret_access_key_env: MINIO_SECRET

    wasabi-archive:
      type: s3
      bucket: cold-tapes
      region: us-east-1
      endpoint_url: "https://s3.us-east-1.wasabisys.com"
      auth:
        type: profile
        name: wasabi          # picks [profile wasabi] from ~/.aws/credentials

    iam-role-bucket:
      type: s3
      bucket: shared
      region: us-east-1
      # no auth: → default chain (IRSA, instance profile, ~/.aws, env)
```

The `auth.type` field selects how the credentials are sourced:

- **`static`** puts the `access_key_id` and `secret_access_key` inline
  in the YAML, with an optional `session_token` for STS temporary
  credentials. The cost is that the secrets are visible to anyone who
  can read the YAML file.
- **`env`** names the env vars to read at startup through
  `access_key_id_env` and `secret_access_key_env` (plus an optional
  `session_token_env`). Pairing this with the daemon env file keeps the
  secret values themselves out of the YAML.
- **`profile`** uses `name` to pick a profile from
  `~/.aws/credentials` and `~/.aws/config`. It works with whatever
  populated those files — `aws configure`, `aws sso login`, or
  hand-edited blocks.

Two further S3 fields shape the request rather than the credentials:

- **`endpoint_url`** is an optional custom endpoint, used for MinIO,
  Ceph RGW, AIStor, Wasabi, and sovereign regions. When it is omitted —
  the real-AWS case — the daemon opts into the dualstack endpoint
  `s3.dualstack.<region>.amazonaws.com`, which publishes both A and
  AAAA records so that IPv4-only hosts still resolve it. An operator
  who needs the IPv4-only endpoint can force it by setting
  `endpoint_url` to `https://s3.<region>.amazonaws.com`.
- **`path_style`** is an optional `true`/`false` override for the
  request URL shape — path-style `/<bucket>/<key>` versus virtual-host
  `<bucket>.<endpoint>/<key>`. When it is omitted, the daemon forces
  path-style whenever `endpoint_url` is set and otherwise leaves the
  SDK at its virtual-host default for real AWS.

### GCS (`type: gcs`)

GCS is simpler than S3: it has a single knob,
`service_account_key_file`, which is a path to a service-account JSON
key file given directly on the backend entry. There is no nested
`auth:` block.

```yaml
cloud:
  backends:
    archive:
      type: gcs
      bucket: thurvtl-cold
      project_id: my-gcp-project
      service_account_key_file: /etc/thurvtl/gcs-archive-sa.json

    cold-storage:
      type: gcs
      bucket: thurvtl-deep-cold
      project_id: my-gcp-project
      service_account_key_file: /etc/thurvtl/gcs-coldstorage-sa.json
```

Because each backend names its own key file, two GCS backends with two
distinct service accounts run side by side without interfering. Lock
each JSON file down as described in *File permissions* below. Omitting
`service_account_key_file` makes the backend fall through to
Application Default Credentials.

### Azure (`type: azure`)

```yaml
cloud:
  backends:
    cold:
      type: azure
      storage_account: mystorageacct
      container: thurvtl
      auth:
        type: sas_url
        value: "https://mystorageacct.blob.core.windows.net/thurvtl?sv=...&sig=..."

    cold-from-env:
      type: azure
      storage_account: anotheracct
      container: tapes
      auth:
        type: sas_url_env
        env: AZURE_PROD_SAS

    aad-sp:
      type: azure
      storage_account: aadacct
      container: worm-tapes
      retention_mode: compliance      # WORM — needs AAD
      subscription_id: "..."
      resource_group: "..."
      auth:
        type: service_principal
        tenant_id: "..."
        client_id: "..."
        client_secret: "..."
      # or: type: service_principal_env, *_env: AZURE_TENANT_ID, etc.
```

The `auth.type` field accepts `sas_url`, `sas_url_env`,
`service_principal`, and `service_principal_env`. Storage-account
shared-key auth used to be an option but was dropped on 2026-05-10,
when the code migrated to Microsoft's `azure_storage_blob` SDK, which
is bearer-token-only.

**WORM caveat:** a SAS URL is a data-plane credential — it can read and
write blobs, but it cannot mint AAD tokens. A backend with
`retention_mode != none` also needs management-plane access, because
the daemon queries the immutability policy there. Such backends must
therefore use `service_principal` (or the default chain backed by
Managed Identity or workload identity), not a SAS URL.

## Default credential chains

When a backend carries no `auth:` block, the daemon hands credential
discovery to the provider's own standard chain. Each provider walks a
different ordered list and stops at the first rung that produces
usable credentials.

| Backend | Discovery order (first match wins) |
| --- | --- |
| **S3** (`aws_sdk_s3`) | Env vars (`AWS_ACCESS_KEY_ID` / `_SECRET_ACCESS_KEY` / `_SESSION_TOKEN`, `AWS_PROFILE`, `AWS_REGION`) → `AssumeRoleWithWebIdentity` (IRSA / OIDC) → SSO → ECS task role → EC2 IMDS instance profile → `~/.aws/credentials` + `~/.aws/config` |
| **GCS** (Application Default Credentials) | `GOOGLE_APPLICATION_CREDENTIALS` file → `gcloud auth application-default login` user creds → GCE/GKE metadata server (workload identity) |
| **Azure** | `AZURE_STORAGE_SAS_URL` → `AZURE_TENANT_ID` + `_CLIENT_ID` + `_CLIENT_SECRET` service principal → AAD fallback chain (`ManagedIdentityCredential` IMDS / Azure VM / AKS → `DeveloperToolsCredential` `az login`) |

The Azure env-var precedence is implemented in
`shared/cloud/src/azure.rs::discover_credentials_from_env`. So that the
choice is never a mystery, the daemon logs which rung it picked at
startup and warns whenever a higher-precedence env var has shadowed the
one the operator expected.

## Cloud-VM installs (instance and workload identity)

On a cloud VM, the cleanest setup is to configure no credentials at
all. Skip both the `auth:` blocks **and** the daemon env file, and let
the provider's SDK pick up the platform's own instance identity:

- **AWS EC2**: attach an IAM role; SDK reads IMDSv2.
- **AWS EKS / IRSA**: annotate the ServiceAccount with an IAM role ARN;
  SDK reads the projected token at
  `/var/run/secrets/eks.amazonaws.com/serviceaccount/token` and calls
  `AssumeRoleWithWebIdentity`.
- **AWS ECS**: task role; SDK reads
  `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`.
- **GCE / GKE Workload Identity**: attach a Google Service Account to
  the VM / pod; ADC reads the metadata server.
- **Azure VM / AKS Managed Identity**: assign a managed identity; the
  AAD fallback chain (`ManagedIdentityCredential` first) reads IMDS.

This is the preferred posture wherever it is available: there are no
static credentials sitting on disk, rotation is automatic, and the IAM
grant is scoped to exactly what the VM needs. The daemon needs no
configuration beyond the bucket or container name.

## On-prem / bare metal: the daemon env file

Off a cloud platform there is no instance identity to fall back on, so
the daemon needs static credentials supplied through its environment.
The package ships a starter file for that purpose — `thurvtl.env` on
the VTL side, `thurvsa.env` on the VSA side — installed at
`/etc/thurvtl/thurvtl.env` with mode root:thurvtl 0640 and every
variable commented out. The systemd unit wires it in with
`EnvironmentFile=-/etc/thurvtl/thurvtl.env`; the leading `-` marks the
file optional, so a daemon with no env file still starts. The same
file also holds the `${ENV_VAR}` secrets that the YAML references for
other subsystems, such as alerting webhook tokens and SMTP passwords.

```bash
sudo $EDITOR /etc/thurvtl/thurvtl.env       # uncomment what you need
sudo systemctl restart thurvtld
sudo systemctl show thurvtld -p EnvironmentFiles
# EnvironmentFiles=/etc/thurvtl/thurvtl.env (ignore_errors=yes)
```

Variables the daemon recognizes (when no per-backend `auth:` is set):

- **AWS S3 / MinIO / Wasabi**: `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (optional `AWS_SESSION_TOKEN`,
  `AWS_PROFILE`).
- **GCS**: `GOOGLE_APPLICATION_CREDENTIALS=/path/to/sa.json` (lock the
  SA file to root:thurvtl 0640).
- **Azure**: either `AZURE_STORAGE_SAS_URL` or the trio
  `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET`.
  Without either, the daemon falls back to the AAD chain (managed
  identity → `az login`).

The variable names listed above are the ones the *default chain*
expects. When a backend instead uses `auth: { type: env, ... }`, the
env-var names are no longer fixed — the YAML names them explicitly, so
they can be anything. Choose names that document which backend they
belong to, such as `MINIO_KEY`, `WASABI_KEY`, or `AWS_PROD_KEY`.

## Recommended layouts

| Scenario | Setup |
| --- | --- |
| Single-provider, on-prem | Credentials in `thurvtl.env` / `thurvsa.env`, no `auth:` block — the default chain reads the env vars. |
| Single-provider, cloud VM | Nothing in the env file, no `auth:` block — instance identity flows through the SDK chain. |
| Multi-provider (AWS + MinIO + Wasabi + …) | `auth: { type: env, ... }` on every backend with distinct env-var names per backend; drop those vars in `thurvtl.env`. The single-provider chain can't carry more than one credential set. |
| Mixed: one cloud-VM-native + one external | Native backend gets no `auth:` block (instance identity); external backend gets `auth: { type: env, ... }` with creds in `thurvtl.env`. |

## File permissions

Three files routinely hold cloud-auth secrets, and all three should be
**root:thurvtl 0640** — readable by the daemon's group, writable only
by root:

- `/etc/thurvtl/thurvtl.yaml` — when `auth: { type: static, ... }` is
  used (inline secrets).
- `/etc/thurvtl/thurvtl.env` — env vars consumed by the daemon.
- Any service-account JSON files referenced by
  `service_account_key_file` or `GOOGLE_APPLICATION_CREDENTIALS`.

The first two files are handled for you: the postinst script chowns
`thurvtl.env` to root:thurvtl 0640 on every install or upgrade, and the
YAML gets the same treatment from cargo-deb's `mode = "640"` asset
declaration. The service-account JSON files are operator-installed, so
their permissions are the operator's responsibility:

```bash
sudo install -o root -g thurvtl -m 0640 \
    /tmp/sa.json /etc/thurvtl/gcs-archive-sa.json
```

## Precedence summary

Pulling the pieces together: for any single backend the daemon resolves
credentials in this order, and stops at the first one that applies.

1. **`auth:` block on the backend** — a strict override; when it is
   present, env vars are ignored.
2. **Provider env vars** in the daemon's environment, loaded from
   `thurvtl.env` by the systemd unit.
3. **Provider SDK chain rungs** — IRSA, instance profile, SSO, ADC,
   Managed Identity, `~/.aws/credentials`, and the rest.

Every backend runs this resolution independently of the others, so one
backend can use an `auth:` block while another walks the chain.

## Verifying the daemon picked up the right credentials

```bash
# What env file does systemd see?
sudo systemctl show thurvtld -p EnvironmentFiles

# Daemon logs which rung it picked per backend at startup:
sudo journalctl -u thurvtld | grep -E "auth =|Initializing .* backend"

# Watch a write actually land in the bucket:
sudo tar -cvf /dev/nst0 /some/path
sudo journalctl -u thurvtld -f | grep -i upload
```

Common failure → fix:

| Symptom | Likely cause |
| --- | --- |
| `auth env var 'X' is not set` at startup | `auth: { type: env, ..._env: X }` references an env var not present in `thurvtl.env` / `thurvsa.env`. Add it. |
| `403 Forbidden` on first write | Credentials valid but lack `s3:PutObject` (or equivalent). Check IAM policy. |
| `InvalidAccessKeyId` on a MinIO/Wasabi backend after configuring AWS creds | The process-global chain is fighting you. Use per-backend `auth:`. |
| `SAS URL is set, so SAS auth wins — AAD service-principal env vars … are being ignored` warning | Stale `AZURE_TENANT_ID` / `_CLIENT_ID` / `_CLIENT_SECRET` in env. Remove them, or move to per-backend `auth:`. |
| GCS `Bearer token has expired` after long uptime | ADC token isn't refreshing. Use `service_account_key_file` — file-based creds refresh automatically. |

## VSA keystore backends

VSA volumes can be encrypted at rest with AES-256-GCM. Each volume gets
its own Data Encryption Key (DEK), and that DEK does not protect
itself — it lives wrapped inside a **pluggable keystore**, chosen at
`volume create` time. Six keystore backends ship, ranging from a
plaintext local file to enterprise HSMs:

- `local` (default) — on-disk plaintext sidecar at
  `<data_dir>/keys/<volume_uuid_hex>.key` (mode 0600). Protects
  ciphertext in cloud buckets and the local pool against bucket-leak /
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
carries a commented-out example per provider type. As with cloud
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
in 1:1. A leftover `<data_dir>/keystore-backends.json` is treated as a
stale artifact and makes the daemon refuse to start.

Each keystore backend's `auth:` block mirrors the strict-override
semantics of `S3Auth` and `AzureAuth` above: an explicit block pins the
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
[`CONFORMANCE_SCSI.md`](CONFORMANCE_SCSI.md) § At-rest encryption.
Forward work on key custody is tracked in
[`../ROADMAP.md`](../ROADMAP.md) § Encryption-key management.

It is worth being explicit that keystore backends and cloud-backend
authentication are orthogonal concerns. The cloud credentials gate
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
pool and the cloud bucket store **ciphertext**, under content hashes
that are themselves computed over the **ciphertext**. Two consequences
follow from that.

- Dedup is largely defeated. Cross-cartridge dedup fails because the
  same content under different DEKs produces different ciphertexts and
  therefore different hashes; within-cartridge dedup fails because
  chunk IDs are monotonic, so every chunk gets a fresh IV. This is the
  same tradeoff VSA pays — see [`DEDUP.md`](DEDUP.md).
- Cloud objects are opaque on the wire. The flip side is that
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
`test-iscsi-fs-cloud.sh`. Pick one entry from a
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
credential-light. The heavier coverage of iSCSI fixtures and cloud
chunks lives elsewhere, in `test-pipeline-layers.sh` row 3 (`+encrypt`
against the default `local` backend).

## Admin HTTP TLS

Both daemons expose their admin HTTP surface — `/health`, `/metrics`,
`/sessions`, and `/info` — on a single TCP listener, defaulting to
`0.0.0.0:9090`. TLS on that listener is opt-in, configured through the
`http.tls` block:

```yaml
http:
  listen: "0.0.0.0:9090"
  tls:
    cert_file: "/var/lib/thurvtl/tls/cert.pem"
    key_file: "/var/lib/thurvtl/tls/key.pem"
    client_ca_file: ""
    extra_sans: []          # extra SANs for the auto-generated cert
```

The daemon accepts exactly three states for the `cert_file` /
`key_file` pair, and rejects any other combination at boot:

| `cert_file` | `key_file` | Listener | Notes |
| ----------- | ---------- | -------- | ----- |
| empty | empty | plaintext HTTP | today's default |
| set + file present | set + file present | HTTPS | loaded as-is at boot |
| set + file absent | set + file absent | HTTPS | **auto-generated self-signed pair on first boot** |

The third state — paths configured but the files absent — triggers
the auto-gen path. It writes an ECDSA P-256 cert to the configured
paths, with CN set to the hostname (or `localhost`), SANs of
`hostname`, `localhost`, `127.0.0.1`, `::1` followed by any
`http.tls.extra_sans`, and a 10-year validity. The files get modes
`0644` for the cert and `0600` for the key, alongside a marker sidecar
`<cert_file>.autogen` (its role is explained under *Regenerating the
self-signed cert*). The daemon then logs at `WARN`:

```
admin HTTP TLS: self-signed cert generated; replace with CA-issued
cert for production cert_file=... key_file=... fingerprint_sha256=...
sans=hostname,localhost,127.0.0.1,::1
```

`http.tls.extra_sans` exists for operators who reach the listener by
an alias — it adds DNS names or IPs to the generated cert. There is no
interface-IP auto-discovery. Note that it is consulted *only* on the
generate path; a loaded CA-issued cert ignores it entirely.

On every subsequent boot the existing pair is loaded as-is, logged at
`INFO` with the same fingerprint. To swap in a CA-issued cert later,
write the new PEMs over the configured paths and restart — the
fingerprint line in the log confirms the switch took.

### Optional mutual TLS

Setting `http.tls.client_ca_file` to a PEM CA bundle flips the listener
into mTLS mode: from then on every request must present a client cert
signed by that CA. mTLS builds on top of server TLS, so it requires
`cert_file` and `key_file` to be set as well — configuring
`client_ca_file` without the server pair is a boot-time error.

**Prometheus scrape note.** Once mTLS is on, every scrape also needs a
client cert. The Alertmanager or Prometheus `scrape_config` must
therefore include:

```yaml
scheme: https
tls_config:
  cert_file: /etc/prometheus/clients/prom.crt
  key_file:  /etc/prometheus/clients/prom.key
  ca_file:   /etc/prometheus/server-ca.pem
```

For the common case of server-side TLS only, leave `client_ca_file`
empty.

### File location convention

By convention the self-signed cert and key live under
`<data_dir>/tls/{cert,key}.pem`. CA-issued certs are less constrained —
they can live anywhere the daemon user can read. On packaged installs
`/etc/<product>/tls/` works fine, provided the postinst sets
`0640 root:<product>` on the key.

### Regenerating the self-signed cert

`<product> system regenerate-cert` overwrites the auto-generated
cert and key in place. It is a daemon-down command — it refuses to run
while the admin socket answers — and the rewritten cert is only served
after a restart. Because it re-derives the SAN list each time, it is
the right tool for picking up a hostname change or an edited
`http.tls.extra_sans`:

```bash
sudo systemctl stop thurvtl
sudo thurvtl system regenerate-cert
sudo systemctl start thurvtl
```

One safeguard is built in: `regenerate-cert` refuses to clobber a cert
the daemon did not itself generate. The mechanism is the marker
sidecar. Every auto-generation writes a `<cert_file>.autogen` file
(mode `0644`) holding the cert's SHA-256 fingerprint, and
`regenerate-cert` only overwrites when the on-disk cert's fingerprint
matches that marker. A CA-issued or operator-supplied cert has no
matching marker, so it is left alone. If you genuinely want a fresh
self-signed cert over an operator cert, delete the cert, key, and
`.autogen` files by hand and re-run.

### Verification

```bash
# Plaintext (legacy)
curl -fsS http://127.0.0.1:9090/health

# HTTPS (any auth mode)
curl -k -fsS https://127.0.0.1:9090/health

# HTTPS with mTLS
curl --cert client.crt --key client.key --cacert server-ca.pem \
     -fsS https://127.0.0.1:9090/health
```

Cross-check the boot fingerprint against the on-disk cert:

```bash
openssl x509 -in /var/lib/thurvtl/tls/cert.pem -outform DER \
    | openssl dgst -sha256
# matches `fingerprint_sha256=...` from the journal
```

### Client-cert provisioning

When `client_ca_file` is set, every request must present a leaf cert
signed by that CA — so an operator running mTLS needs a way to mint
those client certs. Three minting paths follow; pick whichever fits
the PKI tooling already in place.

#### Self-signed CA + leaf with openssl

The starting point is a private CA. ECDSA P-256 keys are used
throughout, to match the daemon's own auto-gen server cert:

```bash
# Root CA (one-time)
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -new -x509 -key ca.key -out ca.pem -days 3650 \
    -subj "/CN=thur-admin-ca"

# Client leaf (one per operator / scraper)
openssl ecparam -name prime256v1 -genkey -noout -out client.key
openssl req -new -key client.key -out client.csr \
    -subj "/CN=prometheus-prod"
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key \
    -CAcreateserial -out client.pem -days 365 \
    -extfile <(printf "extendedKeyUsage=clientAuth")
```

The `extendedKeyUsage=clientAuth` extension is mandatory — rustls
rejects any client cert that omits it. Once minted, `ca.pem` goes to
the daemon as `http.tls.client_ca_file`, and `client.{key,pem}` go on
the calling machine. There is a working example in
[`vtl/scripts/test-smoke.sh`](../vtl/scripts/test-smoke.sh)
under `::test_http_mtls`.

#### cfssl

```bash
cfssl gencert -initca ca-csr.json | cfssljson -bare ca
cfssl gencert -ca=ca.pem -ca-key=ca-key.pem \
    -config=cfssl-config.json -profile=client \
    client-csr.json | cfssljson -bare client
```

The profile and CSR JSON schemas are documented upstream in the cfssl
docs. The only project-specific requirement is that `"client auth"`
appears in the profile's `usages` list.

#### cert-manager (Kubernetes)

In a Kubernetes environment, run one `Issuer` per CA and one
`Certificate` per client, then mount the resulting `Secret` into the
consumer Pod:

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: prometheus-thurvtl
spec:
  secretName: prometheus-thurvtl-tls
  issuerRef:
    name: thur-admin-ca
    kind: ClusterIssuer
  commonName: prometheus-prod
  usages:
    - client auth
  duration: 8760h    # 1y
```

The CA bundle the daemon trusts — mounted as `client_ca_file` — should
come from that same `Issuer`'s CA secret.

#### Key permissions and rotation

Client keys should be mode `0600`, owned by whichever user runs
Prometheus, curl, or the consumer. The CA cert is the trust anchor of
the whole arrangement: rotating it forces every client to re-roll, so
give it a validity measured in years. Leaf certs, by contrast, are
cheap to replace — re-mint them annually.

## NVMe/TCP TLS-PSK (VSA only)

When VSA is run over the NVMe/TCP transport — selected with
`transport: nvmetcp` — that transport can additionally run opt-in
TLS 1.3 with pre-shared keys, as specified in NVMe-TCP §3.6.1.5. The
full wire-flow and key-derivation design is documented in
[`NVMETCP.md`](NVMETCP.md) § TLS-PSK; this section covers only the
operator-facing schema.

The mode is selected in `thurvsa.yaml`:

```yaml
nvmetcp:
  # subnqn: "nqn.2025-10.com.metebalci:thurvsa"
  tls:
    mode: psk      # disabled (default) | psk
    # identity_file: "/etc/thurvsa/nvmetcp-psks.json"
```

One consequence of the design is worth flagging: the PSK derivation
binds to the subsystem NQN. Overriding `nvmetcp.subnqn` therefore
rederives every per-host PSK, and the host's
`nvme gen-tls-key --subsysnqn=` must be invoked with the matching
value or the handshake will not agree.

The host identities live in an identity file at
`<data_dir>/nvmetcp-psks.json` — daemon-managed, mode 0640, and
hand-edited today:

```json
{
  "version": 1,
  "psks": [
    {
      "host_nqn": "nqn.2014-08.org.nvmexpress:uuid:initiator-1",
      "interchange_key": "NVMeTLSkey-1:01:dGVzdF9rZXlfYnl0ZXNfaGVyZQ==:"
    }
  ]
}
```

There is one entry per host NQN. The `interchange_key` is the value
the host operator generates with `nvme gen-tls-key` and pastes in
verbatim; the daemon parses and CRC-validates it at startup, and
refuses to start on a malformed or duplicate entry. As with
`iscsi-users.json` next door, there is no hot reload — edits take
effect on the next restart.

The cipher suites are not configurable. They are hardcoded to the two
NVMe-TCP §3.6.1.5 mandates — `TLS_AES_128_GCM_SHA256` and
`TLS_AES_256_GCM_SHA384` — and a TLS 1.2 fallback is refused outright.
There is no operator knob.

On the host side, Linux setup is a single command per host:

```bash
# Generate the key, write it into the kernel keyring, capture the serial.
KEYSER=$(sudo nvme gen-tls-key \
    --hostnqn nqn.2014-08.org.nvmexpress:uuid:initiator-1 \
    --subsysnqn nqn.2025-10.com.metebalci:thurvsa \
    --hmac 1 --identity 1 --insert --keyring=.nvme)
# Connect through TLS using that serial.
sudo nvme connect -t tcp -a <vsa-host> -s 4420 \
    -n nqn.2025-10.com.metebalci:thurvsa \
    --hostnqn nqn.2014-08.org.nvmexpress:uuid:initiator-1 \
    --tls --tls-key=$KEYSER
```

Note that `nvme connect --tls` on Linux requires the `tlshd` userspace
TLS daemon — the kernel hands the TLS 1.3 handshake off to it. On
Debian and Ubuntu that ships in the `ktls-utils` package.

## Test-only failure injection (LocalBackend)

`LocalBackend` (`type: local`) reads the `THUR_CLOUD_INJECT_FAIL` env
var at construction time. When it is set, its value is a
comma-separated list of `kind@pattern` rules. On each operation the
backend short-circuits with a synthetic classified error, and that
error lands in the daemon log through the very same
`cloud_helpers::retry_async` path the real backends use — so the test
exercises the genuine error-handling code, not a separate stub.

The rule grammar has two parts:

- **kind** (case-insensitive) is one of `auth`, `authz` (alias:
  `permission`), `notfound`, `regionmismatch` (alias: `region`),
  `network`, `timeout`, or `other`. Each maps 1-to-1 onto a
  `FailureKind` and onto the retry classifier: the permanent kinds
  (`auth`, `authz`, `notfound`, `regionmismatch`) fail fast, while the
  transient kinds (`network`, `timeout`, `other`) consume the retry
  budget.
- **pattern** is a dumb glob: `*` matches everything, `prefix*` is a
  prefix match, `*suffix` a suffix match, `*middle*` a contains match,
  and anything else is matched exactly.

```bash
# Make every chunk upload fail as if creds were revoked.
THUR_CLOUD_INJECT_FAIL="auth@chunks/*"

# Time out chunks but let manifests through (exercises the give-up path).
THUR_CLOUD_INJECT_FAIL="timeout@chunks/*"

# Multiple rules: first match wins.
THUR_CLOUD_INJECT_FAIL="authz@manifests/*,notfound@indexes/*"
```

This is off by default and exists for the failure-path shell tests —
`vtl/scripts/test-backup-cloud-failures.sh` and
`vsa/scripts/test-fs-cloud-failures.sh`. Real cloud backends ignore
the env var entirely.
