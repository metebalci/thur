// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import "github.com/container-storage-interface/spec/lib/go/csi"

// nodeServer implements csi.NodeServer. The method bodies land in the
// node-attach milestone; until then the embedded UnimplementedNodeServer
// answers codes.Unimplemented.
type nodeServer struct {
	csi.UnimplementedNodeServer
	driver *Driver
}
