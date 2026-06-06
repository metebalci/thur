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

	creds, err := s.chap.ensure(ctx, vol)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "ensure chap secret: %v", err)
	}
	// Register the per-volume CHAP user. The store is the source of truth for
	// the secret, so a re-publish (409) just re-grants the volume and returns
	// the same creds — the daemon already holds this password.
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
	if err := s.dropChapUser(ctx, req.GetVolumeId()); err != nil {
		return nil, err
	}
	if err := s.chap.remove(ctx, req.GetVolumeId()); err != nil {
		return nil, status.Errorf(codes.Internal, "delete chap secret: %v", err)
	}
	return &csi.ControllerUnpublishVolumeResponse{}, nil
}

// dropChapUser removes a volume's admission from its CHAP user, and removes the
// user entirely once it is admitted to nothing else. In the per-volume model
// the user serves exactly this volume, so revoke always refuses to empty the
// set (400/409) and remove is the terminal step; the revoke-first shape stays
// correct if a user ever carries extra volumes. Missing user is success.
func (s *controllerServer) dropChapUser(ctx context.Context, vol string) error {
	username := chapUsername(vol)
	_, err := s.vsa.RevokeUser(ctx, username, []string{vol})
	switch {
	case err == nil:
		return nil
	case vsa.IsNotFound(err):
		return nil
	case vsa.IsBadRequest(err), vsa.IsConflict(err):
		if err := s.vsa.RemoveUser(ctx, username); err != nil && !vsa.IsNotFound(err) {
			return toStatus(err)
		}
		return nil
	default:
		return toStatus(err)
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
