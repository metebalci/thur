// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"testing"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func createSrcVolume(t *testing.T, cs *controllerServer, name string) {
	t.Helper()
	if _, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name: name, CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps(),
	}); err != nil {
		t.Fatalf("create source %q: %v", name, err)
	}
}

func TestCreateSnapshot(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-snap")

	resp, err := cs.CreateSnapshot(ctx, &csi.CreateSnapshotRequest{SourceVolumeId: "pvc-snap", Name: "snapshot-1"})
	if err != nil {
		t.Fatalf("CreateSnapshot: %v", err)
	}
	snap := resp.GetSnapshot()
	if snap.GetSnapshotId() != "pvc-snap/snapshot-1" {
		t.Errorf("snapshot id = %q, want pvc-snap/snapshot-1", snap.GetSnapshotId())
	}
	if snap.GetSourceVolumeId() != "pvc-snap" {
		t.Errorf("source volume id = %q", snap.GetSourceVolumeId())
	}
	if !snap.GetReadyToUse() {
		t.Errorf("snapshot not ready to use")
	}
	if snap.GetCreationTime() == nil {
		t.Errorf("creation time not set")
	}
}

func TestCreateSnapshotIdempotent(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-snap2")
	req := &csi.CreateSnapshotRequest{SourceVolumeId: "pvc-snap2", Name: "snapshot-x"}
	r1, err := cs.CreateSnapshot(ctx, req)
	if err != nil {
		t.Fatalf("first snapshot: %v", err)
	}
	r2, err := cs.CreateSnapshot(ctx, req)
	if err != nil {
		t.Fatalf("second snapshot: %v", err)
	}
	if r1.GetSnapshot().GetSnapshotId() != r2.GetSnapshot().GetSnapshotId() {
		t.Errorf("idempotent snapshot returned different ids: %q vs %q",
			r1.GetSnapshot().GetSnapshotId(), r2.GetSnapshot().GetSnapshotId())
	}
}

func TestCreateSnapshotMissingSource(t *testing.T) {
	cs := testController(t)
	_, err := cs.CreateSnapshot(context.Background(), &csi.CreateSnapshotRequest{SourceVolumeId: "absent", Name: "snapshot-z"})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("expected NotFound for missing source, got %v", err)
	}
}

func TestDeleteSnapshotIdempotent(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-snap3")
	if _, err := cs.CreateSnapshot(ctx, &csi.CreateSnapshotRequest{SourceVolumeId: "pvc-snap3", Name: "snapshot-d"}); err != nil {
		t.Fatalf("create snapshot: %v", err)
	}
	for i := 0; i < 2; i++ {
		if _, err := cs.DeleteSnapshot(ctx, &csi.DeleteSnapshotRequest{SnapshotId: "pvc-snap3/snapshot-d"}); err != nil {
			t.Fatalf("delete #%d must be idempotent, got %v", i, err)
		}
	}
}

func TestDeleteSnapshotMalformedId(t *testing.T) {
	cs := testController(t)
	if _, err := cs.DeleteSnapshot(context.Background(), &csi.DeleteSnapshotRequest{SnapshotId: "no-slash"}); err != nil {
		t.Fatalf("malformed snapshot id should be a no-op success, got %v", err)
	}
}

func TestCloneFromSnapshot(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-cs")
	if _, err := cs.CreateSnapshot(ctx, &csi.CreateSnapshotRequest{SourceVolumeId: "pvc-cs", Name: "snapshot-c"}); err != nil {
		t.Fatalf("create snapshot: %v", err)
	}
	resp, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{
		Name:               "pvc-fromsnap",
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 1 << 30},
		VolumeCapabilities: singleNodeCaps(),
		VolumeContentSource: &csi.VolumeContentSource{
			Type: &csi.VolumeContentSource_Snapshot{Snapshot: &csi.VolumeContentSource_SnapshotSource{SnapshotId: "pvc-cs/snapshot-c"}},
		},
	})
	if err != nil {
		t.Fatalf("clone from snapshot: %v", err)
	}
	if resp.GetVolume().GetVolumeId() != "pvc-fromsnap" {
		t.Errorf("clone id = %q", resp.GetVolume().GetVolumeId())
	}
	if resp.GetVolume().GetContentSource().GetSnapshot().GetSnapshotId() != "pvc-cs/snapshot-c" {
		t.Errorf("content source snapshot not echoed back")
	}
}
