// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import "github.com/container-storage-interface/spec/lib/go/csi"

// controllerCapabilities is the set of controller RPCs the driver implements.
// Capabilities are added as their milestones land; PUBLISH_UNPUBLISH_VOLUME,
// EXPAND_VOLUME, and CREATE_DELETE_SNAPSHOT are filled in later.
func controllerCapabilities() []*csi.ControllerServiceCapability {
	rpcs := []csi.ControllerServiceCapability_RPC_Type{
		csi.ControllerServiceCapability_RPC_CREATE_DELETE_VOLUME,
		csi.ControllerServiceCapability_RPC_CLONE_VOLUME,
		csi.ControllerServiceCapability_RPC_LIST_VOLUMES,
	}
	caps := make([]*csi.ControllerServiceCapability, 0, len(rpcs))
	for _, r := range rpcs {
		caps = append(caps, &csi.ControllerServiceCapability{
			Type: &csi.ControllerServiceCapability_Rpc{
				Rpc: &csi.ControllerServiceCapability_RPC{Type: r},
			},
		})
	}
	return caps
}

// isSupportedAccessMode reports whether mode is one the driver supports. v1 is
// single-node only (per-volume CHAP + a single iSCSI session is single-attach).
func isSupportedAccessMode(mode csi.VolumeCapability_AccessMode_Mode) bool {
	switch mode {
	case csi.VolumeCapability_AccessMode_SINGLE_NODE_WRITER,
		csi.VolumeCapability_AccessMode_SINGLE_NODE_READER_ONLY,
		csi.VolumeCapability_AccessMode_SINGLE_NODE_SINGLE_WRITER,
		csi.VolumeCapability_AccessMode_SINGLE_NODE_MULTI_WRITER:
		return true
	default:
		return false
	}
}

// capsSupported reports whether every capability uses a supported access mode.
func capsSupported(caps []*csi.VolumeCapability) bool {
	for _, c := range caps {
		if !isSupportedAccessMode(c.GetAccessMode().GetMode()) {
			return false
		}
	}
	return len(caps) > 0
}
