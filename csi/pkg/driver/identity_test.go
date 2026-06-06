// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"testing"

	"github.com/container-storage-interface/spec/lib/go/csi"
)

func testDriver() *Driver {
	return New(Config{Name: DefaultDriverName, Version: "1.2.3", Mode: ModeAll})
}

func TestGetPluginInfo(t *testing.T) {
	s := &identityServer{driver: testDriver()}
	resp, err := s.GetPluginInfo(context.Background(), &csi.GetPluginInfoRequest{})
	if err != nil {
		t.Fatalf("GetPluginInfo: %v", err)
	}
	if resp.GetName() != DefaultDriverName {
		t.Errorf("name = %q, want %q", resp.GetName(), DefaultDriverName)
	}
	if resp.GetVendorVersion() != "1.2.3" {
		t.Errorf("version = %q, want 1.2.3", resp.GetVendorVersion())
	}
}

func TestGetPluginInfoUnconfigured(t *testing.T) {
	s := &identityServer{driver: New(Config{})}
	if _, err := s.GetPluginInfo(context.Background(), &csi.GetPluginInfoRequest{}); err == nil {
		t.Fatal("expected error for empty driver name")
	}
}

func TestGetPluginCapabilities(t *testing.T) {
	s := &identityServer{driver: testDriver()}
	resp, err := s.GetPluginCapabilities(context.Background(), &csi.GetPluginCapabilitiesRequest{})
	if err != nil {
		t.Fatalf("GetPluginCapabilities: %v", err)
	}
	var hasController, hasExpansion bool
	for _, c := range resp.GetCapabilities() {
		if svc := c.GetService(); svc != nil && svc.GetType() == csi.PluginCapability_Service_CONTROLLER_SERVICE {
			hasController = true
		}
		if exp := c.GetVolumeExpansion(); exp != nil && exp.GetType() == csi.PluginCapability_VolumeExpansion_ONLINE {
			hasExpansion = true
		}
	}
	if !hasController {
		t.Error("missing CONTROLLER_SERVICE capability")
	}
	if !hasExpansion {
		t.Error("missing ONLINE volume expansion capability")
	}
}

func TestProbe(t *testing.T) {
	s := &identityServer{driver: testDriver()}
	resp, err := s.Probe(context.Background(), &csi.ProbeRequest{})
	if err != nil {
		t.Fatalf("Probe: %v", err)
	}
	if !resp.GetReady().GetValue() {
		t.Error("Probe not ready")
	}
}

func TestParseMode(t *testing.T) {
	for _, tc := range []struct {
		in   string
		want Mode
		ok   bool
	}{
		{"all", ModeAll, true},
		{"controller", ModeController, true},
		{"node", ModeNode, true},
		{"bogus", 0, false},
	} {
		got, err := ParseMode(tc.in)
		if tc.ok && (err != nil || got != tc.want) {
			t.Errorf("ParseMode(%q) = %v, %v; want %v, nil", tc.in, got, err, tc.want)
		}
		if !tc.ok && err == nil {
			t.Errorf("ParseMode(%q) expected error", tc.in)
		}
	}
}
