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

// publishCtx provisions a volume and returns a publish request whose volume
// context carries a portal (the StorageClass-param path).
func provision(t *testing.T, cs *controllerServer, name string) {
	t.Helper()
	if _, err := cs.CreateVolume(context.Background(), &csi.CreateVolumeRequest{
		Name:               name,
		CapacityRange:      &csi.CapacityRange{RequiredBytes: 1 << 30},
		VolumeCapabilities: singleNodeCaps(),
		Parameters:         map[string]string{"targetPortal": "10.0.0.5:3260"},
	}); err != nil {
		t.Fatalf("provision %q: %v", name, err)
	}
}

func publishReq(name string) *csi.ControllerPublishVolumeRequest {
	return &csi.ControllerPublishVolumeRequest{
		VolumeId:         name,
		NodeId:           "node-1",
		VolumeCapability: singleNodeCaps()[0],
		VolumeContext:    map[string]string{volumeContextTargetPortal: "10.0.0.5:3260"},
	}
}

func TestControllerPublishVolume(t *testing.T) {
	cs, d := testControllerD(t)
	ctx := context.Background()
	provision(t, cs, "pvc-pub")

	resp, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-pub"))
	if err != nil {
		t.Fatalf("publish: %v", err)
	}
	pc := resp.GetPublishContext()
	if pc[PublishCtxIQN] != DefaultTargetIQN {
		t.Errorf("iqn = %q, want %q", pc[PublishCtxIQN], DefaultTargetIQN)
	}
	if pc[PublishCtxPortal] != "10.0.0.5:3260" {
		t.Errorf("portal = %q", pc[PublishCtxPortal])
	}
	if pc[PublishCtxLUN] == "" {
		t.Errorf("lun missing")
	}
	if pc[PublishCtxChapUser] != chapUsername("pvc-pub") {
		t.Errorf("chapUser = %q", pc[PublishCtxChapUser])
	}
	if len(pc[PublishCtxChapSecret]) != 64 {
		t.Errorf("chapSecret = %q (want 64 hex chars)", pc[PublishCtxChapSecret])
	}
	if d.UserCount() != 1 {
		t.Errorf("user count = %d, want 1", d.UserCount())
	}
}

func TestControllerPublishIdempotent(t *testing.T) {
	cs, d := testControllerD(t)
	ctx := context.Background()
	provision(t, cs, "pvc-idem")

	r1, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-idem"))
	if err != nil {
		t.Fatalf("first publish: %v", err)
	}
	r2, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-idem"))
	if err != nil {
		t.Fatalf("second publish: %v", err)
	}
	if r1.GetPublishContext()[PublishCtxChapSecret] != r2.GetPublishContext()[PublishCtxChapSecret] {
		t.Errorf("retried publish returned a different chap secret")
	}
	if d.UserCount() != 1 {
		t.Errorf("user count = %d, want 1 after re-publish", d.UserCount())
	}
}

func TestControllerPublishMissingVolume(t *testing.T) {
	cs, _ := testControllerD(t)
	_, err := cs.ControllerPublishVolume(context.Background(), publishReq("absent"))
	if status.Code(err) != codes.NotFound {
		t.Fatalf("expected NotFound for missing volume, got %v", err)
	}
}

func TestControllerPublishNoPortal(t *testing.T) {
	cs, _ := testControllerD(t)
	provision(t, cs, "pvc-np")
	req := publishReq("pvc-np")
	req.VolumeContext = nil // no flag, no param
	_, err := cs.ControllerPublishVolume(context.Background(), req)
	if status.Code(err) != codes.InvalidArgument {
		t.Fatalf("expected InvalidArgument with no portal, got %v", err)
	}
}

func TestControllerPublishPortalFlagOverride(t *testing.T) {
	cs, _ := testControllerD(t)
	cs.driver.cfg.TargetPortal = "192.168.1.10:3260"
	provision(t, cs, "pvc-flag")
	resp, err := cs.ControllerPublishVolume(context.Background(), publishReq("pvc-flag"))
	if err != nil {
		t.Fatalf("publish: %v", err)
	}
	if got := resp.GetPublishContext()[PublishCtxPortal]; got != "192.168.1.10:3260" {
		t.Errorf("portal = %q, want flag override", got)
	}
}

func TestControllerPublishRejectsMultiNode(t *testing.T) {
	cs, _ := testControllerD(t)
	provision(t, cs, "pvc-mn")
	req := publishReq("pvc-mn")
	req.VolumeCapability = &csi.VolumeCapability{
		AccessType: &csi.VolumeCapability_Mount{Mount: &csi.VolumeCapability_MountVolume{}},
		AccessMode: &csi.VolumeCapability_AccessMode{Mode: csi.VolumeCapability_AccessMode_MULTI_NODE_MULTI_WRITER},
	}
	if _, err := cs.ControllerPublishVolume(context.Background(), req); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("expected InvalidArgument for multi-node, got %v", err)
	}
}

func TestControllerUnpublishVolume(t *testing.T) {
	cs, d := testControllerD(t)
	ctx := context.Background()
	provision(t, cs, "pvc-unp")
	if _, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-unp")); err != nil {
		t.Fatalf("publish: %v", err)
	}
	if d.UserCount() != 1 {
		t.Fatalf("user count = %d, want 1 before unpublish", d.UserCount())
	}
	if _, err := cs.ControllerUnpublishVolume(ctx, &csi.ControllerUnpublishVolumeRequest{VolumeId: "pvc-unp", NodeId: "node-1"}); err != nil {
		t.Fatalf("unpublish: %v", err)
	}
	if d.UserCount() != 0 {
		t.Errorf("user count = %d, want 0 after unpublish (revoke->remove fallthrough)", d.UserCount())
	}
}

func TestControllerUnpublishIdempotent(t *testing.T) {
	cs, _ := testControllerD(t)
	ctx := context.Background()
	provision(t, cs, "pvc-unp2")
	if _, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-unp2")); err != nil {
		t.Fatalf("publish: %v", err)
	}
	for i := 0; i < 2; i++ {
		if _, err := cs.ControllerUnpublishVolume(ctx, &csi.ControllerUnpublishVolumeRequest{VolumeId: "pvc-unp2"}); err != nil {
			t.Fatalf("unpublish #%d must be idempotent, got %v", i, err)
		}
	}
}

func TestDeleteVolumeReapsChapUser(t *testing.T) {
	cs, d := testControllerD(t)
	ctx := context.Background()
	provision(t, cs, "pvc-del")
	if _, err := cs.ControllerPublishVolume(ctx, publishReq("pvc-del")); err != nil {
		t.Fatalf("publish: %v", err)
	}
	// Delete without an intervening unpublish: the volume's CHAP user must not leak.
	if _, err := cs.DeleteVolume(ctx, &csi.DeleteVolumeRequest{VolumeId: "pvc-del"}); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if d.UserCount() != 0 {
		t.Errorf("user count = %d, want 0 after delete", d.UserCount())
	}
	if d.VolumeCount() != 0 {
		t.Errorf("volume count = %d, want 0 after delete", d.VolumeCount())
	}
}
