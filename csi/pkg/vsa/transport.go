// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package vsa is a transport-agnostic Go client for the thurvsad admin API.
//
// The driver talks to the daemon over its admin Unix socket today. Every
// request/response type and the do() helper are transport-neutral: targeting a
// future TCP network admin API is a matter of swapping the dialer here and
// adding an auth header in do() — no call-site changes.
package vsa

import (
	"context"
	"net"
	"net/http"
	"time"
)

// baseURL host is ignored by the daemon (it only requires a Host header);
// the dialer routes every request to the socket regardless of host.
const baseURL = "http://thurvsa"

// newUnixHTTPClient returns an *http.Client whose connections are dialed to the
// daemon's admin Unix socket, regardless of the request URL's host.
func newUnixHTTPClient(socketPath string) *http.Client {
	return &http.Client{
		Timeout: 30 * time.Second,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", socketPath)
			},
		},
	}
}
