// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// ControllerExpandVolume grows a volume. VSA volumes are thin and grow-only:
// a request at or below the current size is already satisfied (idempotent),
// and the driver never issues a shrink. node_expansion_required is always set
// — the node must rescan the iSCSI session to observe the larger LUN (and grow
// the filesystem for mount volumes).
func (s *controllerServer) ControllerExpandVolume(ctx context.Context, req *csi.ControllerExpandVolumeRequest) (*csi.ControllerExpandVolumeResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	requested, err := requiredBytes(req.GetCapacityRange())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}
	vol := req.GetVolumeId()
	v, err := s.vsa.GetVolumeByName(ctx, vol)
	if err != nil {
		return nil, toStatus(err)
	}
	if v == nil {
		return nil, status.Errorf(codes.NotFound, "volume %q not found", vol)
	}
	if requested <= v.SizeBytes {
		return &csi.ControllerExpandVolumeResponse{
			CapacityBytes:         int64(v.SizeBytes),
			NodeExpansionRequired: true,
		}, nil
	}
	rs, err := s.vsa.ResizeVolume(ctx, vol, requested)
	if err != nil {
		return nil, toStatus(err)
	}
	return &csi.ControllerExpandVolumeResponse{
		CapacityBytes:         int64(rs.SizeBytes),
		NodeExpansionRequired: true,
	}, nil
}
