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
// SnapshotId is "<volume>/<snapshot>". Per the CSI contract the snapshot Name
// is the idempotency key and is treated as globally unique: the same name on
// the same source returns the existing snapshot, while the same name on a
// *different* source is ALREADY_EXISTS. The daemon scopes snapshot names per
// volume, so the driver enforces global uniqueness with a cross-volume lookup
// (findSnapshot). In practice the external-snapshotter mints unique names, so
// the scan normally finds nothing; it is the correctness guard for the
// duplicate-name case.
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

	where, row, err := s.findSnapshot(ctx, snap)
	if err != nil {
		return nil, err
	}
	if row != nil {
		if where == src {
			return snapshotResponse(src, snap, row), nil // idempotent
		}
		return nil, status.Errorf(codes.AlreadyExists,
			"snapshot name %q already exists on a different source volume %q", snap, where)
	}

	row, err = s.vsa.CreateSnapshot(ctx, src, snap)
	if err != nil {
		switch {
		case vsa.IsNotFound(err):
			return nil, status.Errorf(codes.NotFound, "source volume %q not found", src)
		case vsa.IsConflict(err):
			// Raced with a concurrent create on this source: fetch and return it.
			if _, row2, ferr := s.findSnapshot(ctx, snap); ferr != nil {
				return nil, ferr
			} else if row2 != nil {
				return snapshotResponse(src, snap, row2), nil
			}
			return nil, status.Errorf(codes.Internal, "snapshot %q reported as existing but not found", snap)
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

// findSnapshot locates a snapshot by name across all volumes, returning the
// volume it lives on and its row (or "", nil if absent). Used to make the CSI
// snapshot Name globally unique against a daemon that scopes names per volume.
// Errors are already gRPC-status-wrapped.
func (s *controllerServer) findSnapshot(ctx context.Context, snap string) (string, *vsa.SnapshotRow, error) {
	vols, err := s.vsa.ListVolumes(ctx)
	if err != nil {
		return "", nil, toStatus(err)
	}
	for i := range vols {
		snaps, err := s.vsa.ListSnapshots(ctx, vols[i].Name)
		if err != nil {
			if vsa.IsNotFound(err) {
				continue // volume removed mid-scan
			}
			return "", nil, toStatus(err)
		}
		for j := range snaps {
			if snaps[j].Snapshot == snap {
				return vols[i].Name, &snaps[j], nil
			}
		}
	}
	return "", nil, nil
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
