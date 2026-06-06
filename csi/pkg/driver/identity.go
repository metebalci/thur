// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/wrapperspb"
)

// identityServer implements csi.IdentityServer.
type identityServer struct {
	csi.UnimplementedIdentityServer
	driver *Driver
}

func (s *identityServer) GetPluginInfo(_ context.Context, _ *csi.GetPluginInfoRequest) (*csi.GetPluginInfoResponse, error) {
	if s.driver.cfg.Name == "" {
		return nil, status.Error(codes.Unavailable, "driver name not configured")
	}
	return &csi.GetPluginInfoResponse{
		Name:          s.driver.cfg.Name,
		VendorVersion: s.driver.cfg.Version,
	}, nil
}

func (s *identityServer) GetPluginCapabilities(_ context.Context, _ *csi.GetPluginCapabilitiesRequest) (*csi.GetPluginCapabilitiesResponse, error) {
	return &csi.GetPluginCapabilitiesResponse{
		Capabilities: []*csi.PluginCapability{
			{
				Type: &csi.PluginCapability_Service_{
					Service: &csi.PluginCapability_Service{
						Type: csi.PluginCapability_Service_CONTROLLER_SERVICE,
					},
				},
			},
			{
				Type: &csi.PluginCapability_VolumeExpansion_{
					VolumeExpansion: &csi.PluginCapability_VolumeExpansion{
						Type: csi.PluginCapability_VolumeExpansion_ONLINE,
					},
				},
			},
		},
	}, nil
}

func (s *identityServer) Probe(_ context.Context, _ *csi.ProbeRequest) (*csi.ProbeResponse, error) {
	// M1: always-ready. A later milestone wires an admin-socket health ping in
	// controller mode so Probe reflects thurvsad reachability.
	return &csi.ProbeResponse{Ready: wrapperspb.Bool(true)}, nil
}
