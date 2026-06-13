// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package iscsi

import (
	"context"
	"fmt"
	"strings"
	"testing"

	"k8s.io/utils/exec"
	testingexec "k8s.io/utils/exec/testing"
)

// recExec returns a fake exec that records the argv of the next n commands and
// reports success for each.
func recExec(n int) (*testingexec.FakeExec, *[][]string) {
	calls := &[][]string{}
	fe := &testingexec.FakeExec{}
	for i := 0; i < n; i++ {
		fe.CommandScript = append(fe.CommandScript, func(cmd string, args ...string) exec.Cmd {
			*calls = append(*calls, append([]string{cmd}, args...))
			return &testingexec.FakeCmd{DisableScripts: true}
		})
	}
	return fe, calls
}

func okCmd() testingexec.FakeCommandAction {
	return func(_ string, _ ...string) exec.Cmd { return &testingexec.FakeCmd{DisableScripts: true} }
}

func errCmd(out string, err error) testingexec.FakeCommandAction {
	return func(_ string, _ ...string) exec.Cmd {
		return &testingexec.FakeCmd{CombinedOutputScript: []testingexec.FakeAction{
			func() ([]byte, []byte, error) { return []byte(out), nil, err },
		}}
	}
}

func wantCmd(t *testing.T, calls [][]string, i int, want string) {
	t.Helper()
	if i >= len(calls) {
		t.Fatalf("missing command #%d (want %q)", i, want)
	}
	if got := strings.Join(calls[i], " "); got != want {
		t.Errorf("command #%d:\n got %q\nwant %q", i, got, want)
	}
}

func TestAttachIssuesExpectedCommands(t *testing.T) {
	fe, calls := recExec(6)
	a := &Attacher{exec: fe, resolve: func(_ context.Context, _ Connector) (string, error) { return "/dev/sdx", nil }}

	dev, err := a.Attach(context.Background(), Connector{
		TargetIQN: "iqn.2025-10.com.metebalci:thurvsa", Portal: "10.0.0.5", Lun: 3,
		ChapUser: "csi-pvc-1", ChapSecret: "deadbeef",
	})
	if err != nil {
		t.Fatalf("Attach: %v", err)
	}
	if dev != "/dev/sdx" {
		t.Errorf("device = %q, want /dev/sdx", dev)
	}
	const iqn = "iqn.2025-10.com.metebalci:thurvsa"
	const p = "10.0.0.5:3260" // default port appended
	wantCmd(t, *calls, 0, "iscsiadm -m node -o new -T "+iqn+" -p "+p)
	wantCmd(t, *calls, 1, "iscsiadm -m node -T "+iqn+" -p "+p+" -o update -n node.session.auth.authmethod -v CHAP")
	wantCmd(t, *calls, 2, "iscsiadm -m node -T "+iqn+" -p "+p+" -o update -n node.session.auth.username -v csi-pvc-1")
	wantCmd(t, *calls, 3, "iscsiadm -m node -T "+iqn+" -p "+p+" -o update -n node.session.auth.password -v deadbeef")
	wantCmd(t, *calls, 4, "iscsiadm -m node -T "+iqn+" -p "+p+" --login")
	// Rescan after login so a LUN granted to the node's CHAP user after
	// the session came up becomes visible (per-node CHAP, issue #15).
	wantCmd(t, *calls, 5, "iscsiadm -m node -T "+iqn+" -p "+p+" -R")
}

func TestAttachVerifiesDeviceIdentity(t *testing.T) {
	// Matching serial → attach succeeds.
	fe, _ := recExec(6)
	a := &Attacher{
		exec:           fe,
		resolve:        func(_ context.Context, _ Connector) (string, error) { return "/dev/sdx", nil },
		verifyIdentity: func(_ context.Context, _, expected string) error {
			if !serialMatches("00112233", expected) {
				return fmt.Errorf("mismatch")
			}
			return nil
		},
	}
	if _, err := a.Attach(context.Background(), Connector{
		TargetIQN: "iqn.x", Portal: "10.0.0.5", ChapUser: "u", ChapSecret: "s", Serial: "00112233",
	}); err != nil {
		t.Fatalf("matching identity should attach: %v", err)
	}

	// Mismatching serial (stale LUN device) → attach must fail (issue #149).
	fe2, _ := recExec(6)
	a2 := &Attacher{
		exec:           fe2,
		resolve:        func(_ context.Context, _ Connector) (string, error) { return "/dev/sdx", nil },
		verifyIdentity: func(_ context.Context, _, _ string) error { return fmt.Errorf("stale device") },
	}
	if _, err := a2.Attach(context.Background(), Connector{
		TargetIQN: "iqn.x", Portal: "10.0.0.5", ChapUser: "u", ChapSecret: "s", Serial: "deadbeef",
	}); err == nil {
		t.Fatal("mismatched device identity must fail the attach")
	}
}

func TestSerialMatchesNormalizes(t *testing.T) {
	if !serialMatches("ABCD-1234", "abcd1234") {
		t.Error("serial compare must normalize case and dashes")
	}
	if serialMatches("aaaa", "bbbb") {
		t.Error("different serials must not match")
	}
	if serialMatches("", "") {
		t.Error("empty serial must never match")
	}
}

func TestAttachToleratesExistingSession(t *testing.T) {
	fe := &testingexec.FakeExec{}
	for i := 0; i < 4; i++ {
		fe.CommandScript = append(fe.CommandScript, okCmd())
	}
	fe.CommandScript = append(fe.CommandScript, errCmd("iscsiadm: the session already exists", fmt.Errorf("exit status 15")))
	// The post-login rescan still runs on the tolerated existing session.
	fe.CommandScript = append(fe.CommandScript, okCmd())
	a := &Attacher{exec: fe, resolve: func(_ context.Context, _ Connector) (string, error) { return "/dev/sdy", nil }}

	if _, err := a.Attach(context.Background(), Connector{TargetIQN: "iqn.x", Portal: "10.0.0.5:3260", Lun: 0, ChapUser: "u", ChapSecret: "s"}); err != nil {
		t.Fatalf("Attach should tolerate an existing session, got %v", err)
	}
}

func TestAttachLoginErrorFails(t *testing.T) {
	fe := &testingexec.FakeExec{}
	for i := 0; i < 4; i++ {
		fe.CommandScript = append(fe.CommandScript, okCmd())
	}
	fe.CommandScript = append(fe.CommandScript, errCmd("iscsiadm: cannot make connection to 10.0.0.5", fmt.Errorf("exit status 8")))
	a := &Attacher{exec: fe, resolve: func(_ context.Context, _ Connector) (string, error) { return "/dev/sdy", nil }}

	if _, err := a.Attach(context.Background(), Connector{TargetIQN: "iqn.x", Portal: "10.0.0.5:3260", ChapUser: "u", ChapSecret: "s"}); err == nil {
		t.Fatal("Attach must fail on a real login error")
	}
}

func TestDetachIssuesLogoutThenDelete(t *testing.T) {
	fe, calls := recExec(2)
	a := &Attacher{exec: fe}
	if err := a.Detach(context.Background(), "iqn.x", "10.0.0.5"); err != nil {
		t.Fatalf("Detach: %v", err)
	}
	wantCmd(t, *calls, 0, "iscsiadm -m node -T iqn.x -p 10.0.0.5:3260 --logout")
	wantCmd(t, *calls, 1, "iscsiadm -m node -T iqn.x -p 10.0.0.5:3260 -o delete")
}

func TestDetachToleratesMissingSession(t *testing.T) {
	fe := &testingexec.FakeExec{CommandScript: []testingexec.FakeCommandAction{
		errCmd("iscsiadm: No matching sessions found", fmt.Errorf("exit status 21")),
		errCmd("iscsiadm: No matching node records found", fmt.Errorf("exit status 21")),
	}}
	a := &Attacher{exec: fe}
	if err := a.Detach(context.Background(), "iqn.x", "10.0.0.5"); err != nil {
		t.Fatalf("Detach should tolerate a missing session, got %v", err)
	}
}

func TestDevicePath(t *testing.T) {
	c := Connector{TargetIQN: "iqn.x", Portal: "10.0.0.5", Lun: 3}
	if got := c.devicePath(); got != "/dev/disk/by-path/ip-10.0.0.5:3260-iscsi-iqn.x-lun-3" {
		t.Errorf("devicePath = %q", got)
	}
}

func TestNormalizePortal(t *testing.T) {
	for in, want := range map[string]string{
		"10.0.0.5":      "10.0.0.5:3260",
		"10.0.0.5:3261": "10.0.0.5:3261",
		"":              "",
	} {
		if got := normalizePortal(in); got != want {
			t.Errorf("normalizePortal(%q) = %q, want %q", in, got, want)
		}
	}
}
