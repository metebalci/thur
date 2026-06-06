// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"fmt"
	"strings"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/vsa"
)

// sectorBytes is the VSA volume sector size; requested capacities round up to it.
const sectorBytes = 4096

// controllerServer implements csi.ControllerServer against the VSA admin API.
type controllerServer struct {
	csi.UnimplementedControllerServer
	driver *Driver
	vsa    *vsa.Client
	chap   chapStore
}

func (s *controllerServer) ControllerGetCapabilities(_ context.Context, _ *csi.ControllerGetCapabilitiesRequest) (*csi.ControllerGetCapabilitiesResponse, error) {
	return &csi.ControllerGetCapabilitiesResponse{Capabilities: controllerCapabilities()}, nil
}

func (s *controllerServer) ValidateVolumeCapabilities(ctx context.Context, req *csi.ValidateVolumeCapabilitiesRequest) (*csi.ValidateVolumeCapabilitiesResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	if len(req.GetVolumeCapabilities()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "volume_capabilities is required")
	}
	v, err := s.vsa.GetVolumeByName(ctx, req.GetVolumeId())
	if err != nil {
		return nil, toStatus(err)
	}
	if v == nil {
		return nil, status.Errorf(codes.NotFound, "volume %q not found", req.GetVolumeId())
	}
	if !capsSupported(req.GetVolumeCapabilities()) {
		return &csi.ValidateVolumeCapabilitiesResponse{Message: "unsupported access mode (single-node only)"}, nil
	}
	return &csi.ValidateVolumeCapabilitiesResponse{
		Confirmed: &csi.ValidateVolumeCapabilitiesResponse_Confirmed{
			VolumeCapabilities: req.GetVolumeCapabilities(),
		},
	}, nil
}

func (s *controllerServer) CreateVolume(ctx context.Context, req *csi.CreateVolumeRequest) (*csi.CreateVolumeResponse, error) {
	if req.GetName() == "" {
		return nil, status.Error(codes.InvalidArgument, "name is required")
	}
	if !capsSupported(req.GetVolumeCapabilities()) {
		return nil, status.Error(codes.InvalidArgument, "volume_capabilities is required and must be single-node")
	}
	name, err := volumeName(req.GetName())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "invalid name: %v", err)
	}
	sizeBytes, err := requiredBytes(req.GetCapacityRange())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}
	params, err := parseParams(req.GetParameters())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}

	if src := req.GetVolumeContentSource(); src != nil {
		return s.createFromSource(ctx, name, sizeBytes, src, params)
	}

	row, err := s.vsa.CreateVolume(ctx, vsa.CreateVolumeRequest{
		Name:          name,
		SizeBytes:     sizeBytes,
		PageSizeBytes: params.pageSize,
		Backend:       params.backend,
		Dedup:         params.dedupScope,
		Worm:          params.worm,
		Encrypt:       params.encrypt,
		Keystore:      params.keystore,
		DekSource:     params.dekSource,
		SyncAfter:     params.syncAfter,
	})
	if err != nil {
		if vsa.IsConflict(err) {
			return s.reconcileExisting(ctx, name, sizeBytes, params, nil)
		}
		return nil, toStatus(err)
	}
	return createResponse(row, params, nil), nil
}

// createFromSource handles CreateVolume with a content source: clone from a
// snapshot (snapshot_id = "<volume>/<snapshot>") or from a source volume.
func (s *controllerServer) createFromSource(ctx context.Context, name string, requested uint64, src *csi.VolumeContentSource, params storageClassParams) (*csi.CreateVolumeResponse, error) {
	var srcVol string
	clone := vsa.CloneVolumeRequest{NewName: name}
	switch cs := src.GetType().(type) {
	case *csi.VolumeContentSource_Snapshot:
		vol, snap, ok := splitSnapshotID(cs.Snapshot.GetSnapshotId())
		if !ok {
			// A snapshot id that doesn't parse can't name an existing snapshot:
			// NOT_FOUND, per the CSI CreateVolume contract.
			return nil, status.Errorf(codes.NotFound, "snapshot %q not found", cs.Snapshot.GetSnapshotId())
		}
		srcVol, clone.FromSnapshot = vol, snap
	case *csi.VolumeContentSource_Volume:
		srcVol = cs.Volume.GetVolumeId()
	default:
		return nil, status.Error(codes.InvalidArgument, "unsupported volume content source")
	}
	if srcVol == "" {
		return nil, status.Error(codes.InvalidArgument, "content source is missing an id")
	}

	row, err := s.vsa.CloneVolume(ctx, srcVol, clone)
	if err != nil {
		switch {
		case vsa.IsConflict(err):
			return s.reconcileExisting(ctx, name, requested, params, src)
		case vsa.IsNotFound(err):
			return nil, status.Errorf(codes.NotFound, "clone source not found: %v", err)
		default:
			return nil, toStatus(err)
		}
	}
	return createResponse(row, params, src), nil
}

// reconcileExisting implements CreateVolume idempotency: a name that already
// exists with the requested size is returned as success; a size mismatch is
// ALREADY_EXISTS.
func (s *controllerServer) reconcileExisting(ctx context.Context, name string, requested uint64, params storageClassParams, src *csi.VolumeContentSource) (*csi.CreateVolumeResponse, error) {
	existing, err := s.vsa.GetVolumeByName(ctx, name)
	if err != nil {
		return nil, toStatus(err)
	}
	if existing == nil {
		return nil, status.Errorf(codes.Internal, "volume %q reported as existing but not found", name)
	}
	if existing.SizeBytes != requested {
		return nil, status.Errorf(codes.AlreadyExists,
			"volume %q already exists with size %d, requested %d", name, existing.SizeBytes, requested)
	}
	return createResponse(existing, params, src), nil
}

func (s *controllerServer) DeleteVolume(ctx context.Context, req *csi.DeleteVolumeRequest) (*csi.DeleteVolumeResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	// Reap any CHAP user + secret a missed unpublish left behind, so a deleted
	// volume never leaks a credential.
	if err := s.vsa.RemoveUser(ctx, chapUsername(req.GetVolumeId())); err != nil && !vsa.IsNotFound(err) {
		return nil, toStatus(err)
	}
	if err := s.chap.remove(ctx, req.GetVolumeId()); err != nil {
		return nil, status.Errorf(codes.Internal, "delete chap secret: %v", err)
	}
	if err := s.vsa.DeleteVolume(ctx, req.GetVolumeId()); err != nil && !vsa.IsNotFound(err) {
		return nil, toStatus(err)
	}
	return &csi.DeleteVolumeResponse{}, nil
}

// ---- helpers ----

func createResponse(row *vsa.VolumeRow, params storageClassParams, src *csi.VolumeContentSource) *csi.CreateVolumeResponse {
	vctx := map[string]string{"volumeName": row.Name}
	if params.fsType != "" {
		vctx["fsType"] = params.fsType
	}
	// Carry the StorageClass portal through to ControllerPublishVolume, which
	// has no other view of the StorageClass.
	if params.targetPortal != "" {
		vctx[volumeContextTargetPortal] = params.targetPortal
	}
	return &csi.CreateVolumeResponse{
		Volume: &csi.Volume{
			VolumeId:      row.Name,
			CapacityBytes: int64(row.SizeBytes),
			VolumeContext: vctx,
			ContentSource: src,
		},
	}
}

// requiredBytes resolves the volume size from a CSI capacity range, rounded up
// to the sector size.
func requiredBytes(cr *csi.CapacityRange) (uint64, error) {
	if cr == nil {
		return 0, fmt.Errorf("capacity_range is required")
	}
	req := cr.GetRequiredBytes()
	if req <= 0 {
		req = cr.GetLimitBytes()
	}
	if req <= 0 {
		return 0, fmt.Errorf("capacity_range must set required_bytes or limit_bytes")
	}
	size := roundUp(uint64(req), sectorBytes)
	if lim := cr.GetLimitBytes(); lim > 0 && size > uint64(lim) {
		return 0, fmt.Errorf("sector-rounded size %d exceeds limit_bytes %d", size, lim)
	}
	return size, nil
}

func roundUp(n, mult uint64) uint64 {
	if mult == 0 || n%mult == 0 {
		return n
	}
	return (n/mult + 1) * mult
}

// splitSnapshotID parses a "<volume>/<snapshot>" CSI snapshot id.
func splitSnapshotID(id string) (vol, snap string, ok bool) {
	i := strings.IndexByte(id, '/')
	if i <= 0 || i >= len(id)-1 {
		return "", "", false
	}
	return id[:i], id[i+1:], true
}

// toStatus maps a vsa.APIError to the closest gRPC status.
func toStatus(err error) error {
	switch {
	case vsa.IsNotFound(err):
		return status.Error(codes.NotFound, err.Error())
	case vsa.IsConflict(err):
		return status.Error(codes.FailedPrecondition, err.Error())
	case vsa.IsBadRequest(err):
		return status.Error(codes.InvalidArgument, err.Error())
	default:
		return status.Error(codes.Internal, err.Error())
	}
}
