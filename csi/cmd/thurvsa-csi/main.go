// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Command thurvsa-csi is the Kubernetes CSI driver for Thur VSA. A single
// binary serves the Identity service plus, selected by --mode, the Controller
// and/or Node services.
package main

import (
	"flag"
	"fmt"
	"os"

	"k8s.io/klog/v2"

	"github.com/metebalci/thur/csi/pkg/driver"
)

// version is overridden at build time via -ldflags "-X main.version=...".
var version = "dev"

func main() {
	var (
		endpoint     = flag.String("endpoint", "unix:///csi/csi.sock", "CSI gRPC endpoint the kubelet/sidecars dial")
		mode         = flag.String("mode", "all", "which services to serve: controller, node, or all")
		nodeID       = flag.String("node-id", "", "node identifier (required for node mode)")
		adminSocket  = flag.String("admin-socket", "/run/thurvsa/admin.sock", "thurvsad admin unix socket (controller mode)")
		driverName   = flag.String("drivername", driver.DefaultDriverName, "CSI driver name")
		targetIQN    = flag.String("target-iqn", driver.DefaultTargetIQN, "iSCSI target IQN")
		targetPortal = flag.String("target-portal", "", "iSCSI target portal host:port (overrides the StorageClass param when set)")
		chapStore    = flag.String("chap-secret-store", "kubernetes", "where per-volume CHAP secrets live: kubernetes or memory (memory = tests / non-cluster runs)")
		secretNS     = flag.String("secret-namespace", os.Getenv("POD_NAMESPACE"), "namespace for CHAP secrets (defaults to POD_NAMESPACE, else the service-account namespace)")
		showVersion  = flag.Bool("version", false, "print version and exit")
	)
	klog.InitFlags(nil)
	flag.Parse()

	if *showVersion {
		fmt.Printf("thurvsa-csi %s\n", version)
		return
	}

	m, err := driver.ParseMode(*mode)
	if err != nil {
		klog.ErrorS(err, "invalid --mode")
		os.Exit(2)
	}

	d := driver.New(driver.Config{
		Name:            *driverName,
		Version:         version,
		Mode:            m,
		NodeID:          *nodeID,
		Endpoint:        *endpoint,
		AdminSocket:     *adminSocket,
		TargetIQN:       *targetIQN,
		TargetPortal:    *targetPortal,
		ChapStoreKind:   *chapStore,
		SecretNamespace: *secretNS,
	})

	if err := d.Run(); err != nil {
		klog.ErrorS(err, "driver exited with error")
		os.Exit(1)
	}
}
