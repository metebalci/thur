# CSI driver end-to-end test (M10)

`lifecycle.sh` drives the full PVC lifecycle against a cluster that already has
the driver installed:

    provision → attach + mount → write → snapshot → clone-from-snapshot →
    verify the file in the clone → online-expand → verify the larger fs in the
    running pod → delete → assert the PVs are reclaimed.

It is pure `kubectl` and asserts only cluster-observable state, so it runs
unchanged against a kind cluster (the `csi-e2e.yml` workflow) or any real
cluster wired to a `thurvsad`.

## Required cluster topology

`lifecycle.sh` assumes the cluster is already provisioned with:

1. **A running `thurvsad`** reachable from the workers over iSCSI (the daemon's
   `iscsi.auth.method: CHAP`, a `local` storage backend is fine for the test).
2. **The driver installed** — controller (on the appliance node, with the
   admin socket hostPath-reachable and the right `thurvsaGid`) and the node
   DaemonSet Running. Workers need the host iSCSI stack (open-iscsi + `iscsid`
   + the `iscsi_tcp` module).
3. **A `StorageClass` named `thurvsa`** with `allowVolumeExpansion: true` and a
   `targetPortal` routable from the node(s).
4. **The external-snapshotter** CRDs + controller installed, and a
   **`VolumeSnapshotClass` named `thurvsa`**.

Override the names via env: `SC`, `VSC`, `NS`, `TIMEOUT`. `KEEP=1` leaves the
test namespace for inspection.

```bash
NS=thurvsa-e2e SC=thurvsa VSC=thurvsa ./lifecycle.sh
```

## Automated run (kind)

[`.github/workflows/csi-e2e.yml`](../../../.github/workflows/csi-e2e.yml) brings
the whole topology up on a single-node kind cluster — modprobe `iscsi_tcp` on
the runner, build + `kind load` the driver image, run `thurvsad` as a privileged
hostNetwork pod, `helm install` the chart pointed at the node IP, install the
snapshotter, then run `lifecycle.sh`.

The workflow is **gated to `workflow_dispatch`** (manual) rather than a nightly
schedule: iSCSI inside kind-in-docker depends on the runner's kernel modules and
is environment-sensitive, so the bring-up is expected to need iteration against
real runs before it is promoted to a schedule. The reusable, cluster-agnostic
assertions live in `lifecycle.sh`; only the bring-up is kind-specific.
