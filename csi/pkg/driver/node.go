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
	"strings"
	"sync"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/iscsi"
)

// attacher is the node-side iSCSI surface (pkg/iscsi.Attacher in production,
// faked in tests).
type attacher interface {
	Attach(ctx context.Context, c iscsi.Connector) (string, error)
	DeleteDevice(ctx context.Context, c iscsi.Connector) error
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
	// Serializes NodeStageVolume / NodeUnstageVolume. kubelet runs
	// per-volume operations concurrently, and all volumes on a node share
	// one iSCSI session; without this, an unstage's last-connector scan
	// could race a concurrent stage's Attach-to-saveConn window — seeing
	// an empty dir and logging out the session the stage just
	// established, so the staging volume's device vanishes (issue #292).
	// Stage/unstage are infrequent, so a node-wide lock is acceptable.
	mu sync.Mutex
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

	// Serialize against a concurrent unstage's last-connector scan (#292).
	s.mu.Lock()
	defer s.mu.Unlock()

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
	// Serialize against a concurrent stage's Attach-to-saveConn window so
	// the last-connector scan below can't race it (#292).
	s.mu.Lock()
	defer s.mu.Unlock()

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
	// Every volume a node mounts shares one iSCSI session (one (target
	// IQN, portal) per node under per-node CHAP, issue #15), so a logout
	// would tear every other volume's LUN off this node. We therefore log
	// out only when this was the last staged volume — and crucially we
	// remove this volume's connector file LAST, after the device delete
	// and (if last) the logout both succeed. The connector is the only
	// state this RPC has (it gets no PublishContext); removing it before
	// the teardown meant a failed Detach returned Internal but the
	// kubelet retry then hit os.IsNotExist on loadConn and returned
	// success without ever logging out — leaking the session, the
	// open-iscsi node DB record (with since-revoked CHAP creds), and the
	// stale devices until reboot (issue #228). Keeping the connector
	// until the end makes the retry re-run the full teardown; DeleteDevice
	// and Detach both tolerate already-cleaned state, so the rerun is
	// idempotent.
	//
	// Delete this LUN's kernel device so it can't linger and be handed to
	// a later volume that reuses the same LUN number (issue #149). The
	// session stays up for the node's other volumes; iscsiadm -R never
	// removes a dropped LUN, so we must do it explicitly here.
	if err := s.attacher.DeleteDevice(ctx, conn); err != nil {
		return nil, status.Errorf(codes.Internal, "delete stale device: %v", err)
	}
	// Last-volume check excludes this volume's own (still-present)
	// connector — "last" means no OTHER connector remains. The daemon
	// revokes the volume from the node's CHAP user on
	// ControllerUnpublishVolume; a surviving session drops the
	// now-unadmitted LUN on its next rescan.
	last, err := s.isLastConnector(req.GetVolumeId())
	if err != nil {
		return nil, status.Errorf(codes.Internal, "scan node state: %v", err)
	}
	if last {
		if err := s.attacher.Detach(ctx, conn.TargetIQN, conn.Portal); err != nil {
			return nil, status.Errorf(codes.Internal, "iscsi detach: %v", err)
		}
	}
	// Teardown succeeded — now drop this volume's connector. Until this
	// point any earlier failure left it on disk so the retry re-ran.
	if path, err := s.connPath(req.GetVolumeId()); err == nil {
		_ = os.Remove(path)
	}
	return &csi.NodeUnstageVolumeResponse{}, nil
}

func (s *nodeServer) NodeExpandVolume(ctx context.Context, req *csi.NodeExpandVolumeRequest) (*csi.NodeExpandVolumeResponse, error) {
	if req.GetVolumeId() == "" || req.GetVolumePath() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id and volume_path are required")
	}
	conn, err := s.loadConn(req.GetVolumeId())
	if err != nil {
		if os.IsNotExist(err) {
			// No persisted connector means the volume was never staged on this
			// node (or has been unstaged): NOT_FOUND from the node's view.
			return nil, status.Errorf(codes.NotFound, "volume %q is not staged on this node", req.GetVolumeId())
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
		Serial:     pc[PublishCtxSerial],
	}
	if c.TargetIQN == "" || c.Portal == "" {
		return c, fmt.Errorf("publish context missing iqn or portal")
	}
	return c, nil
}

func (s *nodeServer) connPath(volID string) (string, error) {
	// Reject volume_ids that aren't a valid VSA volume name. The driver
	// only mints [A-Za-z0-9_-] names, but a '/' or '../' in a
	// statically-provisioned PV's volumeHandle — which never runs through
	// the CreateVolume sanitizer — would otherwise let saveConn write the
	// connector JSON (including the CHAP secret) to an arbitrary host path
	// as root, and loadConn read arbitrary .json files: path traversal out
	// of --node-state-dir on the privileged node plugin (issue #293).
	if !isValidVolumeName(volID) {
		return "", fmt.Errorf("invalid volume id %q: must be 1-%d chars of [A-Za-z0-9_-]", volID, maxVolumeNameLen)
	}
	return filepath.Join(s.stateDir, volID+".json"), nil
}

// isLastConnector reports whether the volume being unstaged (excludeVolID)
// is the last one sharing the node's single iSCSI session — i.e. no OTHER
// per-volume connector file remains — so it is safe to log out. It is
// called BEFORE the volume's own connector is removed (issue #228), so
// that file is excluded from the count. A missing dir counts as "last"
// (nothing left to keep the session for).
func (s *nodeServer) isLastConnector(excludeVolID string) (bool, error) {
	entries, err := os.ReadDir(s.stateDir)
	if err != nil {
		if os.IsNotExist(err) {
			return true, nil
		}
		return false, err
	}
	self := excludeVolID + ".json"
	for _, e := range entries {
		if !e.IsDir() && strings.HasSuffix(e.Name(), ".json") && e.Name() != self {
			return false, nil
		}
	}
	return true, nil
}

func (s *nodeServer) saveConn(volID string, c iscsi.Connector) error {
	if err := os.MkdirAll(s.stateDir, 0o700); err != nil {
		return err
	}
	b, err := json.Marshal(c)
	if err != nil {
		return err
	}
	path, err := s.connPath(volID)
	if err != nil {
		return err
	}
	return os.WriteFile(path, b, 0o600)
}

func (s *nodeServer) loadConn(volID string) (iscsi.Connector, error) {
	path, err := s.connPath(volID)
	if err != nil {
		return iscsi.Connector{}, err
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return iscsi.Connector{}, err
	}
	var c iscsi.Connector
	return c, json.Unmarshal(b, &c)
}
