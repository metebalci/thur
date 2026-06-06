#!/usr/bin/env bash
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end lifecycle check for the Thur VSA CSI driver (issue #15, M10).
#
# Drives the full PVC lifecycle against an ALREADY-PROVISIONED cluster:
#   provision -> attach + mount -> write -> snapshot -> clone-from-snapshot
#   -> verify the file in the clone -> online-expand -> verify the larger fs
#   in the running pod -> delete -> assert the PV is gone.
#
# Prerequisites (the csi-e2e.yml workflow sets these up; see ../e2e/README.md):
#   - kubectl context points at the target cluster
#   - the thurvsa-csi driver is installed (controller + node Running)
#   - a StorageClass named $SC exists (allowVolumeExpansion: true) whose
#     targetPortal is reachable from the node(s)
#   - the external-snapshotter CRDs + controller are installed and a
#     VolumeSnapshotClass named $VSC exists
#
# Exit 0 on success; non-zero (set -e) on the first failed step. Cleans up its
# namespace on exit unless KEEP=1.

set -euo pipefail

NS="${NS:-thurvsa-e2e}"
SC="${SC:-thurvsa}"
VSC="${VSC:-thurvsa}"
TIMEOUT="${TIMEOUT:-180s}"

log() { echo "=== $*"; }

cleanup() {
  if [ "${KEEP:-0}" = "1" ]; then
    log "KEEP=1, leaving namespace $NS"
    return
  fi
  log "cleanup: deleting namespace $NS"
  kubectl delete namespace "$NS" --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

kubectl create namespace "$NS" >/dev/null 2>&1 || true

# ---- 1. provision + attach + mount + write ------------------------------------
log "1. provision PVC + pod, write a marker file"
kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: src
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ${SC}
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: writer
spec:
  containers:
    - name: app
      image: busybox:1.36
      command: ["sh", "-c", "echo thur-e2e-marker > /data/marker && sync && sleep 86400"]
      volumeMounts:
        - { name: vol, mountPath: /data }
  volumes:
    - name: vol
      persistentVolumeClaim:
        claimName: src
YAML
kubectl -n "$NS" wait --for=condition=Ready pod/writer --timeout="$TIMEOUT"
kubectl -n "$NS" exec writer -- cat /data/marker | grep -q thur-e2e-marker
log "   marker written and read back"

# ---- 2. snapshot --------------------------------------------------------------
log "2. snapshot the source PVC"
kubectl -n "$NS" apply -f - <<YAML
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: snap
spec:
  volumeSnapshotClassName: ${VSC}
  source:
    persistentVolumeClaimName: src
YAML
kubectl -n "$NS" wait --for=jsonpath='{.status.readyToUse}'=true \
  volumesnapshot/snap --timeout="$TIMEOUT"
log "   snapshot ready"

# ---- 3. clone from snapshot + verify the marker -------------------------------
log "3. clone a PVC from the snapshot and verify the marker survived"
kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: clone
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ${SC}
  resources:
    requests:
      storage: 1Gi
  dataSource:
    name: snap
    kind: VolumeSnapshot
    apiGroup: snapshot.storage.k8s.io
---
apiVersion: v1
kind: Pod
metadata:
  name: reader
spec:
  containers:
    - name: app
      image: busybox:1.36
      command: ["sh", "-c", "sleep 86400"]
      volumeMounts:
        - { name: vol, mountPath: /data }
  volumes:
    - name: vol
      persistentVolumeClaim:
        claimName: clone
YAML
kubectl -n "$NS" wait --for=condition=Ready pod/reader --timeout="$TIMEOUT"
kubectl -n "$NS" exec reader -- cat /data/marker | grep -q thur-e2e-marker
log "   marker present in the clone"

# ---- 4. online expand + verify the larger fs in the running pod ---------------
log "4. expand the source PVC to 2Gi and confirm the pod sees the larger fs"
kubectl -n "$NS" patch pvc src --type merge -p '{"spec":{"resources":{"requests":{"storage":"2Gi"}}}}'
# Wait for the resize to reach the live filesystem (capacity is updated after
# NodeExpandVolume runs).
for _ in $(seq 1 60); do
  cap=$(kubectl -n "$NS" get pvc src -o jsonpath='{.status.capacity.storage}' || true)
  [ "$cap" = "2Gi" ] && break
  sleep 5
done
[ "$cap" = "2Gi" ] || { echo "PVC capacity did not reach 2Gi (got '$cap')"; exit 1; }
# The in-pod filesystem should now report > 1Gi.
kb=$(kubectl -n "$NS" exec writer -- sh -c "df -k /data | awk 'NR==2{print \$2}'")
[ "$kb" -gt 1100000 ] || { echo "filesystem did not grow online (df 1k-blocks=$kb)"; exit 1; }
log "   pod sees the grown filesystem (${kb} 1k-blocks)"

# ---- 5. delete + assert the PV is reclaimed -----------------------------------
log "5. delete the workloads + claims and assert the PVs are reclaimed"
src_pv=$(kubectl -n "$NS" get pvc src -o jsonpath='{.spec.volumeName}')
clone_pv=$(kubectl -n "$NS" get pvc clone -o jsonpath='{.spec.volumeName}')
kubectl -n "$NS" delete pod writer reader --wait=true --timeout="$TIMEOUT"
kubectl -n "$NS" delete volumesnapshot snap --wait=true --timeout="$TIMEOUT"
kubectl -n "$NS" delete pvc src clone --wait=true --timeout="$TIMEOUT"
for pv in "$src_pv" "$clone_pv"; do
  for _ in $(seq 1 36); do
    kubectl get pv "$pv" >/dev/null 2>&1 || break
    sleep 5
  done
  if kubectl get pv "$pv" >/dev/null 2>&1; then
    echo "PV $pv was not reclaimed after PVC deletion"; exit 1
  fi
done
log "   PVs reclaimed"

log "PASS: full CSI lifecycle (provision, attach, snapshot, clone, expand, delete)"
