# Kubernetes CSI driver

Thur VSA presents block volumes over iSCSI backed by the dedup chunk pool.
Without a CSI driver an operator has to create each volume, wire up CHAP
admission, and run `iscsiadm` on the node by hand. The CSI driver makes VSA a
first-class Kubernetes storage class instead: a `PersistentVolumeClaim`
provisions a VSA volume, attaches it to the scheduled node over iSCSI, mounts it
into the pod, and tears all of that down when the claim is deleted — with
snapshots, clones, and online expansion along the way, and no operator in the
loop.

This document is the design walkthrough: where the driver lives, how it talks to
the daemon, the per-node CHAP isolation model, the RPC-to-admin-call mapping,
and how it is deployed and released. The driver is issue #15.

## Where it lives

The driver is a **self-contained Go module under `csi/`** in this otherwise-Rust
monorepo (module `github.com/metebalci/thur/csi`). The cargo workspace
`members` list is explicit — no globs — so a `csi/` directory with its own
`go.mod` is invisible to cargo and harmless to the Rust build. It ships as a
container image and a Helm chart on their own `csi-v*` tag cadence, independent
of the daemon's SemVer (see [§ Release](#release)).

A single binary (`cmd/thurvsa-csi`) serves all three CSI gRPC services; `--mode`
selects which:

- `--mode=controller` runs co-located on the VSA appliance node. It implements
  the Controller service (provision, attach, snapshot, expand) by calling the
  daemon's admin socket directly. One replica.
- `--mode=node` runs as a DaemonSet on every worker that may mount a volume. It
  implements the Node service: it logs into the target over iSCSI with the
  node's CHAP credentials and mounts the LUN. It never touches the admin
  socket.
- `--mode=all` serves everything in one process (tests, single-node clusters).

The Identity service is always served.

### Package layout

```
csi/
  cmd/thurvsa-csi/        the binary + flags
  pkg/driver/             Identity / Controller / Node gRPC services + glue
                          (controller, publish, snapshots, expand, node,
                          chap, k8ssecrets, capabilities, naming, params)
  pkg/vsa/                transport-agnostic Go client for the admin API
  pkg/iscsi/              node-side iscsiadm + by-path device resolution
  pkg/grpcserver/         the gRPC listener
  deploy/helm/thurvsa-csi the chart; deploy/examples/ standalone manifests
  test/fake/              an in-memory fake daemon for unit tests
```

## Talking to the daemon

The controller speaks to a running `thurvsad` over its **admin Unix socket**
(`/run/thurvsa/admin.sock`, mode 0660, SO_PEERCRED), not by shelling out to the
`thurvsa` CLI. `pkg/vsa` is a small HTTP/JSON client whose request and response
types mirror the daemon's wire structs exactly. The transport is deliberately
isolated in `pkg/vsa/transport.go`: the unix dialer is the one swap point, so
moving to a future TCP network admin API is a dialer plus an auth header, not a
rewrite of the call sites.

The mutating admin verbs the driver depends on are a documented contract.
Rather than extend [`openapi.yaml`](../reference/openapi.yaml) — which is deliberately the
**read-only network** spec (#12), with mutations declared out of scope — the
driver's contract lives in its own [`openapi-admin.yaml`](../reference/openapi-admin.yaml).
A guard test (`vsa/daemon/tests/admin_openapi_sync.rs`) holds an allowlist of
exactly the ten routes the driver uses and asserts each is a real route in
`admin/mod.rs` *and* that the spec documents exactly that subset — so renaming
or dropping a route the driver consumes turns the build red without dragging the
entire admin surface (NVMe-TCP, DH-HMAC-CHAP, …) into the spec.

### VolumeId is the volume name

The CSI `VolumeId` is the **VSA volume name**, not the volume's uuid. The admin
API is entirely name-keyed — the uuid is only ever returned, never accepted as
input — and the driver derives the name deterministically from the immutable PVC
UID (`pvc-<uid>`, already a valid VSA name). That name is therefore as stable and
unique as the uuid would be, while letting every later controller op address the
daemon directly with no uuid→name lookup. A `SnapshotId` is `"<volume>/<snapshot>"`
for the same reason. (The original plan named the uuid as the VolumeId; this is
the one deliberate deviation, recorded here.)

## Per-node CHAP isolation

A single shared iSCSI target serves every volume, so the driver needs a fence
that stops one node from seeing another's LUN. iSCSI keys a session by
`(target IQN, portal)`, and every VSA volume shares one IQN — so a node holds
**one** session, and every LUN it mounts has to ride that single session, under
a single CHAP identity. The model is therefore **one CHAP user per node**,
admitted to every volume that node mounts:

- `ControllerPublishVolume` ensures a CHAP user named `csi-node-<nodeID>`
  (hashed if it would exceed the daemon's 256-byte username cap) and grants it
  the volume being published. The first publish to a node creates the user
  (admitted to that one volume); each later publish hits a 409 and falls through
  to a `…/grant` that adds the new volume to the node's admission set. It returns
  `{iqn, portal, lun, chapUser, chapSecret}` in the `PublishContext`.
- The 32-byte secret is **per node**, persisted in a Kubernetes Secret
  (`thurvsa-chap-node-<hash>`, in the driver's own namespace). This is what makes
  a retried publish idempotent: the external-attacher can re-drive
  `ControllerPublishVolume`, or the controller can restart, and the call must
  return the *identical* secret the daemon's CHAP user holds — read back from the
  Secret — or the node's login would desync. The Secret, not the daemon, is the
  source of truth for the secret value.
- `ControllerUnpublishVolume` revokes just *this* volume from the node's CHAP
  user (`…/revoke`). While the node still has other volumes, the revoke
  succeeds and the user (and its Secret) stay. When this was the node's last
  volume the daemon refuses to empty the admission set (400/409), which is the
  signal to `…/remove` the user and delete the Secret.
- `DeleteVolume` does **not** touch CHAP users — they belong to the node, not
  the volume, and may admit other volumes. `ControllerUnpublishVolume` owns the
  user's lifecycle.
- The node's CHAP user + Secret are **shared across all that node's volumes**,
  but the external-attacher serializes only per *volume*, so the controller
  holds a **per-node mutex** spanning the whole
  `ensure + AddUser/Grant` (publish) and `revoke + RemoveUser + chap.remove`
  (unpublish) sequence. Without it, a publish of one volume could interleave
  with the unpublish of another on the same node, deleting+recreating the user
  out from under the Secret store and silently desyncing the node's CHAP
  password (issue #148).

Because a node's volumes are admitted incrementally to one already-connected
session, the daemon's per-CHAP-user admission is **dynamic**: a `grant` reaches
sessions that are already up, and the daemon raises a REPORTED LUNS DATA HAS
CHANGED Unit Attention so the node re-reads REPORT LUNS. The node's stage path
issues an explicit SCSI rescan after login for the same reason. See
[`docs/reference/CONFORMANCE_SCSI.md`](../reference/CONFORMANCE_SCSI.md) § SBC-3 — dynamic LUN
admission.

The secret store is an interface. The Kubernetes-backed store is the default;
`--chap-secret-store=memory` selects an in-process store for csi-sanity and
non-cluster runs. The chart grants the driver `get`/`create`/`delete` on Secrets
in its namespace (a namespaced Role, not a ClusterRole — the blast radius is one
namespace).

> The CHAP secret travels in the `PublishContext`, which the external-attacher
> persists in the `VolumeAttachment` object. Anyone with RBAC on
> volumeattachments in the cluster can therefore read it — the standard
> trade-off for iSCSI CSI drivers. A per-node secret means such a read exposes
> the volumes mounted on that one node (not the whole cluster), and the node is
> the meaningful isolation boundary in Kubernetes (a pod is already scheduled to
> exactly one node).

> **Node-local exposure (issue #295).** `NodeStageVolume` writes the CHAP
> secret into the iSCSI node DB by passing it as an `iscsiadm … -o update -n
> node.session.auth.password -v <secret>` argument. With the default
> `--host-iscsiadm=true`, that command runs under `nsenter --target 1` in the
> **host** PID namespace, where `/proc/<pid>/cmdline` is world-readable: an
> unprivileged process or a `hostPID` pod on the node can poll cmdline during
> any stage / block-publish / expand and capture the node's CHAP
> username+secret. Because the one per-node credential admits **every** volume
> published to that node, a local foothold on the node can escalate to iSCSI
> access to all of the node's volumes from anywhere routable to the portal —
> the same node-scoped blast radius as the `VolumeAttachment` exposure above,
> but reachable without cluster RBAC. The node is still the isolation boundary
> (no cross-node escalation), and a host-root foothold already has the node's
> volumes mounted; the gap is specifically an *unprivileged* node-local reader.
> The planned hardening is to write the secret into the node DB record file
> under `/etc/iscsi/nodes/…` directly (root-only) instead of on the argv, so it
> never appears in any process command line. Until then, avoid scheduling
> untrusted `hostPID` workloads on nodes that mount thurvsa volumes.

## RPC → admin-call mapping

| CSI RPC | Admin call(s) | Notes |
| --- | --- | --- |
| `CreateVolume` | `POST /volumes` | name `pvc-<uid>`, sector-rounded size; 409 → `GET /volumes` and reconcile (size match = success, mismatch = `ALREADY_EXISTS`) |
| `CreateVolume` (from source) | `POST /volumes/:src/clone` | `content_source` snapshot (`<vol>/<snap>`) or volume |
| `DeleteVolume` | `DELETE /volumes/:name` | 404 tolerated; CHAP users are per-node, reaped by unpublish |
| `ControllerPublishVolume` | ensure per-node CHAP Secret, `POST /iscsi/users` (409 → `…/grant` this volume) | returns `PublishContext` |
| `ControllerUnpublishVolume` | `…/revoke` this volume → on last volume (400/409) `…/remove` + delete Secret | needs `node_id`; idempotent |
| `CreateSnapshot` / `DeleteSnapshot` | `POST` / `DELETE /volumes/:src/snapshots[/:snap]` | `SnapshotId = <vol>/<snap>`; 409 → list-and-return |
| `ControllerExpandVolume` | `POST /volumes/:name/resize` | grow-only; ≤current is an idempotent no-op; `node_expansion_required=true` |
| `NodeStageVolume` | — (iscsiadm) | attach + format + mount; persists the connector |
| `NodePublishVolume` | — | bind-mount staging → target (or device → target for raw block) |
| `NodeUnstageVolume` | — | unmount + iSCSI logout; reads the persisted connector |
| `NodeExpandVolume` | — | iSCSI rescan + `resize2fs`/`xfs_growfs` |

VSA volumes are thin and grow-only; the driver advertises `EXPAND_VOLUME` and
never issues a shrink (a request at or below the current size simply returns the
current size). Access modes are restricted to the single-node family — a volume
is mounted on one node at a time, on that node's single CHAP-authenticated
session.

## The node side

`NodeStageVolume` builds an `iscsi.Connector` from the `PublishContext` and runs
the attach through a `k8s.io/utils/exec` interface (so the exact `iscsiadm` argv
is unit-tested with a fake, no real `iscsiadm` needed): it creates the node DB
record directly from the IQN + portal (`-o new` — no SendTargets discovery,
which can stall against a CHAP-gated target), writes the three
`node.session.auth.*` CHAP fields, runs `--login` (an existing session is
tolerated), then forces a SCSI **rescan** (`-R`). The rescan is what surfaces a
LUN granted to the node's CHAP user *after* the session came up: `--login` is a
no-op on an existing session, and the daemon re-reads admission dynamically, so
the rescan makes the just-admitted LUN appear. The device is resolved from the
`/dev/disk/by-path/ip-<portal>-iscsi-<iqn>-lun-<lun>` symlink, which udev
creates a moment after the rescan, with a bounded poll. Because the daemon
reuses LUN numbers smallest-gap-first, the resolved device is then **verified
against the volume's identity**: the `PublishContext` carries the volume's SCSI
Unit Serial Number (the volume UUID, VPD 0x80), and the node forces a per-device
revalidate and compares it against `/sys/block/<dev>/device/vpd_pg80` before
returning. A mismatch (a stale device from a deleted volume that held the same
LUN) fails the stage rather than handing the new volume the wrong block device —
which could otherwise corrupt across volumes (issue #149). The volume is then
formatted and mounted to the staging path via `mount-utils` `SafeFormatAndMount`.

`iscsiadm` and the host `iscsid` must be the same open-iscsi version, and the
container's bundled copy rarely matches the host's, so by default the node runs
the *host's* `iscsiadm` via `nsenter` into PID 1's namespaces (the DaemonSet
sets `hostPID`); `--host-iscsiadm=false` falls back to the bundled binary.

`NodeUnstageVolume` receives no `PublishContext`, so the connector is persisted
to `--node-state-dir/<volid>.json` at stage time and read back at unstage. Since
every volume on a node shares the one session, unstage drops this volume's
connector file and logs out **only when it was the last** one (no `.json`
connector files remain) — logging out earlier would tear every other volume's
LUN off the node. Unstage also **deletes this LUN's kernel device**
(`/sys/block/<dev>/device/delete`) even when it does not log out, since
`iscsiadm -R` only discovers new LUNs and never removes a dropped one — a
lingering stale device is what the identity check above defends against
(issue #149). `NodeExpandVolume` reads the connector the same way to rescan the
session before growing the filesystem.

Raw-block volumes skip the stage-time format/mount; the device file is
bind-mounted to the target at `NodePublishVolume`, and node expansion is just
the rescan (no filesystem to grow).

The node image bundles its own `iscsiadm` and the `mkfs`/`mount`/`resize`
tools and runs privileged with `hostNetwork`, the host `/dev`, `/sys`,
`/etc/iscsi`, `/var/lib/iscsi`, and the kubelet plugin/pods directories mounted.
The container's `iscsiadm` talks to the **host's** `iscsid` over the shared host
network namespace, so the host must have open-iscsi installed and `iscsid`
running with the `iscsi_tcp` module available.

## Deployment

The Helm chart (`csi/deploy/helm/thurvsa-csi`) renders the `CSIDriver`, the
controller `Deployment`, the node `DaemonSet`, and RBAC.

The controller is pinned to the appliance node (a `nodeSelector` on a label the
operator applies) because it needs the local admin socket, which it
`hostPath`-mounts. The socket is peer-cred-gated, so the controller pod runs
with `runAsGroup` and `supplementalGroups` set to the host `thurvsa` group's
numeric gid — **a required Helm value with no default** (`thurvsaGid`): the
chart refuses to render without it, because a wrong or absent gid silently fails
admission. Find it with `getent group thurvsa` on the appliance.

The controller pod also runs the standard sig-storage sidecars (provisioner,
attacher, resizer, snapshotter, livenessprobe); the node pod runs the
node-driver-registrar and livenessprobe. The chart ships optional example
`StorageClass` and `VolumeSnapshotClass` (off by default — most operators bring
their own); `deploy/examples/` has standalone PVC/Pod and snapshot/clone
manifests. The portal address (which must be routable from every worker) is a
StorageClass parameter, with a `--target-portal` flag override.

## Testing

Unit tests run against `test/fake/fakedaemon.go`, an in-memory stand-in for the
admin socket on a real Unix socket that mirrors the daemon's status codes (409
on duplicate, 404 on missing, 400 on a CHAP rule violation). The controller
handlers are tested against it for idempotency, the publish secret-reuse and
revoke→remove fallthrough, snapshot/clone id round-trips, and shrink rejection.
The node side is tested with a fake `exec.Interface` (asserting the exact
`iscsiadm` argv) and fakes for the mounter and the Kubernetes Secret store
(client-go's fake clientset).

On top of the unit tests, **csi-sanity** (`kubernetes-csi/csi-test`) runs the
official CSI conformance suite against the whole Identity + Controller + Node
surface, wired to the fake daemon with the node-side fakes
(`pkg/driver/sanity_test.go`, part of `go test ./...`). It validates the gRPC
contract — capabilities, response fields, idempotency, error codes — without a
real iscsiadm, mount, or cluster. Two notes fell out of making it pass:

- The CSI snapshot *Name* is a global idempotency key, so the same name on a
  different source volume must be `ALREADY_EXISTS`. The daemon scopes snapshot
  names per volume, so the driver enforces global uniqueness with a cross-volume
  lookup before creating (the external-snapshotter mints unique names, so this
  is a correctness guard, not a hot path).
- One sanity spec is skipped: "ControllerPublishVolume should fail when the node
  does not exist". Publish mints (or reuses) a per-node CHAP user keyed by the
  node id and grants it the volume — the node id is a credential key, not a
  handle into a node registry, so there is nothing to validate it against.

The full lifecycle on a real cluster (the iSCSI + filesystem data path
csi-sanity's fakes can't cover) is the gated e2e suite (`csi/test/e2e`, issue
#15 M10).

## Release

The driver releases on its own `csi-v*` tags, independent of the daemon's `v*`
SemVer (see [RELEASING.md](../dev/RELEASING.md) § CSI driver). A tag publishes a
multi-arch image to `ghcr.io/<owner>/thurvsa-csi:<version>` and the Helm chart
as an OCI artifact to `ghcr.io/<owner>/charts/thurvsa-csi:<version>`, via
[`.github/workflows/csi-release.yml`](../../.github/workflows/csi-release.yml). The
per-PR gate is [`.github/workflows/csi.yml`](../../.github/workflows/csi.yml)
(path-scoped to `csi/**`): gofmt, vet, staticcheck, build, test, plus `helm
lint`/`template` and `kubeconform` on the rendered manifests.
