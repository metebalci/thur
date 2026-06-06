// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package driver implements the Thur VSA CSI driver: the Identity, Controller,
// and Node gRPC services and the glue that wires them to a running thurvsad.
package driver

import (
	"fmt"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"k8s.io/klog/v2"
	"k8s.io/mount-utils"
	utilexec "k8s.io/utils/exec"

	"github.com/metebalci/thur/csi/pkg/grpcserver"
	"github.com/metebalci/thur/csi/pkg/iscsi"
	"github.com/metebalci/thur/csi/pkg/vsa"
)

// Defaults shared by the binary flags and tests.
const (
	DefaultDriverName = "thurvsa.csi.metebalci.com"
	DefaultTargetIQN  = "iqn.2025-10.com.metebalci:thurvsa"
)

// Mode selects which CSI services the process serves.
type Mode int

const (
	ModeAll Mode = iota
	ModeController
	ModeNode
)

// ParseMode maps the --mode flag to a Mode.
func ParseMode(s string) (Mode, error) {
	switch s {
	case "all":
		return ModeAll, nil
	case "controller":
		return ModeController, nil
	case "node":
		return ModeNode, nil
	default:
		return 0, fmt.Errorf("unknown mode %q (want controller, node, or all)", s)
	}
}

// String renders the mode for logs.
func (m Mode) String() string {
	switch m {
	case ModeController:
		return "controller"
	case ModeNode:
		return "node"
	default:
		return "all"
	}
}

func (m Mode) servesController() bool { return m == ModeAll || m == ModeController }
func (m Mode) servesNode() bool       { return m == ModeAll || m == ModeNode }

// Config is the fully-resolved driver configuration.
type Config struct {
	Name            string
	Version         string
	Mode            Mode
	NodeID          string
	Endpoint        string
	AdminSocket     string
	TargetIQN       string
	TargetPortal    string
	ChapStoreKind   string
	SecretNamespace string
	NodeStateDir    string
}

// Driver is the top-level CSI driver.
type Driver struct {
	cfg Config
}

// New builds a Driver from cfg.
func New(cfg Config) *Driver {
	return &Driver{cfg: cfg}
}

// Run registers the enabled services and serves until the process is signalled.
func (d *Driver) Run() error {
	srv := grpcserver.New(d.cfg.Endpoint)

	csi.RegisterIdentityServer(srv.Server(), &identityServer{driver: d})
	if d.cfg.Mode.servesController() {
		chap, err := buildChapStore(d.cfg)
		if err != nil {
			return err
		}
		csi.RegisterControllerServer(srv.Server(), &controllerServer{
			driver: d,
			vsa:    vsa.NewUnixClient(d.cfg.AdminSocket),
			chap:   chap,
		})
	}
	if d.cfg.Mode.servesNode() {
		csi.RegisterNodeServer(srv.Server(), &nodeServer{
			driver:   d,
			attacher: iscsi.NewAttacher(utilexec.New()),
			mounter:  mount.NewSafeFormatAndMount(mount.New(""), utilexec.New()),
			stateDir: d.cfg.NodeStateDir,
		})
	}

	klog.InfoS("starting thurvsa-csi",
		"name", d.cfg.Name, "version", d.cfg.Version,
		"mode", d.cfg.Mode, "endpoint", d.cfg.Endpoint)
	return srv.Serve()
}
