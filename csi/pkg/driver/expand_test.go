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

func TestControllerExpandGrows(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-grow") // 1 GiB

	resp, err := cs.ControllerExpandVolume(ctx, &csi.ControllerExpandVolumeRequest{
		VolumeId:      "pvc-grow",
		CapacityRange: &csi.CapacityRange{RequiredBytes: 2 << 30},
	})
	if err != nil {
		t.Fatalf("expand: %v", err)
	}
	if resp.GetCapacityBytes() != 2<<30 {
		t.Errorf("capacity = %d, want %d", resp.GetCapacityBytes(), 2<<30)
	}
	if !resp.GetNodeExpansionRequired() {
		t.Errorf("node_expansion_required must be true")
	}
	v, _ := cs.vsa.GetVolumeByName(ctx, "pvc-grow")
	if v.SizeBytes != 2<<30 {
		t.Errorf("daemon size = %d, want %d", v.SizeBytes, 2<<30)
	}
}

func TestControllerExpandNoOpWhenNotLarger(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-same") // 1 GiB

	resp, err := cs.ControllerExpandVolume(ctx, &csi.ControllerExpandVolumeRequest{
		VolumeId:      "pvc-same",
		CapacityRange: &csi.CapacityRange{RequiredBytes: 512 << 20}, // smaller than current
	})
	if err != nil {
		t.Fatalf("expand: %v", err)
	}
	if resp.GetCapacityBytes() != 1<<30 {
		t.Errorf("capacity = %d, want current 1 GiB (no shrink)", resp.GetCapacityBytes())
	}
}

func TestControllerExpandRoundsToSector(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	createSrcVolume(t, cs, "pvc-round") // 1 GiB

	resp, err := cs.ControllerExpandVolume(ctx, &csi.ControllerExpandVolumeRequest{
		VolumeId:      "pvc-round",
		CapacityRange: &csi.CapacityRange{RequiredBytes: (2 << 30) + 1},
	})
	if err != nil {
		t.Fatalf("expand: %v", err)
	}
	if resp.GetCapacityBytes() != (2<<30)+sectorBytes {
		t.Errorf("capacity = %d, want sector-rounded %d", resp.GetCapacityBytes(), (2<<30)+sectorBytes)
	}
}

func TestControllerExpandMissingVolume(t *testing.T) {
	cs := testController(t)
	_, err := cs.ControllerExpandVolume(context.Background(), &csi.ControllerExpandVolumeRequest{
		VolumeId: "absent", CapacityRange: &csi.CapacityRange{RequiredBytes: 2 << 30},
	})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("expected NotFound for missing volume, got %v", err)
	}
}
