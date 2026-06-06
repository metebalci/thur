// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/metebalci/thur/csi/pkg/iscsi"
)

type fakeAttacher struct {
	device    string
	attached  []iscsi.Connector
	rescanned []iscsi.Connector
	detached  [][2]string
}

func (f *fakeAttacher) Attach(_ context.Context, c iscsi.Connector) (string, error) {
	f.attached = append(f.attached, c)
	return f.device, nil
}

func (f *fakeAttacher) Rescan(_ context.Context, c iscsi.Connector) (string, error) {
	f.rescanned = append(f.rescanned, c)
	return f.device, nil
}

func (f *fakeAttacher) Detach(_ context.Context, iqn, portal string) error {
	f.detached = append(f.detached, [2]string{iqn, portal})
	return nil
}

type fakeResizer struct {
	resized [][2]string
}

func (r *fakeResizer) Resize(devicePath, deviceMountPath string) (bool, error) {
	r.resized = append(r.resized, [2]string{devicePath, deviceMountPath})
	return true, nil
}

type fakeMounter struct {
	mounted   map[string]bool
	formatted []string
	bound     [][2]string
}

func newFakeMounter() *fakeMounter { return &fakeMounter{mounted: map[string]bool{}} }

func (m *fakeMounter) FormatAndMount(source, target, _ string, _ []string) error {
	m.formatted = append(m.formatted, source)
	m.mounted[target] = true
	return nil
}

func (m *fakeMounter) Mount(source, target, _ string, _ []string) error {
	m.bound = append(m.bound, [2]string{source, target})
	m.mounted[target] = true
	return nil
}

func (m *fakeMounter) Unmount(target string) error {
	delete(m.mounted, target)
	return nil
}

func (m *fakeMounter) IsLikelyNotMountPoint(file string) (bool, error) {
	if _, err := os.Stat(file); err != nil {
		return true, err
	}
	return !m.mounted[file], nil
}

func testNode(t *testing.T) (*nodeServer, *fakeAttacher, *fakeMounter) {
	ns, fa, fm, _ := testNodeR(t)
	return ns, fa, fm
}

func testNodeR(t *testing.T) (*nodeServer, *fakeAttacher, *fakeMounter, *fakeResizer) {
	t.Helper()
	fa := &fakeAttacher{device: "/dev/sdx"}
	fm := newFakeMounter()
	fr := &fakeResizer{}
	ns := &nodeServer{
		driver:   New(Config{Name: DefaultDriverName, NodeID: "node-1"}),
		attacher: fa,
		mounter:  fm,
		resizer:  fr,
		stateDir: t.TempDir(),
	}
	return ns, fa, fm, fr
}

func nodePubCtx() map[string]string {
	return map[string]string{
		PublishCtxIQN:        "iqn.2025-10.com.metebalci:thurvsa",
		PublishCtxPortal:     "10.0.0.5:3260",
		PublishCtxLUN:        "3",
		PublishCtxChapUser:   "csi-pvc-1",
		PublishCtxChapSecret: "abcdef0123456789",
	}
}

func blockCap() *csi.VolumeCapability {
	return &csi.VolumeCapability{
		AccessType: &csi.VolumeCapability_Block{Block: &csi.VolumeCapability_BlockVolume{}},
		AccessMode: &csi.VolumeCapability_AccessMode{Mode: csi.VolumeCapability_AccessMode_SINGLE_NODE_WRITER},
	}
}

func TestNodeStagePublishUnpublishUnstageMount(t *testing.T) {
	ns, fa, fm := testNode(t)
	ctx := context.Background()
	staging := filepath.Join(t.TempDir(), "staging")
	target := filepath.Join(t.TempDir(), "target")

	if _, err := ns.NodeStageVolume(ctx, &csi.NodeStageVolumeRequest{
		VolumeId: "pvc-1", StagingTargetPath: staging,
		VolumeCapability: singleNodeCaps()[0], PublishContext: nodePubCtx(),
	}); err != nil {
		t.Fatalf("stage: %v", err)
	}
	if len(fa.attached) != 1 || fa.attached[0].Lun != 3 || fa.attached[0].ChapUser != "csi-pvc-1" {
		t.Fatalf("attach not called with the connector: %+v", fa.attached)
	}
	if len(fm.formatted) != 1 || fm.formatted[0] != "/dev/sdx" {
		t.Errorf("device not formatted+mounted: %+v", fm.formatted)
	}
	if !fm.mounted[staging] {
		t.Errorf("staging not mounted")
	}
	if _, err := os.Stat(ns.connPath("pvc-1")); err != nil {
		t.Errorf("connector state not persisted: %v", err)
	}

	if _, err := ns.NodePublishVolume(ctx, &csi.NodePublishVolumeRequest{
		VolumeId: "pvc-1", StagingTargetPath: staging, TargetPath: target,
		VolumeCapability: singleNodeCaps()[0],
	}); err != nil {
		t.Fatalf("publish: %v", err)
	}
	if len(fm.bound) != 1 || fm.bound[0] != [2]string{staging, target} {
		t.Errorf("bind mount staging->target not issued: %+v", fm.bound)
	}

	if _, err := ns.NodeUnpublishVolume(ctx, &csi.NodeUnpublishVolumeRequest{VolumeId: "pvc-1", TargetPath: target}); err != nil {
		t.Fatalf("unpublish: %v", err)
	}
	if fm.mounted[target] {
		t.Errorf("target still mounted after unpublish")
	}

	if _, err := ns.NodeUnstageVolume(ctx, &csi.NodeUnstageVolumeRequest{VolumeId: "pvc-1", StagingTargetPath: staging}); err != nil {
		t.Fatalf("unstage: %v", err)
	}
	if fm.mounted[staging] {
		t.Errorf("staging still mounted after unstage")
	}
	if len(fa.detached) != 1 || fa.detached[0] != [2]string{"iqn.2025-10.com.metebalci:thurvsa", "10.0.0.5:3260"} {
		t.Errorf("detach not called: %+v", fa.detached)
	}
	if _, err := os.Stat(ns.connPath("pvc-1")); !os.IsNotExist(err) {
		t.Errorf("connector state not removed after unstage: %v", err)
	}
}

func TestNodeStageBlockSkipsFormat(t *testing.T) {
	ns, fa, fm := testNode(t)
	ctx := context.Background()
	staging := filepath.Join(t.TempDir(), "staging")
	target := filepath.Join(t.TempDir(), "blk", "target")

	if _, err := ns.NodeStageVolume(ctx, &csi.NodeStageVolumeRequest{
		VolumeId: "pvc-b", StagingTargetPath: staging,
		VolumeCapability: blockCap(), PublishContext: nodePubCtx(),
	}); err != nil {
		t.Fatalf("stage block: %v", err)
	}
	if len(fm.formatted) != 0 {
		t.Errorf("block volume must not be formatted: %+v", fm.formatted)
	}
	if len(fa.attached) != 1 {
		t.Errorf("attach not called for block stage")
	}

	if _, err := ns.NodePublishVolume(ctx, &csi.NodePublishVolumeRequest{
		VolumeId: "pvc-b", TargetPath: target, VolumeCapability: blockCap(), PublishContext: nodePubCtx(),
	}); err != nil {
		t.Fatalf("publish block: %v", err)
	}
	if len(fm.bound) != 1 || fm.bound[0] != [2]string{"/dev/sdx", target} {
		t.Errorf("block bind mount device->target not issued: %+v", fm.bound)
	}
}

func TestNodeStageBadPublishContext(t *testing.T) {
	ns, _, _ := testNode(t)
	staging := filepath.Join(t.TempDir(), "staging")
	bad := nodePubCtx()
	delete(bad, PublishCtxLUN)
	_, err := ns.NodeStageVolume(context.Background(), &csi.NodeStageVolumeRequest{
		VolumeId: "pvc-x", StagingTargetPath: staging, VolumeCapability: singleNodeCaps()[0], PublishContext: bad,
	})
	if status.Code(err) != codes.InvalidArgument {
		t.Fatalf("expected InvalidArgument for missing lun, got %v", err)
	}
}

func TestNodeUnstageIdempotent(t *testing.T) {
	ns, fa, _ := testNode(t)
	staging := filepath.Join(t.TempDir(), "staging")
	if err := os.MkdirAll(staging, 0o750); err != nil {
		t.Fatal(err)
	}
	// No prior stage: no connector state, so no detach, but still success.
	if _, err := ns.NodeUnstageVolume(context.Background(), &csi.NodeUnstageVolumeRequest{VolumeId: "absent", StagingTargetPath: staging}); err != nil {
		t.Fatalf("unstage with no state must succeed, got %v", err)
	}
	if len(fa.detached) != 0 {
		t.Errorf("detach should not run without persisted state: %+v", fa.detached)
	}
}

func TestNodeExpandFilesystem(t *testing.T) {
	ns, fa, _, fr := testNodeR(t)
	ctx := context.Background()
	staging := filepath.Join(t.TempDir(), "staging")
	volPath := filepath.Join(t.TempDir(), "published")

	// Stage first so the connector is persisted for the rescan.
	if _, err := ns.NodeStageVolume(ctx, &csi.NodeStageVolumeRequest{
		VolumeId: "pvc-x", StagingTargetPath: staging,
		VolumeCapability: singleNodeCaps()[0], PublishContext: nodePubCtx(),
	}); err != nil {
		t.Fatalf("stage: %v", err)
	}

	resp, err := ns.NodeExpandVolume(ctx, &csi.NodeExpandVolumeRequest{
		VolumeId: "pvc-x", VolumePath: volPath,
		CapacityRange:    &csi.CapacityRange{RequiredBytes: 2 << 30},
		VolumeCapability: singleNodeCaps()[0],
	})
	if err != nil {
		t.Fatalf("expand: %v", err)
	}
	if resp.GetCapacityBytes() != 2<<30 {
		t.Errorf("capacity = %d, want %d", resp.GetCapacityBytes(), 2<<30)
	}
	if len(fa.rescanned) != 1 {
		t.Errorf("session not rescanned: %+v", fa.rescanned)
	}
	if len(fr.resized) != 1 || fr.resized[0] != [2]string{"/dev/sdx", volPath} {
		t.Errorf("filesystem not resized with (device, volPath): %+v", fr.resized)
	}
}

func TestNodeExpandBlockSkipsResize(t *testing.T) {
	ns, fa, _, fr := testNodeR(t)
	ctx := context.Background()
	staging := filepath.Join(t.TempDir(), "staging")
	if _, err := ns.NodeStageVolume(ctx, &csi.NodeStageVolumeRequest{
		VolumeId: "pvc-b", StagingTargetPath: staging,
		VolumeCapability: blockCap(), PublishContext: nodePubCtx(),
	}); err != nil {
		t.Fatalf("stage block: %v", err)
	}
	if _, err := ns.NodeExpandVolume(ctx, &csi.NodeExpandVolumeRequest{
		VolumeId: "pvc-b", VolumePath: "/dev/whatever",
		CapacityRange: &csi.CapacityRange{RequiredBytes: 2 << 30}, VolumeCapability: blockCap(),
	}); err != nil {
		t.Fatalf("expand block: %v", err)
	}
	if len(fa.rescanned) != 1 {
		t.Errorf("block volume must still be rescanned")
	}
	if len(fr.resized) != 0 {
		t.Errorf("block volume must not resize a filesystem: %+v", fr.resized)
	}
}

func TestNodeExpandNoConnector(t *testing.T) {
	ns, _, _, _ := testNodeR(t)
	_, err := ns.NodeExpandVolume(context.Background(), &csi.NodeExpandVolumeRequest{
		VolumeId: "never-staged", VolumePath: filepath.Join(t.TempDir(), "p"),
		CapacityRange: &csi.CapacityRange{RequiredBytes: 2 << 30},
	})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("expected FailedPrecondition without staged connector, got %v", err)
	}
}

func TestNodeGetInfoAndCapabilities(t *testing.T) {
	ns, _, _ := testNode(t)
	info, err := ns.NodeGetInfo(context.Background(), &csi.NodeGetInfoRequest{})
	if err != nil || info.GetNodeId() != "node-1" {
		t.Fatalf("NodeGetInfo: %v / %q", err, info.GetNodeId())
	}
	caps, err := ns.NodeGetCapabilities(context.Background(), &csi.NodeGetCapabilitiesRequest{})
	if err != nil || len(caps.GetCapabilities()) == 0 {
		t.Fatalf("NodeGetCapabilities: %v / %d", err, len(caps.GetCapabilities()))
	}
}
