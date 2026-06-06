// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package iscsi drives the node-side iSCSI attach: an iscsiadm CHAP login and
// by-path device resolution. It shells out through a k8s.io/utils/exec
// Interface so the command sequence is unit-testable with a fake.
package iscsi

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"
	"time"

	"k8s.io/utils/exec"
)

const (
	iscsiadm  = "iscsiadm"
	devByPath = "/dev/disk/by-path"

	defaultPort     = "3260"
	deviceWaitTotal = 30 * time.Second
	deviceWaitStep  = 500 * time.Millisecond
)

// Connector is everything the node needs to attach a single iSCSI LUN. It is
// built from the controller's PublishContext and persisted at stage time so
// NodeUnstageVolume (which gets no PublishContext) can detach.
type Connector struct {
	TargetIQN  string `json:"targetIqn"`
	Portal     string `json:"portal"`
	Lun        uint32 `json:"lun"`
	ChapUser   string `json:"chapUser"`
	ChapSecret string `json:"chapSecret"`
}

// Attacher issues iscsiadm against an exec.Interface. resolve maps a Connector
// to its block device; it is the real by-path poll in production and is
// overridden in tests (no /dev there).
type Attacher struct {
	exec    exec.Interface
	resolve func(ctx context.Context, c Connector) (string, error)
}

// NewAttacher builds an Attacher over e.
func NewAttacher(e exec.Interface) *Attacher {
	a := &Attacher{exec: e}
	a.resolve = a.waitForDevice
	return a
}

// Attach configures per-volume CHAP, logs in, and returns the resolved block
// device path. Idempotent: a pre-existing session is tolerated.
func (a *Attacher) Attach(ctx context.Context, c Connector) (string, error) {
	portal := normalizePortal(c.Portal)
	c.Portal = portal

	if out, err := a.run(ctx, "-m", "discovery", "-t", "sendtargets", "-p", portal); err != nil {
		return "", fmt.Errorf("iscsi discovery on %s: %s: %w", portal, out, err)
	}
	for _, kv := range [][2]string{
		{"node.session.auth.authmethod", "CHAP"},
		{"node.session.auth.username", c.ChapUser},
		{"node.session.auth.password", c.ChapSecret},
	} {
		if out, err := a.run(ctx, "-m", "node", "-T", c.TargetIQN, "-p", portal,
			"-o", "update", "-n", kv[0], "-v", kv[1]); err != nil {
			return "", fmt.Errorf("iscsi set %s: %s: %w", kv[0], out, err)
		}
	}
	if out, err := a.run(ctx, "-m", "node", "-T", c.TargetIQN, "-p", portal, "--login"); err != nil {
		// exit 15 / "already" => a session already exists; not an error.
		if !strings.Contains(out, "already") {
			return "", fmt.Errorf("iscsi login to %s: %s: %w", c.TargetIQN, strings.TrimSpace(out), err)
		}
	}
	return a.resolve(ctx, c)
}

// Detach logs out of the target and removes its node DB record. Tolerates a
// missing session/record so NodeUnstageVolume is idempotent.
func (a *Attacher) Detach(ctx context.Context, iqn, portal string) error {
	portal = normalizePortal(portal)
	if out, err := a.run(ctx, "-m", "node", "-T", iqn, "-p", portal, "--logout"); err != nil {
		if !tolerable(out) {
			return fmt.Errorf("iscsi logout of %s: %s: %w", iqn, strings.TrimSpace(out), err)
		}
	}
	if out, err := a.run(ctx, "-m", "node", "-T", iqn, "-p", portal, "-o", "delete"); err != nil {
		if !tolerable(out) {
			return fmt.Errorf("iscsi node delete %s: %s: %w", iqn, strings.TrimSpace(out), err)
		}
	}
	return nil
}

func (a *Attacher) run(ctx context.Context, args ...string) (string, error) {
	out, err := a.exec.CommandContext(ctx, iscsiadm, args...).CombinedOutput()
	return string(out), err
}

// devicePath is the stable by-path symlink udev creates for the session's LUN.
func (c Connector) devicePath() string {
	return filepath.Join(devByPath,
		fmt.Sprintf("ip-%s-iscsi-%s-lun-%d", normalizePortal(c.Portal), c.TargetIQN, c.Lun))
}

// waitForDevice polls for the by-path symlink and resolves it to the real
// /dev node. iscsiadm --login returns before udev has created the link.
func (a *Attacher) waitForDevice(ctx context.Context, c Connector) (string, error) {
	link := c.devicePath()
	ctx, cancel := context.WithTimeout(ctx, deviceWaitTotal)
	defer cancel()
	for {
		if real, err := filepath.EvalSymlinks(link); err == nil {
			return real, nil
		}
		select {
		case <-ctx.Done():
			return "", fmt.Errorf("iscsi device %s did not appear: %w", link, ctx.Err())
		case <-time.After(deviceWaitStep):
		}
	}
}

// normalizePortal appends the default iSCSI port when the operator gave a bare
// host (the by-path link always carries host:port). IPv6 literals (which carry
// their own colons) must be bracketed by the caller and are left untouched.
func normalizePortal(p string) string {
	if p == "" || strings.Contains(p, ":") {
		return p
	}
	return p + ":" + defaultPort
}

func tolerable(out string) bool {
	return strings.Contains(out, "No matching") || strings.Contains(out, "not found")
}
