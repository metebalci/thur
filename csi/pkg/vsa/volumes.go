// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

import (
	"context"
	"net/http"
	"net/url"
)

// CreateVolume provisions a volume and returns its row (incl. the immutable
// uuid and assigned lun). A duplicate name returns an *APIError with IsConflict.
func (c *Client) CreateVolume(ctx context.Context, req CreateVolumeRequest) (*VolumeRow, error) {
	var row VolumeRow
	if err := c.do(ctx, http.MethodPost, "/api/v1/volumes", req, &row); err != nil {
		return nil, err
	}
	return &row, nil
}

// ListVolumes returns every volume's row.
func (c *Client) ListVolumes(ctx context.Context) ([]VolumeRow, error) {
	var resp struct {
		Volumes []VolumeRow `json:"volumes"`
	}
	if err := c.do(ctx, http.MethodGet, "/api/v1/volumes", nil, &resp); err != nil {
		return nil, err
	}
	return resp.Volumes, nil
}

// GetVolumeByName returns the named volume, or (nil, nil) if it does not exist.
// It hits the daemon's per-name row endpoint — a constant-time registry lookup
// — instead of listing every volume and scanning client-side (issue #297).
func (c *Client) GetVolumeByName(ctx context.Context, name string) (*VolumeRow, error) {
	var row VolumeRow
	if err := c.do(ctx, http.MethodGet,
		"/api/v1/volumes/"+url.PathEscape(name)+"/row", nil, &row); err != nil {
		if IsNotFound(err) {
			return nil, nil // absent, not an error — the documented contract
		}
		return nil, err
	}
	return &row, nil
}

// DeleteVolume destroys a volume. A missing volume returns an *APIError with
// IsNotFound, which callers treat as success (idempotent delete).
func (c *Client) DeleteVolume(ctx context.Context, name string) error {
	return c.do(ctx, http.MethodDelete, "/api/v1/volumes/"+url.PathEscape(name), nil, nil)
}

// ResizeVolume grows a volume to sizeBytes (grow-only; the driver never
// shrinks). Returns the previous and new size.
func (c *Client) ResizeVolume(ctx context.Context, name string, sizeBytes uint64) (*ResizeResponse, error) {
	var resp ResizeResponse
	if err := c.do(ctx, http.MethodPost,
		"/api/v1/volumes/"+url.PathEscape(name)+"/resize",
		ResizeRequest{SizeBytes: sizeBytes}, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}
