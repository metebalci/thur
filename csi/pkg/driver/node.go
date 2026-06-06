// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strconv"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/iscsi"
)

// attacher is the node-side iSCSI surface (pkg/iscsi.Attacher in production,
// faked in tests).
type attacher interface {
	Attach(ctx context.Context, c iscsi.Connector) (string, error)
	Rescan(ctx context.Context, c iscsi.Connector) (string, error)
	Detach(ctx context.Context, iqn, portal string) error
}

// volumeMounter is the subset of mount-utils the node server uses
// (*mount.SafeFormatAndMount in production, faked in tests).
type volumeMounter interface {
	FormatAndMount(source, target, fstype string, options []string) error
	Mount(source, target, fstype string, options []string) error
	Unmount(target string) error
	IsLikelyNotMountPoint(file string) (bool, error)
}

// resizeFs grows a filesystem in place (*mount.ResizeFs in production).
type resizeFs interface {
	Resize(devicePath, deviceMountPath string) (bool, error)
}

// nodeServer implements csi.NodeServer: it attaches the LUN over iSCSI with the
// per-volume CHAP creds from PublishContext, then formats + mounts it. It never
// talks to the admin socket.
type nodeServer struct {
	csi.UnimplementedNodeServer
	driver   *Driver
	attacher attacher
	mounter  volumeMounter
	resizer  resizeFs
	stateDir string
}

func (s *nodeServer) NodeGetInfo(_ context.Context, _ *csi.NodeGetInfoRequest) (*csi.NodeGetInfoResponse, error) {
	if s.driver.cfg.NodeID == "" {
		return nil, status.Error(codes.Internal, "node id not configured (set --node-id)")
	}
	return &csi.NodeGetInfoResponse{NodeId: s.driver.cfg.NodeID}, nil
}

func (s *nodeServer) NodeGetCapabilities(_ context.Context, _ *csi.NodeGetCapabilitiesRequest) (*csi.NodeGetCapabilitiesResponse, error) {
	return &csi.NodeGetCapabilitiesResponse{Capabilities: nodeCapabilities()}, nil
}

func (s *nodeServer) NodeStageVolume(ctx context.Context, req *csi.NodeStageVolumeRequest) (*csi.NodeStageVolumeResponse, error) {
	staging := req.GetStagingTargetPath()
	vc := req.GetVolumeCapability()
	if req.GetVolumeId() == "" || staging == "" || vc == nil {
		return nil, status.Error(codes.InvalidArgument, "volume_id, staging_target_path and volume_capability are required")
	}
	conn, err := connectorFromPublishContext(req.GetPublishContext())
	if err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}

	device, err := s.attacher.Attach(ctx, conn)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "iscsi attach: %v", err)
	}
	// Persist the connector so NodeUnstageVolume (no PublishContext) can detach.
	if err := s.saveConn(req.GetVolumeId(), conn); err != nil {
		return nil, status.Errorf(codes.Internal, "persist connector: %v", err)
	}

	// Raw block volumes are not mounted at stage; the device is bind-mounted in
	// NodePublishVolume.
	if vc.GetBlock() != nil {
		return &csi.NodeStageVolumeResponse{}, nil
	}

	m := vc.GetMount()
	fsType := m.GetFsType()
	if fsType == "" {
		fsType = "ext4"
	}
	if err := os.MkdirAll(staging, 0o750); err != nil {
		return nil, status.Errorf(codes.Internal, "mkdir staging: %v", err)
	}
	notMnt, err := s.mounter.IsLikelyNotMountPoint(staging)
	if err != nil && !os.IsNotExist(err) {
		return nil, status.Errorf(codes.Internal, "check staging mount: %v", err)
	}
	if !notMnt {
		return &csi.NodeStageVolumeResponse{}, nil // already staged
	}
	if err := s.mounter.FormatAndMount(device, staging, fsType, m.GetMountFlags()); err != nil {
		return nil, status.Errorf(codes.Internal, "format+mount %s: %v", device, err)
	}
	return &csi.NodeStageVolumeResponse{}, nil
}

func (s *nodeServer) NodePublishVolume(ctx context.Context, req *csi.NodePublishVolumeRequest) (*csi.NodePublishVolumeResponse, error) {
	target := req.GetTargetPath()
	vc := req.GetVolumeCapability()
	if req.GetVolumeId() == "" || target == "" || vc == nil {
		return nil, status.Error(codes.InvalidArgument, "volume_id, target_path and volume_capability are required")
	}
	opts := []string{"bind"}
	if req.GetReadonly() {
		opts = append(opts, "ro")
	}

	if vc.GetBlock() != nil {
		conn, err := connectorFromPublishContext(req.GetPublishContext())
		if err != nil {
			return nil, status.Errorf(codes.InvalidArgument, "%v", err)
		}
		device, err := s.attacher.Attach(ctx, conn) // idempotent; the session already exists
		if err != nil {
			return nil, status.Errorf(codes.Internal, "iscsi attach: %v", err)
		}
		if err := os.MkdirAll(filepath.Dir(target), 0o750); err != nil {
			return nil, status.Errorf(codes.Internal, "mkdir target parent: %v", err)
		}
		f, err := os.OpenFile(target, os.O_CREATE, 0o600)
		if err != nil {
			return nil, status.Errorf(codes.Internal, "create target file: %v", err)
		}
		_ = f.Close()
		if err := s.bindMount(target, func() error { return s.mounter.Mount(device, target, "", opts) }); err != nil {
			return nil, err
		}
		return &csi.NodePublishVolumeResponse{}, nil
	}

	staging := req.GetStagingTargetPath()
	if staging == "" {
		return nil, status.Error(codes.InvalidArgument, "staging_target_path is required for mount volumes")
	}
	if err := os.MkdirAll(target, 0o750); err != nil {
		return nil, status.Errorf(codes.Internal, "mkdir target: %v", err)
	}
	fsType := vc.GetMount().GetFsType()
	if err := s.bindMount(target, func() error { return s.mounter.Mount(staging, target, fsType, opts) }); err != nil {
		return nil, err
	}
	return &csi.NodePublishVolumeResponse{}, nil
}

func (s *nodeServer) NodeUnpublishVolume(_ context.Context, req *csi.NodeUnpublishVolumeRequest) (*csi.NodeUnpublishVolumeResponse, error) {
	target := req.GetTargetPath()
	if req.GetVolumeId() == "" || target == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id and target_path are required")
	}
	if err := s.unmount(target); err != nil {
		return nil, status.Errorf(codes.Internal, "unmount target: %v", err)
	}
	return &csi.NodeUnpublishVolumeResponse{}, nil
}

func (s *nodeServer) NodeUnstageVolume(ctx context.Context, req *csi.NodeUnstageVolumeRequest) (*csi.NodeUnstageVolumeResponse, error) {
	staging := req.GetStagingTargetPath()
	if req.GetVolumeId() == "" || staging == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id and staging_target_path are required")
	}
	if err := s.unmount(staging); err != nil {
		return nil, status.Errorf(codes.Internal, "unmount staging: %v", err)
	}
	conn, err := s.loadConn(req.GetVolumeId())
	if err != nil {
		if os.IsNotExist(err) {
			return &csi.NodeUnstageVolumeResponse{}, nil // never staged / already cleaned
		}
		return nil, status.Errorf(codes.Internal, "load connector: %v", err)
	}
	if err := s.attacher.Detach(ctx, conn.TargetIQN, conn.Portal); err != nil {
		return nil, status.Errorf(codes.Internal, "iscsi detach: %v", err)
	}
	_ = os.Remove(s.connPath(req.GetVolumeId()))
	return &csi.NodeUnstageVolumeResponse{}, nil
}

func (s *nodeServer) NodeExpandVolume(ctx context.Context, req *csi.NodeExpandVolumeRequest) (*csi.NodeExpandVolumeResponse, error) {
	if req.GetVolumeId() == "" || req.GetVolumePath() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id and volume_path are required")
	}
	conn, err := s.loadConn(req.GetVolumeId())
	if err != nil {
		if os.IsNotExist(err) {
			return nil, status.Errorf(codes.FailedPrecondition, "no iSCSI connector state for %q; volume must be staged on this node", req.GetVolumeId())
		}
		return nil, status.Errorf(codes.Internal, "load connector: %v", err)
	}
	// Rescan so the kernel observes the grown LUN.
	device, err := s.attacher.Rescan(ctx, conn)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "iscsi rescan: %v", err)
	}
	// Raw block volumes have no filesystem to grow; the rescan is enough.
	if req.GetVolumeCapability().GetBlock() != nil {
		return &csi.NodeExpandVolumeResponse{}, nil
	}
	if _, err := s.resizer.Resize(device, req.GetVolumePath()); err != nil {
		return nil, status.Errorf(codes.Internal, "resize filesystem on %s: %v", device, err)
	}
	return &csi.NodeExpandVolumeResponse{CapacityBytes: req.GetCapacityRange().GetRequiredBytes()}, nil
}

// ---- helpers ----

// bindMount runs mount only if target is not already a mount point, so a
// retried publish is a no-op.
func (s *nodeServer) bindMount(target string, mount func() error) error {
	notMnt, err := s.mounter.IsLikelyNotMountPoint(target)
	if err != nil && !os.IsNotExist(err) {
		return status.Errorf(codes.Internal, "check target mount: %v", err)
	}
	if !notMnt {
		return nil
	}
	if err := mount(); err != nil {
		return status.Errorf(codes.Internal, "bind mount: %v", err)
	}
	return nil
}

// unmount unmounts target if it is a mount point and removes the mountpoint,
// tolerating an absent path so unpublish/unstage are idempotent.
func (s *nodeServer) unmount(target string) error {
	notMnt, err := s.mounter.IsLikelyNotMountPoint(target)
	if err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	if !notMnt {
		if err := s.mounter.Unmount(target); err != nil {
			return err
		}
	}
	_ = os.Remove(target)
	return nil
}

func connectorFromPublishContext(pc map[string]string) (iscsi.Connector, error) {
	if pc == nil {
		return iscsi.Connector{}, fmt.Errorf("missing publish context")
	}
	lun, err := strconv.ParseUint(pc[PublishCtxLUN], 10, 32)
	if err != nil {
		return iscsi.Connector{}, fmt.Errorf("bad lun %q in publish context: %w", pc[PublishCtxLUN], err)
	}
	c := iscsi.Connector{
		TargetIQN:  pc[PublishCtxIQN],
		Portal:     pc[PublishCtxPortal],
		Lun:        uint32(lun),
		ChapUser:   pc[PublishCtxChapUser],
		ChapSecret: pc[PublishCtxChapSecret],
	}
	if c.TargetIQN == "" || c.Portal == "" {
		return c, fmt.Errorf("publish context missing iqn or portal")
	}
	return c, nil
}

func (s *nodeServer) connPath(volID string) string {
	return filepath.Join(s.stateDir, volID+".json")
}

func (s *nodeServer) saveConn(volID string, c iscsi.Connector) error {
	if err := os.MkdirAll(s.stateDir, 0o700); err != nil {
		return err
	}
	b, err := json.Marshal(c)
	if err != nil {
		return err
	}
	return os.WriteFile(s.connPath(volID), b, 0o600)
}

func (s *nodeServer) loadConn(volID string) (iscsi.Connector, error) {
	b, err := os.ReadFile(s.connPath(volID))
	if err != nil {
		return iscsi.Connector{}, err
	}
	var c iscsi.Connector
	return c, json.Unmarshal(b, &c)
}
