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
the daemon, the per-volume CHAP isolation model, the RPC-to-admin-call mapping,
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
  per-volume CHAP credentials and mounts the LUN. It never touches the admin
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
Rather than extend [`openapi.yaml`](openapi.yaml) — which is deliberately the
**read-only network** spec (#12), with mutations declared out of scope — the
driver's contract lives in its own [`openapi-admin.yaml`](openapi-admin.yaml).
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

## Per-volume CHAP isolation

A single shared iSCSI target serves every volume, so the driver needs a fence
that stops one node from seeing another's LUN. The model is **one CHAP user per
volume**, admitted only to that volume:

- `ControllerPublishVolume` mints a CHAP user named `csi-<volume>`, admitted to
  exactly that volume, and returns its credentials in the `PublishContext`
  (`{iqn, portal, lun, chapUser, chapSecret}`). The node logs in with those
  creds, and VSA's admission (`iscsi-users.json` `volumes:` array) means that
  session sees only its one LUN.
- The 32-byte secret is **persisted in a Kubernetes Secret**
  (`thurvsa-chap-<hash>`, in the driver's own namespace). This is what makes a
  retried publish idempotent: the external-attacher can re-drive
  `ControllerPublishVolume`, or the controller can restart, and the call must
  return the *identical* secret the daemon's CHAP user was created with — read
  back from the Secret — or the node's login would desync. The CHAP user create
  is `POST /iscsi/users`; a 409 (already exists) falls through to a grant, since
  the Secret, not the daemon, is the source of truth for the secret value.
- `ControllerUnpublishVolume` removes the volume from the CHAP user's admission
  set; since the user serves exactly one volume, the revoke always refuses to
  empty the set (the daemon returns 400) and `remove` is the terminal step. The
  revoke-first shape stays correct if a user ever carries extra volumes. The
  Secret is then deleted.
- `DeleteVolume` also reaps a lingering CHAP user and Secret, so a deleted
  volume can never leak a credential even if an unpublish was missed.

The secret store is an interface. The Kubernetes-backed store is the default;
`--chap-secret-store=memory` selects an in-process store for csi-sanity and
non-cluster runs. The chart grants the driver `get`/`create`/`delete` on Secrets
in its namespace (a namespaced Role, not a ClusterRole — the blast radius is one
namespace).

> The CHAP secret travels in the `PublishContext`, which the external-attacher
> persists in the `VolumeAttachment` object. Anyone with RBAC on
> volumeattachments in the cluster can therefore read it — the standard
> trade-off for iSCSI CSI drivers. Per-volume secrets keep the blast radius of
> such a read to a single volume.

## RPC → admin-call mapping

| CSI RPC | Admin call(s) | Notes |
| --- | --- | --- |
| `CreateVolume` | `POST /volumes` | name `pvc-<uid>`, sector-rounded size; 409 → `GET /volumes` and reconcile (size match = success, mismatch = `ALREADY_EXISTS`) |
| `CreateVolume` (from source) | `POST /volumes/:src/clone` | `content_source` snapshot (`<vol>/<snap>`) or volume |
| `DeleteVolume` | reap CHAP user + Secret, then `DELETE /volumes/:name` | 404 tolerated |
| `ControllerPublishVolume` | ensure CHAP Secret, `POST /iscsi/users` (409 → `…/grant`) | returns `PublishContext` |
| `ControllerUnpublishVolume` | `…/revoke` → (400/409) `…/remove`, delete Secret | idempotent |
| `CreateSnapshot` / `DeleteSnapshot` | `POST` / `DELETE /volumes/:src/snapshots[/:snap]` | `SnapshotId = <vol>/<snap>`; 409 → list-and-return |
| `ControllerExpandVolume` | `POST /volumes/:name/resize` | grow-only; ≤current is an idempotent no-op; `node_expansion_required=true` |
| `NodeStageVolume` | — (iscsiadm) | attach + format + mount; persists the connector |
| `NodePublishVolume` | — | bind-mount staging → target (or device → target for raw block) |
| `NodeUnstageVolume` | — | unmount + iSCSI logout; reads the persisted connector |
| `NodeExpandVolume` | — | iSCSI rescan + `resize2fs`/`xfs_growfs` |

VSA volumes are thin and grow-only; the driver advertises `EXPAND_VOLUME` and
never issues a shrink (a request at or below the current size simply returns the
current size). Access modes are restricted to the single-node family —
per-volume CHAP plus a single iSCSI session is single-attach by construction.

## The node side

`NodeStageVolume` builds an `iscsi.Connector` from the `PublishContext` and runs
the attach through a `k8s.io/utils/exec` interface (so the exact `iscsiadm` argv
is unit-tested with a fake, no real `iscsiadm` needed): SendTargets discovery,
the three `node.session.auth.*` CHAP fields written to the node DB, then
`--login` (an existing session is tolerated). The device is resolved from the
`/dev/disk/by-path/ip-<portal>-iscsi-<iqn>-lun-<lun>` symlink, which udev
creates a moment after login, with a bounded poll. The volume is then formatted
and mounted to the staging path via `mount-utils` `SafeFormatAndMount`.

`NodeUnstageVolume` receives no `PublishContext`, so the connector is persisted
to `--node-state-dir/<volid>.json` at stage time and read back to drive the
logout and node-record delete. `NodeExpandVolume` reads it the same way to
rescan the session before growing the filesystem.

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
  does not exist". The driver's attach is node-agnostic — publish just mints
  per-volume CHAP creds usable from any node — so the controller keeps no node
  registry to validate a node id against.

The full lifecycle on a real cluster (the iSCSI + filesystem data path
csi-sanity's fakes can't cover) is the gated e2e suite (`csi/test/e2e`, issue
#15 M10).

## Release

The driver releases on its own `csi-v*` tags, independent of the daemon's `v*`
SemVer (see [RELEASING.md](RELEASING.md) § CSI driver). A tag publishes a
multi-arch image to `ghcr.io/<owner>/thurvsa-csi:<version>` and the Helm chart
as an OCI artifact to `ghcr.io/<owner>/charts/thurvsa-csi:<version>`, via
[`.github/workflows/csi-release.yml`](../.github/workflows/csi-release.yml). The
per-PR gate is [`.github/workflows/csi.yml`](../.github/workflows/csi.yml)
(path-scoped to `csi/**`): gofmt, vet, staticcheck, build, test, plus `helm
lint`/`template` and `kubeconform` on the rendered manifests.
