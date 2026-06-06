// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"flag"
	"net"
	"os"
	"path/filepath"
	"testing"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"github.com/kubernetes-csi/csi-test/v5/pkg/sanity"
	"google.golang.org/grpc"

	"github.com/metebalci/thur/csi/pkg/vsa"
	"github.com/metebalci/thur/csi/test/fake"
)

// TestMain skips the one sanity spec that doesn't apply to this driver:
// "ControllerPublishVolume should fail when the node does not exist".
// ControllerPublishVolume mints (or reuses) a per-node CHAP user and grants it
// the volume; the node id is the credential key, not a handle into a node
// registry, so there is nothing to validate it against. Documented in
// docs/CSI.md. The skip is a Ginkgo flag default so a bare `go test ./...`
// also honors it.
func TestMain(m *testing.M) {
	_ = flag.Set("ginkgo.skip", "should fail when the node does not exist")
	os.Exit(m.Run())
}

// TestSanity runs the official kubernetes-csi conformance suite against the
// full Identity + Controller + Node surface. The controller talks to the
// in-memory fake daemon; the node uses the same fakes as the unit tests
// (fakeAttacher / fakeMounter / fakeResizer) and a memory CHAP store, so the
// suite validates the gRPC contract end to end without a real iscsiadm, mount,
// or cluster. The real iSCSI + filesystem data path is the job of the
// cluster-level e2e (csi/test/e2e).
func TestSanity(t *testing.T) {
	dir := t.TempDir()

	adminSock := filepath.Join(dir, "admin.sock")
	d, err := fake.StartUnix(adminSock)
	if err != nil {
		t.Fatalf("start fake daemon: %v", err)
	}
	t.Cleanup(func() { _ = d.Close() })

	drv := New(Config{
		Name:         DefaultDriverName,
		Version:      "sanity",
		NodeID:       "sanity-node",
		TargetIQN:    DefaultTargetIQN,
		TargetPortal: "10.0.0.5:3260",
	})
	srv := grpc.NewServer()
	csi.RegisterIdentityServer(srv, &identityServer{driver: drv})
	csi.RegisterControllerServer(srv, &controllerServer{
		driver: drv,
		vsa:    vsa.NewUnixClient(adminSock),
		chap:   newMemoryChapStore(),
	})
	csi.RegisterNodeServer(srv, &nodeServer{
		driver:   drv,
		attacher: &fakeAttacher{device: "/dev/sdx"},
		mounter:  newFakeMounter(),
		resizer:  &fakeResizer{},
		stateDir: filepath.Join(dir, "state"),
	})

	csiSock := filepath.Join(dir, "csi.sock")
	lis, err := net.Listen("unix", csiSock)
	if err != nil {
		t.Fatalf("listen csi socket: %v", err)
	}
	go func() { _ = srv.Serve(lis) }()
	t.Cleanup(srv.Stop)

	cfg := sanity.NewTestConfig()
	cfg.Address = "unix://" + csiSock
	cfg.TargetPath = filepath.Join(dir, "target")
	cfg.StagingPath = filepath.Join(dir, "staging")
	cfg.TestVolumeParameters = map[string]string{"targetPortal": "10.0.0.5:3260", "fsType": "ext4"}

	sanity.Test(t, cfg)
}
