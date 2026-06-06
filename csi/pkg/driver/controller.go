// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import "github.com/container-storage-interface/spec/lib/go/csi"

// controllerServer implements csi.ControllerServer. The method bodies land in
// later milestones (provisioning, publish, snapshots, expand); until then the
// embedded UnimplementedControllerServer answers codes.Unimplemented.
type controllerServer struct {
	csi.UnimplementedControllerServer
	driver *Driver
}
