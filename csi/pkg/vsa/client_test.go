// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa_test

import (
	"context"
	"path/filepath"
	"testing"

	"github.com/metebalci/thur/csi/pkg/vsa"
	"github.com/metebalci/thur/csi/test/fake"
)

func startFake(t *testing.T) *vsa.Client {
	t.Helper()
	sock := filepath.Join(t.TempDir(), "admin.sock")
	d, err := fake.StartUnix(sock)
	if err != nil {
		t.Fatalf("start fake: %v", err)
	}
	t.Cleanup(func() { _ = d.Close() })
	return vsa.NewUnixClient(sock)
}

func TestHealth(t *testing.T) {
	if err := startFake(t).Health(context.Background()); err != nil {
		t.Fatalf("Health: %v", err)
	}
}

func TestCreateAndGetVolume(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	row, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "vol1", SizeBytes: 1 << 30})
	if err != nil {
		t.Fatalf("CreateVolume: %v", err)
	}
	if row.UUID == "" || row.Name != "vol1" || row.SizeBytes != 1<<30 {
		t.Fatalf("unexpected row: %+v", row)
	}
	if row.SectorBytes != 4096 {
		t.Errorf("sector_bytes = %d, want 4096", row.SectorBytes)
	}
	got, err := c.GetVolumeByName(ctx, "vol1")
	if err != nil || got == nil {
		t.Fatalf("GetVolumeByName: %+v / %v", got, err)
	}
	if got.UUID != row.UUID {
		t.Errorf("uuid mismatch: %s vs %s", got.UUID, row.UUID)
	}
	absent, err := c.GetVolumeByName(ctx, "nope")
	if err != nil {
		t.Fatalf("GetVolumeByName(absent): %v", err)
	}
	if absent != nil {
		t.Errorf("expected nil for absent volume, got %+v", absent)
	}
}

func TestCreateDuplicateConflict(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "dup", SizeBytes: 4096}); err != nil {
		t.Fatalf("first create: %v", err)
	}
	_, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "dup", SizeBytes: 4096})
	if !vsa.IsConflict(err) {
		t.Fatalf("expected conflict, got %v", err)
	}
}

func TestDeleteVolumeIdempotent(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "d", SizeBytes: 4096}); err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := c.DeleteVolume(ctx, "d"); err != nil {
		t.Fatalf("DeleteVolume: %v", err)
	}
	if err := c.DeleteVolume(ctx, "d"); !vsa.IsNotFound(err) {
		t.Fatalf("expected not-found on second delete, got %v", err)
	}
}

func TestResizeGrow(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "g", SizeBytes: 1 << 30}); err != nil {
		t.Fatalf("create: %v", err)
	}
	resp, err := c.ResizeVolume(ctx, "g", 2<<30)
	if err != nil {
		t.Fatalf("ResizeVolume: %v", err)
	}
	if resp.Previous != 1<<30 || resp.SizeBytes != 2<<30 {
		t.Fatalf("unexpected resize resp: %+v", resp)
	}
}

func TestSnapshotLifecycle(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "s", SizeBytes: 1 << 30}); err != nil {
		t.Fatalf("create: %v", err)
	}
	if _, err := c.CreateSnapshot(ctx, "s", "snap1"); err != nil {
		t.Fatalf("CreateSnapshot: %v", err)
	}
	snaps, err := c.ListSnapshots(ctx, "s")
	if err != nil || len(snaps) != 1 || snaps[0].Snapshot != "snap1" {
		t.Fatalf("ListSnapshots: %v / %+v", err, snaps)
	}
	if _, err := c.CreateSnapshot(ctx, "s", "snap1"); !vsa.IsConflict(err) {
		t.Fatalf("expected conflict on dup snapshot, got %v", err)
	}
	if err := c.DeleteSnapshot(ctx, "s", "snap1"); err != nil {
		t.Fatalf("DeleteSnapshot: %v", err)
	}
	if err := c.DeleteSnapshot(ctx, "s", "snap1"); !vsa.IsNotFound(err) {
		t.Fatalf("expected not-found on second delete, got %v", err)
	}
}

func TestClone(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "src", SizeBytes: 1 << 30}); err != nil {
		t.Fatalf("create: %v", err)
	}
	clone, err := c.CloneVolume(ctx, "src", vsa.CloneVolumeRequest{NewName: "clone1"})
	if err != nil {
		t.Fatalf("CloneVolume: %v", err)
	}
	if clone.Name != "clone1" || clone.SizeBytes != 1<<30 || clone.UUID == "" {
		t.Fatalf("unexpected clone: %+v", clone)
	}
}

func TestUserAdmission(t *testing.T) {
	c := startFake(t)
	ctx := context.Background()
	if _, err := c.CreateVolume(ctx, vsa.CreateVolumeRequest{Name: "v", SizeBytes: 4096}); err != nil {
		t.Fatalf("create: %v", err)
	}

	if _, err := c.AddUser(ctx, vsa.AddUserRequest{Username: "u", Password: "short", Volumes: []string{"v"}}); !vsa.IsBadRequest(err) {
		t.Fatalf("expected bad-request for short password, got %v", err)
	}
	if _, err := c.AddUser(ctx, vsa.AddUserRequest{Username: "u", Password: "abcdefghijkl"}); !vsa.IsBadRequest(err) {
		t.Fatalf("expected bad-request for empty volumes, got %v", err)
	}
	row, err := c.AddUser(ctx, vsa.AddUserRequest{Username: "u", Password: "abcdefghijkl", Volumes: []string{"v"}})
	if err != nil || row.Username != "u" {
		t.Fatalf("AddUser: %v / %+v", err, row)
	}
	if _, err := c.AddUser(ctx, vsa.AddUserRequest{Username: "u", Password: "abcdefghijkl", Volumes: []string{"v"}}); !vsa.IsConflict(err) {
		t.Fatalf("expected conflict for dup user, got %v", err)
	}
	if _, err := c.GrantUser(ctx, "u", []string{"v2"}); err != nil {
		t.Fatalf("GrantUser: %v", err)
	}
	ur, err := c.RevokeUser(ctx, "u", []string{"v2"})
	if err != nil || len(ur.Volumes) != 1 || ur.Volumes[0] != "v" {
		t.Fatalf("RevokeUser: %v / %+v", err, ur)
	}
	if _, err := c.RevokeUser(ctx, "u", []string{"v"}); !vsa.IsBadRequest(err) {
		t.Fatalf("expected bad-request revoking to empty, got %v", err)
	}
	if err := c.RemoveUser(ctx, "u"); err != nil {
		t.Fatalf("RemoveUser: %v", err)
	}
	if err := c.RemoveUser(ctx, "u"); !vsa.IsNotFound(err) {
		t.Fatalf("expected not-found removing twice, got %v", err)
	}
}
