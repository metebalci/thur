# thurvsa-csi

Kubernetes CSI driver for **Thur VSA**. Provisions VSA volumes for
`PersistentVolumeClaim`s and attaches them to pods over iSCSI, with snapshots,
clones, and online expansion.

A single binary serves the CSI Identity service plus, by `--mode`, the
Controller and/or Node services:

- **controller** — runs co-located on the VSA appliance node; talks to the
  `thurvsad` admin unix socket to create/delete/resize/snapshot/clone volumes
  and mint per-volume CHAP credentials.
- **node** — runs on every worker; logs in over iSCSI with the per-volume CHAP
  credentials and mounts the device into the pod.

This is a self-contained Go module under the Thur monorepo; it has its own
`go.mod` and is invisible to the Rust cargo workspace. Design and operator docs
live in [`../docs/CSI.md`](../docs/CSI.md).

## Build

```bash
make build      # -> bin/thurvsa-csi
make test
make vet
```
