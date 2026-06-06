// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"time"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"

	"github.com/metebalci/thur/csi/pkg/vsa"
)

// CreateSnapshot freezes a point-in-time snapshot of a source volume. The CSI
// SnapshotId is "<volume>/<snapshot>"; snapshot names are scoped to their
// volume, so idempotency is a per-source lookup (a retry returns the existing
// snapshot, never ALREADY_EXISTS across sources).
func (s *controllerServer) CreateSnapshot(ctx context.Context, req *csi.CreateSnapshotRequest) (*csi.CreateSnapshotResponse, error) {
	if req.GetSourceVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "source_volume_id is required")
	}
	if req.GetName() == "" {
		return nil, status.Error(codes.InvalidArgument, "name is required")
	}
	snap, err := snapshotName(req.GetName())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "invalid snapshot name: %v", err)
	}
	src := req.GetSourceVolumeId()

	row, err := s.vsa.CreateSnapshot(ctx, src, snap)
	if err != nil {
		switch {
		case vsa.IsConflict(err):
			return s.existingSnapshot(ctx, src, snap)
		case vsa.IsNotFound(err):
			return nil, status.Errorf(codes.NotFound, "source volume %q not found", src)
		default:
			return nil, toStatus(err)
		}
	}
	return snapshotResponse(src, snap, row), nil
}

func (s *controllerServer) DeleteSnapshot(ctx context.Context, req *csi.DeleteSnapshotRequest) (*csi.DeleteSnapshotResponse, error) {
	if req.GetSnapshotId() == "" {
		return nil, status.Error(codes.InvalidArgument, "snapshot_id is required")
	}
	vol, snap, ok := splitSnapshotID(req.GetSnapshotId())
	if !ok {
		// An unparseable id can't reference a real snapshot; delete is a no-op.
		return &csi.DeleteSnapshotResponse{}, nil
	}
	if err := s.vsa.DeleteSnapshot(ctx, vol, snap); err != nil && !vsa.IsNotFound(err) {
		return nil, toStatus(err)
	}
	return &csi.DeleteSnapshotResponse{}, nil
}

// existingSnapshot returns the already-present snapshot for an idempotent
// CreateSnapshot retry (the create reported 409).
func (s *controllerServer) existingSnapshot(ctx context.Context, src, snap string) (*csi.CreateSnapshotResponse, error) {
	snaps, err := s.vsa.ListSnapshots(ctx, src)
	if err != nil {
		return nil, toStatus(err)
	}
	for i := range snaps {
		if snaps[i].Snapshot == snap {
			return snapshotResponse(src, snap, &snaps[i]), nil
		}
	}
	return nil, status.Errorf(codes.Internal, "snapshot %q reported as existing but not found on %q", snap, src)
}

func snapshotResponse(src, snap string, row *vsa.SnapshotRow) *csi.CreateSnapshotResponse {
	return &csi.CreateSnapshotResponse{
		Snapshot: &csi.Snapshot{
			SizeBytes:      int64(row.SizeBytes),
			SnapshotId:     src + "/" + snap,
			SourceVolumeId: src,
			CreationTime:   parseSnapshotTime(row.CreatedAt),
			ReadyToUse:     true,
		},
	}
}

// parseSnapshotTime converts the daemon's RFC3339 created_at to a protobuf
// timestamp, or nil if it can't be parsed (the field is optional).
func parseSnapshotTime(s string) *timestamppb.Timestamp {
	t, err := time.Parse(time.RFC3339, s)
	if err != nil {
		return nil
	}
	return timestamppb.New(t)
}
