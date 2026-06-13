// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package iscsi drives the node-side iSCSI attach: an iscsiadm CHAP login and
// by-path device resolution. It shells out through a k8s.io/utils/exec
// Interface so the command sequence is unit-testable with a fake.
package iscsi

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"k8s.io/utils/exec"
)

const (
	iscsiadm   = "iscsiadm"
	devByPath  = "/dev/disk/by-path"
	sysfsBlock = "/sys/block"

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
	// Serial is the expected SCSI Unit Serial Number of the volume
	// (VSA reports the volume UUID hex in VPD 0x80). When set, Attach
	// verifies the resolved by-path device actually carries it before
	// returning — LUN numbers are reused smallest-gap-first, so without
	// this check a stale device from a deleted volume that held the same
	// LUN could be handed to a new volume (cross-volume corruption,
	// issue #149). Empty disables the check (older PublishContexts).
	Serial string `json:"serial,omitempty"`
}

// Attacher issues iscsiadm against an exec.Interface. resolve maps a Connector
// to its block device; it is the real by-path poll in production and is
// overridden in tests (no /dev there). cmd is the iscsiadm invocation (default
// ["iscsiadm"]); the node wraps it in nsenter so the *host's* iscsiadm runs —
// iscsiadm and iscsid must be the same open-iscsi version, and the container's
// bundled copy almost never matches the host's iscsid.
type Attacher struct {
	exec    exec.Interface
	cmd     []string
	resolve func(ctx context.Context, c Connector) (string, error)
	// verifyIdentity confirms a resolved device carries the expected
	// volume serial (issue #149). Real sysfs reader in production,
	// overridden in tests (no /sys there).
	verifyIdentity func(ctx context.Context, dev, expectedSerial string) error
}

// NewAttacher builds an Attacher over e. base is the iscsiadm invocation
// (e.g. ["iscsiadm"] or an nsenter wrapper); empty defaults to ["iscsiadm"].
func NewAttacher(e exec.Interface, base []string) *Attacher {
	a := &Attacher{exec: e, cmd: base}
	a.resolve = a.waitForDevice
	a.verifyIdentity = defaultVerifyIdentity
	return a
}

// Attach configures per-node CHAP, logs in, rescans, and returns the
// resolved block device path. Idempotent: a pre-existing session is
// tolerated. A node holds one iSCSI session per (target IQN, portal) and
// mounts every volume granted to its CHAP user through it (issue #15), so
// when this volume was granted after the session came up, the rescan is
// what makes its just-admitted LUN appear.
func (a *Attacher) Attach(ctx context.Context, c Connector) (string, error) {
	portal := normalizePortal(c.Portal)
	c.Portal = portal

	// Create the node DB record directly from the target coordinates the
	// controller handed us in PublishContext. SendTargets discovery is only for
	// *learning* unknown targets — we already know the IQN + portal — and a
	// discovery-session login can stall when the target requires CHAP. `-o new`
	// is tolerant of an existing record (retry-safe).
	if out, err := a.run(ctx, "-m", "node", "-o", "new", "-T", c.TargetIQN, "-p", portal); err != nil {
		if !strings.Contains(out, "already") {
			return "", fmt.Errorf("iscsi node create %s: %s: %w", c.TargetIQN, strings.TrimSpace(out), err)
		}
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
	// Force a SCSI rescan so a LUN granted to this node's CHAP user
	// *after* the session came up becomes visible — `--login` is a no-op
	// ("already") on an existing session and would not surface it. The
	// daemon re-reads REPORT LUNS dynamically (issue #15), so the rescan
	// picks up the newly-admitted LUN. Harmless on a fresh login.
	if out, err := a.run(ctx, "-m", "node", "-T", c.TargetIQN, "-p", portal, "-R"); err != nil {
		if !tolerable(out) {
			return "", fmt.Errorf("iscsi rescan %s: %s: %w", c.TargetIQN, strings.TrimSpace(out), err)
		}
	}
	dev, err := a.resolve(ctx, c)
	if err != nil {
		return "", err
	}
	// Verify the resolved device is actually THIS volume's, not a stale
	// device left from a deleted volume that held the same (reused) LUN
	// (issue #149). Skipped when the controller supplied no serial.
	if c.Serial != "" && a.verifyIdentity != nil {
		if err := a.verifyIdentity(ctx, dev, c.Serial); err != nil {
			return "", err
		}
	}
	return dev, nil
}

// DeleteDevice removes the kernel SCSI device backing this connector's
// LUN. A node keeps its single iSCSI session up while any volume remains
// staged, and `iscsiadm -R` only discovers new LUNs — it never deletes a
// dropped one. So at unstage we must explicitly delete the device, else
// it lingers and a later volume reusing the same LUN resolves the stale
// node (issue #149). Best-effort: a missing device is success.
func (a *Attacher) DeleteDevice(_ context.Context, c Connector) error {
	c.Portal = normalizePortal(c.Portal)
	real, err := filepath.EvalSymlinks(c.devicePath())
	if err != nil {
		return nil // already gone
	}
	p := filepath.Join(sysfsBlock, filepath.Base(real), "device", "delete")
	if err := os.WriteFile(p, []byte("1"), 0o200); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("delete stale device %s: %w", real, err)
	}
	return nil
}

// Rescan re-scans the target's sessions so the kernel observes a grown LUN,
// then re-resolves and returns the (unchanged) device path. Used by node-side
// volume expansion.
func (a *Attacher) Rescan(ctx context.Context, c Connector) (string, error) {
	portal := normalizePortal(c.Portal)
	c.Portal = portal
	if out, err := a.run(ctx, "-m", "node", "-T", c.TargetIQN, "-p", portal, "-R"); err != nil {
		if !tolerable(out) {
			return "", fmt.Errorf("iscsi rescan %s: %s: %w", c.TargetIQN, strings.TrimSpace(out), err)
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
	base := a.cmd
	if len(base) == 0 {
		base = []string{iscsiadm}
	}
	full := append(append([]string{}, base[1:]...), args...)
	out, err := a.exec.CommandContext(ctx, base[0], full...).CombinedOutput()
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

// defaultVerifyIdentity forces a per-device revalidate (so capacity /
// page-cache / identity reflect the current LUN, not a stale one) and
// compares the device's SCSI Unit Serial Number against the expected
// volume serial (issue #149).
func defaultVerifyIdentity(_ context.Context, dev, expected string) error {
	base := filepath.Base(dev)
	// Force the kernel to re-read the device (capacity + VPD). Best-effort
	// — a failure here just means we read whatever sysfs already has.
	_ = os.WriteFile(filepath.Join(sysfsBlock, base, "device", "rescan"), []byte("1"), 0o200)
	got, err := readUnitSerialPg80(base)
	if err != nil {
		return fmt.Errorf("read device identity for %s: %w", dev, err)
	}
	if !serialMatches(got, expected) {
		return fmt.Errorf(
			"device %s serial %q does not match expected volume serial %q "+
				"(stale LUN device from a deleted volume?)",
			dev, got, expected)
	}
	return nil
}

// readUnitSerialPg80 reads the device's VPD page 0x80 (Unit Serial
// Number) from sysfs and returns the ASCII serial. The 4-byte page
// header (peripheral qualifier/type, page code, reserved, page length)
// precedes the serial bytes.
func readUnitSerialPg80(base string) (string, error) {
	raw, err := os.ReadFile(filepath.Join(sysfsBlock, base, "device", "vpd_pg80"))
	if err != nil {
		return "", err
	}
	if len(raw) < 4 {
		return "", fmt.Errorf("vpd_pg80 too short (%d bytes)", len(raw))
	}
	end := 4 + int(raw[3])
	if end > len(raw) {
		end = len(raw)
	}
	return strings.TrimSpace(string(raw[4:end])), nil
}

// serialMatches compares two SCSI serials after normalizing (lowercase,
// strip dashes) — VSA reports the volume UUID hex; the controller may
// carry it dashed or cased differently. A non-empty match is required.
func serialMatches(got, want string) bool {
	n := func(s string) string {
		return strings.ToLower(strings.ReplaceAll(strings.TrimSpace(s), "-", ""))
	}
	gn, wn := n(got), n(want)
	return wn != "" && gn == wn
}
