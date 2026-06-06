// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/vsa"
	"github.com/metebalci/thur/csi/test/fake"
)

func testController(t *testing.T) *controllerServer {
	t.Helper()
	sock := filepath.Join(t.TempDir(), "admin.sock")
	d, err := fake.StartUnix(sock)
	if err != nil {
		t.Fatalf("start fake: %v", err)
	}
	t.Cleanup(func() { _ = d.Close() })
	return &controllerServer{driver: New(Config{Name: DefaultDriverName}), vsa: vsa.NewUnixClient(sock)}
}

func singleNodeCaps() []*csi.VolumeCapability {
	return []*csi.VolumeCapability{{
		AccessType: &csi.VolumeCapability_Mount{Mount: &csi.VolumeCapability_MountVolume{FsType: "ext4"}},
		AccessMode: &csi.VolumeCapability_AccessMode{Mode: csi.VolumeCapability_AccessMode_SINGLE_NODE_WRITER},
	}}
}

func TestCreateVolume(t *testing.T) {
	cs := testController(t)
	resp, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name:               "pvc-abc",
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 1 << 30},
		VolumeCapabilities: singleNodeCaps(),
		Parameters:         map[string]string{"backend": "primary", "fsType": "ext4"},
	})
	if err != nil {
		t.Fatalf("CreateVolume: %v", err)
	}
	v := resp.GetVolume()
	if v.GetVolumeId() != "pvc-abc" {
		t.Errorf("volume id = %q, want pvc-abc", v.GetVolumeId())
	}
	if v.GetCapacityBytes() != 1<<30 {
		t.Errorf("capacity = %d, want %d", v.GetCapacityBytes(), 1<<30)
	}
	if v.GetVolumeContext()["fsType"] != "ext4" {
		t.Errorf("fsType not propagated to volume context")
	}
}

func TestCreateVolumeRoundsUpToSector(t *testing.T) {
	cs := testController(t)
	resp, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name:               "pvc-odd",
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 4097},
		VolumeCapabilities: singleNodeCaps(),
	})
	if err != nil {
		t.Fatalf("CreateVolume: %v", err)
	}
	if got := resp.GetVolume().GetCapacityBytes(); got != 8192 {
		t.Errorf("capacity = %d, want 8192 (rounded to sector)", got)
	}
}

func TestCreateVolumeIdempotent(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	req := &csi.CreateVolumeRequest{Name: "pvc-x", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps()}
	r1, err := cs.CreateVolume(ctx, req)
	if err != nil {
		t.Fatalf("first create: %v", err)
	}
	r2, err := cs.CreateVolume(ctx, req)
	if err != nil {
		t.Fatalf("second create: %v", err)
	}
	if r1.GetVolume().GetVolumeId() != r2.GetVolume().GetVolumeId() {
		t.Errorf("idempotent create returned different ids: %q vs %q",
			r1.GetVolume().GetVolumeId(), r2.GetVolume().GetVolumeId())
	}
}

func TestCreateVolumeConflictingSize(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	if _, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{Name: "pvc-c", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps()}); err != nil {
		t.Fatalf("create: %v", err)
	}
	_, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{Name: "pvc-c", CapacityRange: &csi.CapacityRange{RequiredBytes: 2 << 30}, VolumeCapabilities: singleNodeCaps()})
	if status.Code(err) != codes.AlreadyExists {
		t.Fatalf("expected AlreadyExists for size mismatch, got %v", err)
	}
}

func TestCreateVolumeRejectsMultiNode(t *testing.T) {
	cs := testController(t)
	caps := []*csi.VolumeCapability{{
		AccessType: &csi.VolumeCapability_Mount{Mount: &csi.VolumeCapability_MountVolume{}},
		AccessMode: &csi.VolumeCapability_AccessMode{Mode: csi.VolumeCapability_AccessMode_MULTI_NODE_MULTI_WRITER},
	}}
	_, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name: "pvc-m", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: caps,
	})
	if status.Code(err) != codes.InvalidArgument {
		t.Fatalf("expected InvalidArgument for multi-node, got %v", err)
	}
}

func TestDeleteVolumeIdempotent(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	if _, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{Name: "pvc-d", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps()}); err != nil {
		t.Fatalf("create: %v", err)
	}
	if _, err := cs.DeleteVolume(ctx, &csi.DeleteVolumeRequest{VolumeId: "pvc-d"}); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if _, err := cs.DeleteVolume(ctx, &csi.DeleteVolumeRequest{VolumeId: "pvc-d"}); err != nil {
		t.Fatalf("second delete must be a no-op success, got %v", err)
	}
}

func TestCloneFromVolume(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	if _, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{Name: "pvc-src", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps()}); err != nil {
		t.Fatalf("create src: %v", err)
	}
	resp, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{
		Name:               "pvc-clone",
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 1 << 30},
		VolumeCapabilities: singleNodeCaps(),
		VolumeContentSource: &csi.VolumeContentSource{
			Type: &csi.VolumeContentSource_Volume{Volume: &csi.VolumeContentSource_VolumeSource{VolumeId: "pvc-src"}},
		},
	})
	if err != nil {
		t.Fatalf("clone: %v", err)
	}
	if resp.GetVolume().GetVolumeId() != "pvc-clone" {
		t.Errorf("clone id = %q", resp.GetVolume().GetVolumeId())
	}
	if resp.GetVolume().GetContentSource().GetVolume().GetVolumeId() != "pvc-src" {
		t.Errorf("content source not echoed back")
	}
}

func TestCloneMissingSource(t *testing.T) {
	cs := testController(t)
	_, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name:               "pvc-clone2",
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 1 << 30},
		VolumeCapabilities: singleNodeCaps(),
		VolumeContentSource: &csi.VolumeContentSource{
			Type: &csi.VolumeContentSource_Volume{Volume: &csi.VolumeContentSource_VolumeSource{VolumeId: "absent"}},
		},
	})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("expected NotFound for missing clone source, got %v", err)
	}
}

func TestValidateVolumeCapabilities(t *testing.T) {
	cs := testController(t)
	ctx := context.Background()
	if _, err := cs.CreateVolume(ctx, &csi.CreateVolumeRequest{Name: "pvc-v", CapacityRange: &csi.CapacityRange{RequiredBytes: 1 << 30}, VolumeCapabilities: singleNodeCaps()}); err != nil {
		t.Fatalf("create: %v", err)
	}
	resp, err := cs.ValidateVolumeCapabilities(ctx, &csi.ValidateVolumeCapabilitiesRequest{
		VolumeId: "pvc-v", VolumeCapabilities: singleNodeCaps(),
	})
	if err != nil {
		t.Fatalf("ValidateVolumeCapabilities: %v", err)
	}
	if resp.GetConfirmed() == nil {
		t.Errorf("expected confirmed capabilities")
	}
	if _, err := cs.ValidateVolumeCapabilities(ctx, &csi.ValidateVolumeCapabilitiesRequest{VolumeId: "absent", VolumeCapabilities: singleNodeCaps()}); status.Code(err) != codes.NotFound {
		t.Errorf("expected NotFound for absent volume, got %v", err)
	}
}

func TestControllerCapabilities(t *testing.T) {
	cs := testController(t)
	resp, err := cs.ControllerGetCapabilities(context.Background(), &csi.ControllerGetCapabilitiesRequest{})
	if err != nil {
		t.Fatalf("ControllerGetCapabilities: %v", err)
	}
	if len(resp.GetCapabilities()) == 0 {
		t.Fatal("no controller capabilities advertised")
	}
}

func TestVolumeNameSanitize(t *testing.T) {
	got, err := volumeName("pvc-12345678-90ab-cdef-1234-567890abcdef")
	if err != nil {
		t.Fatalf("volumeName: %v", err)
	}
	if got != "pvc-12345678-90ab-cdef-1234-567890abcdef" {
		t.Errorf("valid name was altered: %q", got)
	}
	bad, err := volumeName("has spaces/and:slashes")
	if err != nil {
		t.Fatalf("volumeName(bad): %v", err)
	}
	if !isValidVolumeName(bad) {
		t.Errorf("sanitized name still invalid: %q", bad)
	}
}
