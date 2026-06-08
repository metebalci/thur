# Network & Admin Security

How to secure the daemons' network-facing surfaces: TLS on the admin
HTTP listener, the web-admin password gate, and the VSA NVMe/TCP
transport authentication options (TLS-PSK and DH-HMAC-CHAP).
Storage-backend credentials are in [`AUTH.md`](AUTH.md); at-rest
encryption is in [`ENCRYPTION.md`](ENCRYPTION.md).

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
[`vtl/scripts/test-smoke.sh`](../../vtl/scripts/test-smoke.sh)
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

## Admin password (Web UI / HTTP listener gate)

The admin Unix socket and the admin HTTP listener authenticate
differently because they have different things to lean on. The socket
at `/run/<product>/admin.sock` is local-only and peer-cred-authed: the
kernel hands the daemon the connecting process's uid/gid over
`SO_PEERCRED`, so membership in the daemon's group is the credential and
nothing crosses the network. The HTTP listener has none of that. It is a
TCP socket reachable from anywhere the operator pointed it, so there is
no peer identity to trust. What it serves is read-only — live session
and topology status plus the read-only Web UI (issue #5; mutating forms
were considered and rejected, issue #91) — but read-only is not the same
as public: the audit tail, session identities, and inventory it exposes
are confidential. The admin password is the gate that keeps them off an
open TCP port. It is **optional** (see _The gate is optional_ below): on
an isolated management network the network itself is the boundary, the
same posture the unauthenticated iSCSI data plane defaults to.

The model is deliberately the one a network printer or a home router
uses: a **single shared password**, not a directory of accounts. There
is one secret for the whole appliance, it authenticates against a fixed
synthetic username `webadmin`, and there are no per-operator logins,
roles, or groups. LDAP, OIDC, SAML, and RBAC are explicitly out of
scope — they are the wrong weight for a self-hosted single-appliance
audience, where the operator who can set the password is already the
operator who can read the conffile and restart the daemon. One password,
one appliance.

### Setting the password

```bash
# Interactive — prompts twice, no echo.
sudo -u thurvsa thurvsa system set-admin-password

# Non-interactive provisioning — read from the per-product env var.
THURVSA_ADMIN_PASSWORD='...' sudo -u thurvsa thurvsa system set-admin-password
```

`<product> system set-admin-password` is **daemon-routed**: the daemon
owns the on-disk store, so the verb refuses if the daemon is down. It
prompts twice with no echo, or reads the per-product env var
`THURVTL_ADMIN_PASSWORD` / `THURVSA_ADMIN_PASSWORD` for non-interactive
provisioning. The plaintext travels only over the local peer-cred admin
socket; the daemon hashes it **server-side** with Argon2id (the
OWASP-baseline parameters m=19456, t=2, p=1) and stores only the
resulting self-describing PHC string. The plaintext never lands on disk
and never leaves the host. The change is effective immediately by an
arc-swap of the live verifier — no restart, and the same in-process
verifier is shared between the admin socket and the HTTP listener so the
two can never disagree about the current password.

The hash lives at `<data_dir>/admin-password.json`, daemon-managed
(written by the daemon on `set`, never by the packager — there is no
postinst entry for it), mode 0640, written by atomic rename, a sibling
of `iscsi-users.json` next door. Its schema holds the hash and nothing
else:

```json
{ "phc": "<Argon2id PHC string>", "updated_at": "<RFC3339>" }
```

An **absent file means no password is configured**, and when the gate is
enabled (`http.auth.method: Password`) it fails closed in that state —
see the 503 verdict below. There is no plaintext anywhere in the file, so
a stolen `admin-password.json` yields only an Argon2id hash to grind, not
the password.

### The gate is optional (`http.auth.method`)

Whether the protected routes actually require the password is controlled
by `http.auth.method`, which mirrors the opt-out shape of
`iscsi.auth.method`:

- **`None` (default)** — no authentication; the protected routes are
  served open. This is the right setting when the listener lives on an
  isolated / trusted management network, where the network boundary is
  the gate — the same default the iSCSI data plane uses
  (`iscsi.auth.method: None`), and a far smaller exposure than
  unauthenticated iSCSI, since the HTTP surface is read-only metadata,
  not bulk data. `/health` + `/metrics` are open either way.
- **`Password`** — require the single shared web-admin password over
  HTTP Basic. With no password configured the gate fails closed (503).
  Pair it with `http.tls` (below) so the password is not sent in clear.

Because the default is `None`, a fresh install serves its read-only
console without a password. The **method**, not the presence of a
password file, is what turns the gate on: if you set a password but leave
`http.auth.method: None`, the password is *not* enforced and the daemon
warns about that at startup.

### Open vs protected routes

The HTTP listener always splits its router into two groups; whether the
protected group is gated depends on `http.auth.method` above:

- **Open**, unauthenticated: `/health` and `/metrics`. These stay open
  so a Prometheus scrape and a liveness probe keep working without
  credentials, exactly as before.
- **Protected**: everything else — `/sessions`, `/info`, and the
  read-only Web UI (`/ui` + the read-only `/api/v1` GET subset). Gated by
  HTTP Basic only when `http.auth.method: Password`; under the default
  `None` it is served open.

When `http.auth.method: Password`, the Basic challenge uses the fixed
username `webadmin` and the realm `thur admin`, and the middleware
returns one of three verdicts (under `None` the protected group passes
through unconditionally):

| State | Response |
| --- | --- |
| No password configured | `503` + `WWW-Authenticate` challenge |
| Missing / malformed / wrong credentials | `401` + `WWW-Authenticate` challenge |
| Valid credentials | request passes through |

The `503`-versus-`401` split is deliberate, so an operator can tell
"the appliance has no admin password set yet" apart from "I typed the
wrong password." On a valid request the middleware also stamps a
`shared_audit::AuditActor::rest("webadmin", <peer ip:port>)` descriptor
into the request extensions. It is currently unconsumed — the Web UI is
read-only, so nothing on the TCP listener performs an audited mutation
(mutating forms were rejected, issue #91); the descriptor stays in place
in case a future authenticated write path needs to attribute itself.

**Behavior change.** `/sessions` and `/info` were *unauthenticated*
before this landed; they are now behind the gate. That is intended —
those endpoints leak live session and topology detail that has no
business being world-readable on a TCP port — and it follows the
project's no-backward-compatibility rule. `/metrics` and `/health`
remain open for Prometheus and liveness compatibility.

### Use TLS so the password is not sent in clear

HTTP Basic credentials are **base64-encoded, not encrypted** — base64 is
trivially reversible, so on a plaintext listener the `webadmin` password
crosses the wire in effectively cleartext on every protected request.
The strong recommendation is therefore to enable the admin HTTP TLS
listener (§ Admin HTTP TLS above, the `http.tls` block) before relying on
the gate over anything but loopback. This is an operational
recommendation, not a code knob: the daemon does not refuse Basic auth
over plaintext, it trusts the operator to put TLS in front of it.

### Audit

Every successful set emits an audit row under the op name
`system.admin_password.set`. The params carry **no secret** — only the
peer-cred descriptor of the CLI caller that performed the change, so the
log records *who* reset the admin password and *when*, never *to what*.

## NVMe/TCP TLS-PSK (VSA only)

When VSA is run over the NVMe/TCP transport — enabled by listing
`nvmetcp` in `transports:` — that transport can additionally run opt-in
TLS 1.3 with pre-shared keys, as specified in NVMe-TCP §3.6.1.5. The
full wire-flow and key-derivation design is documented in
[`NVMETCP.md`](../reference/NVMETCP.md) § TLS-PSK; this section covers only the
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

### Per-host volume admission (VSA only)

When TLS-PSK is on (`nvmetcp.tls.mode: Psk`), every entry in
`nvmetcp-psks.json` **must** carry a non-empty `volumes` field
naming the subset of volumes the host is admitted to — admission is
mandatory:

```json
{
  "version": 1,
  "psks": [
    {
      "host_nqn": "nqn.2014-08.org.nvmexpress:uuid:initiator-1",
      "interchange_key": "NVMeTLSkey-1:01:...:",
      "volumes": ["alpha", "beta"]
    }
  ]
}
```

The dispatcher filters Identify CNS=0x02 (Active NS List), CNS=0x00
(Namespace), and CNS=0x03 (NS ID Descriptor List) to those names,
and rejects per-NSID I/O against non-admitted namespaces with
`INVALID_NAMESPACE`. The admission set is captured at Fabrics
Connect from the in-band host NQN; volumes created after a host
connects are invisible to existing connections until they
reconnect.

Provision via `thurvsa nvmetcp psks add --host-nqn ... --key ...
--volume NAME [--volume NAME ...]` (at least one `--volume`
required). Mutate post-creation with `nvmetcp psks grant` /
`revoke` — both refuse if the operation would empty the volume set
(use `disable` / `remove` for full cutoff). The daemon rejects
unknown volume names at `add` / `grant` time.

Pre-existing entries without `volumes` (created before the
mandatory-admission rollout) are still loaded by the TLS handshake
path but resolve to an empty admission set post-Connect — those
hosts authenticate but see no namespaces. The daemon logs
`admission_fenced` per connection so operators can audit which
hosts need re-provisioning.

Plaintext mode (`nvmetcp.tls.mode: Disabled`) skips admission
entirely — connections see every namespace, same shape as iSCSI
no-CHAP. The host NQN in the Connect command is unauthenticated in
plaintext mode anyway, so admission would be advisory at best;
pairing admission with the TLS auth layer keeps the model simple.

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

## NVMe/TCP DH-HMAC-CHAP (VSA only)

DH-HMAC-CHAP is in-band host authentication — the NVMe analog of the
iSCSI CHAP described above, and the common choice for Linux NVMe/TCP on
trusted networks (`nvme connect --dhchap-secret`). It authenticates the
host *without* requiring a TLS stack. The wire-flow and crypto design
are in [`NVMETCP.md`](../reference/NVMETCP.md) § DH-HMAC-CHAP; this section is the
operator-facing schema.

It is selected by its own block, independent of `tls`:

```yaml
nvmetcp:
  auth:
    mode: dhchap   # none (default) | dhchap
    # identity_file: "/etc/thurvsa/nvmetcp-dhchap.json"
```

The three postures: `auth.mode: none` (+ `tls.mode: disabled`) is the
legacy cleartext default; `auth.mode: dhchap` alone authenticates the
host in-band over plain TCP; `auth.mode: dhchap` with `tls.mode: psk`
runs DH-HMAC-CHAP inside a TLS-PSK channel ("dhchap+tls"), getting both
an authenticated host *and* an encrypted data stream.

Per-host secrets live in an identity file at
`<data_dir>/nvmetcp-dhchap.json` — daemon-managed, mode 0640, atomic
save. Unlike the TLS-PSK file, it is **re-read on every Connect**, so
`nvmetcp dhchap` edits take effect on the next session with no restart:

```json
{
  "version": 1,
  "dhchap": [
    {
      "host_nqn": "nqn.2014-08.org.nvmexpress:uuid:initiator-1",
      "dhchap_key": "DHHC-1:01:base64key...:",
      "volumes": ["alpha", "beta"]
    }
  ]
}
```

The `dhchap_key` is the `DHHC-1:NN:<base64(key‖crc32)>:` string the host
operator generates with `nvme gen-dhchap-key`; the `NN` selects the
key-transform hash (`00` = none, `01`/`02`/`03` = SHA-256/384/512) and
the CRC is validated at parse. As with TLS-PSK, every entry **must**
carry a non-empty `volumes` admission set — admission is mandatory under
`auth.mode: dhchap`, gating every I/O command (the host NQN is now
authenticated, so the fence is meaningful). A revoke that would empty
the set is refused.

Provision and manage with the `nvmetcp dhchap` verbs (daemon-routed,
audited):

```bash
# Add a host secret + admit it to one or more volumes.
thurvsa nvmetcp dhchap add \
    --host-nqn nqn.2014-08.org.nvmexpress:uuid:initiator-1 \
    --key "DHHC-1:01:base64key...:" \
    --volume alpha --volume beta

thurvsa nvmetcp dhchap list [--json]
thurvsa nvmetcp dhchap grant  --host-nqn ... --volume gamma
thurvsa nvmetcp dhchap revoke --host-nqn ... --volume beta
thurvsa nvmetcp dhchap disable/enable/remove --host-nqn ...
# Rotate with a grace window (both secrets authenticate until it expires).
thurvsa nvmetcp dhchap rotate --host-nqn ... --key "DHHC-1:01:...:" --grace 24h
thurvsa nvmetcp dhchap rotate --host-nqn ... --cancel
```

On the host side:

```bash
# Generate a host secret (paste the printed DHHC-1 string into `add`).
nvme gen-dhchap-key --hostnqn nqn.2014-08.org.nvmexpress:uuid:initiator-1
# Connect with in-band auth (no TLS / tlshd needed).
sudo nvme connect -t tcp -a <vsa-host> -s 4420 \
    -n nqn.2025-10.com.metebalci:thurvsa \
    --hostnqn nqn.2014-08.org.nvmexpress:uuid:initiator-1 \
    --dhchap-secret "DHHC-1:01:base64key...:"
```

### Bidirectional (mutual) authentication

To also authenticate the *controller* to the host, configure a
controller secret per host and have the host pass `--dhchap-ctrl-secret`:

```bash
# Controller side — set the controller secret for a host.
thurvsa nvmetcp dhchap set-ctrl-key --host-nqn ... --key "DHHC-1:02:...:"
# (or pass --ctrl-key on `add`; clear with `clear-ctrl-key`).

# Host side — supply both secrets.
sudo nvme connect -t tcp -a <vsa-host> -s 4420 -n <subnqn> \
    --hostnqn <hostnqn> \
    --dhchap-secret "DHHC-1:01:...:" \
    --dhchap-ctrl-secret "DHHC-1:02:...:"
```

The controller then proves itself with the response `R2` in the
Success1 message. A host that requests mutual auth against a host entry
with no controller secret configured is refused. The `list` output's
`MUTUAL` column shows which hosts have a controller secret set (the
secret itself is never returned).

