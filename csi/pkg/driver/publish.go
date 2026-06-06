// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"strconv"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/vsa"
)

// PublishContext keys. The node reads these in NodeStageVolume to drive the
// iscsiadm CHAP login and resolve the device by-path. They travel in the
// VolumeAttachment object that the external-attacher persists.
const (
	PublishCtxIQN        = "iqn"
	PublishCtxPortal     = "portal"
	PublishCtxLUN        = "lun"
	PublishCtxChapUser   = "chapUser"
	PublishCtxChapSecret = "chapSecret"
)

// volumeContextTargetPortal is the volume-context key carrying the StorageClass
// targetPortal param from CreateVolume through to ControllerPublishVolume.
const volumeContextTargetPortal = "targetPortal"

func (s *controllerServer) ControllerPublishVolume(ctx context.Context, req *csi.ControllerPublishVolumeRequest) (*csi.ControllerPublishVolumeResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	if req.GetNodeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "node_id is required")
	}
	vc := req.GetVolumeCapability()
	if vc == nil {
		return nil, status.Error(codes.InvalidArgument, "volume_capability is required")
	}
	if !isSupportedAccessMode(vc.GetAccessMode().GetMode()) {
		return nil, status.Error(codes.InvalidArgument, "unsupported access mode (single-node only)")
	}

	vol := req.GetVolumeId()
	v, err := s.vsa.GetVolumeByName(ctx, vol)
	if err != nil {
		return nil, toStatus(err)
	}
	if v == nil {
		return nil, status.Errorf(codes.NotFound, "volume %q not found", vol)
	}
	portal, err := s.resolvePortal(req.GetVolumeContext())
	if err != nil {
		return nil, err
	}

	creds, err := s.chap.ensure(ctx, req.GetNodeId())
	if err != nil {
		return nil, status.Errorf(codes.Internal, "ensure chap secret: %v", err)
	}
	// Admit this volume to the node's CHAP user. First publish to the node
	// creates the user (admitted to this volume); later publishes hit 409 and
	// grant the additional volume. The store is the source of truth for the
	// secret, so the daemon always holds this node's password.
	if _, err := s.vsa.AddUser(ctx, vsa.AddUserRequest{
		Username: creds.username,
		Password: creds.secret,
		Volumes:  []string{vol},
	}); err != nil {
		if !vsa.IsConflict(err) {
			return nil, toStatus(err)
		}
		if _, err := s.vsa.GrantUser(ctx, creds.username, []string{vol}); err != nil {
			return nil, toStatus(err)
		}
	}

	return &csi.ControllerPublishVolumeResponse{
		PublishContext: map[string]string{
			PublishCtxIQN:        s.driver.cfg.TargetIQN,
			PublishCtxPortal:     portal,
			PublishCtxLUN:        strconv.FormatUint(v.Lun, 10),
			PublishCtxChapUser:   creds.username,
			PublishCtxChapSecret: creds.secret,
		},
	}, nil
}

func (s *controllerServer) ControllerUnpublishVolume(ctx context.Context, req *csi.ControllerUnpublishVolumeRequest) (*csi.ControllerUnpublishVolumeResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	if req.GetNodeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "node_id is required")
	}
	lastVolume, err := s.dropChapVolume(ctx, req.GetNodeId(), req.GetVolumeId())
	if err != nil {
		return nil, err
	}
	// Only drop the node's secret once its last volume is unpublished — other
	// volumes on the node still need it.
	if lastVolume {
		if err := s.chap.remove(ctx, req.GetNodeId()); err != nil {
			return nil, status.Errorf(codes.Internal, "delete chap secret: %v", err)
		}
	}
	return &csi.ControllerUnpublishVolumeResponse{}, nil
}

// dropChapVolume removes a volume from the node's CHAP user admission set,
// removing the user entirely when that was its last volume. Returns whether the
// node user was removed (its last volume). The daemon refuses (400/409) to
// revoke the final volume, which is the signal to remove the user. A missing
// user is treated as already-removed.
func (s *controllerServer) dropChapVolume(ctx context.Context, nodeID, vol string) (bool, error) {
	username := chapUsername(nodeID)
	_, err := s.vsa.RevokeUser(ctx, username, []string{vol})
	switch {
	case err == nil:
		return false, nil // node retains other volumes
	case vsa.IsNotFound(err):
		return true, nil // user already gone
	case vsa.IsBadRequest(err), vsa.IsConflict(err):
		// Revoke refused to empty the set: this was the node's last volume.
		if err := s.vsa.RemoveUser(ctx, username); err != nil && !vsa.IsNotFound(err) {
			return false, toStatus(err)
		}
		return true, nil
	default:
		return false, toStatus(err)
	}
}

// resolvePortal picks the iSCSI portal: the --target-portal flag wins (operator
// override), else the StorageClass targetPortal param carried in the volume
// context. One must be set — the node needs a routable portal address.
func (s *controllerServer) resolvePortal(volumeContext map[string]string) (string, error) {
	if s.driver.cfg.TargetPortal != "" {
		return s.driver.cfg.TargetPortal, nil
	}
	if p := volumeContext[volumeContextTargetPortal]; p != "" {
		return p, nil
	}
	return "", status.Error(codes.InvalidArgument,
		"no iSCSI portal: set the StorageClass targetPortal parameter or the driver --target-portal flag")
}
