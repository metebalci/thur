// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// Client talks to a thurvsad admin socket over HTTP/JSON.
type Client struct {
	http    *http.Client
	baseURL string
}

// NewUnixClient builds a Client bound to the daemon's admin Unix socket.
func NewUnixClient(socketPath string) *Client {
	return &Client{http: newUnixHTTPClient(socketPath), baseURL: baseURL}
}

// do performs an HTTP request with an optional JSON body and decodes a JSON
// response into out (when non-nil). Non-2xx responses become *APIError.
func (c *Client) do(ctx context.Context, method, path string, body, out any) error {
	var rdr io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("marshal request: %w", err)
		}
		rdr = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, rdr)
	if err != nil {
		return fmt.Errorf("build request: %w", err)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, path, err)
	}
	defer resp.Body.Close()

	data, _ := io.ReadAll(resp.Body)
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return parseAPIError(resp.StatusCode, data)
	}
	if out != nil && len(data) > 0 {
		if err := json.Unmarshal(data, out); err != nil {
			return fmt.Errorf("decode response: %w", err)
		}
	}
	return nil
}

// Health pings the daemon's health endpoint.
func (c *Client) Health(ctx context.Context) error {
	return c.do(ctx, http.MethodGet, "/api/v1/health", nil, nil)
}
