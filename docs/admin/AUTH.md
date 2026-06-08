# Storage Authentication

This document explains how Thur VTL and Thur VSA authenticate to their
storage backends — S3, GCS, and Azure — and why credential
resolution is shaped the way it is. It is the authoritative reference
on the subject; other docs link here rather than repeat it.

At-rest encryption (volume / cartridge DEK keystores) is in
[`ENCRYPTION.md`](ENCRYPTION.md); network and admin-listener security
(admin HTTP TLS, the web-admin password, NVMe/TCP transport auth) is in
[`NETWORK_SECURITY.md`](NETWORK_SECURITY.md).

Every storage backend the daemon talks to is a named definition under
`storage.backends:` in the daemon's YAML conffile, which is
`/etc/thurvtl/thurvtl.yaml` on the VTL side and
`/etc/thurvsa/thurvsa.yaml` on the VSA side. The workflow is the same
each time: edit the conffile, restart the daemon, and the backends
come up live. The installed conffile already carries a commented-out
example of every provider type, so the starting point is to uncomment
and adapt.

The top-level YAML looks like this:

```yaml
storage:
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

Any `storage.backends:` entry may carry an `auth:` block that pins its
credentials. Adding that block is a **strict override** — once it is
present, the daemon ignores every env var, instance identity, and SDK
chain for that one backend. Other backends are untouched, so the
override is local to the entry it appears on.

The reason this matters is that storage SDK credential chains are
process-global. The AWS chain, for example, reads `AWS_ACCESS_KEY_ID`
once per process. That works fine for a single backend, but it breaks
the moment two S3-flavored providers share one daemon: whichever
credentials win the chain race end up applying to every backend. A
per-backend `auth:` block sidesteps the problem by injecting explicit
credentials directly into the SDK, so the global chain is never
consulted for that backend.

### S3 (`type: s3`)

```yaml
storage:
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
storage:
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
storage:
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
`shared/object-store/src/azure.rs::discover_credentials_from_env`. So that the
choice is never a mystery, the daemon logs which rung it picked at
startup and warns whenever a higher-precedence env var has shadowed the
one the operator expected.

## Managed-identity installs (instance and workload identity)

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
| Mixed: one managed-identity-native + one external | Native backend gets no `auth:` block (instance identity); external backend gets `auth: { type: env, ... }` with creds in `thurvtl.env`. |

## File permissions

Three files routinely hold storage-auth secrets, and all three should be
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

## Test-only failure injection (LocalBackend)

`LocalBackend` (`type: local`) reads the `THUR_STORAGE_INJECT_FAIL` env
var at construction time. When it is set, its value is a
comma-separated list of `kind@pattern` rules. On each operation the
backend short-circuits with a synthetic classified error, and that
error lands in the daemon log through the very same
`object_store_helpers::retry_async` path the real backends use — so the test
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
THUR_STORAGE_INJECT_FAIL="auth@chunks/*"

# Time out chunks but let manifests through (exercises the give-up path).
THUR_STORAGE_INJECT_FAIL="timeout@chunks/*"

# Multiple rules: first match wins.
THUR_STORAGE_INJECT_FAIL="authz@manifests/*,notfound@indexes/*"
```

This is off by default and exists for the failure-path shell tests —
`vtl/scripts/test-backup-storage-failures.sh` and
`vsa/scripts/test-fs-storage-failures.sh`. Real storage backends ignore
the env var entirely.
