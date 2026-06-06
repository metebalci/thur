// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

import (
	"context"
	"net/http"
	"net/url"
)

// CreateSnapshot freezes a point-in-time snapshot of a volume. A duplicate
// snapshot name returns an *APIError with IsConflict.
func (c *Client) CreateSnapshot(ctx context.Context, volume, snapshot string) (*SnapshotRow, error) {
	var row SnapshotRow
	if err := c.do(ctx, http.MethodPost,
		"/api/v1/volumes/"+url.PathEscape(volume)+"/snapshots",
		CreateSnapshotRequest{Snapshot: snapshot}, &row); err != nil {
		return nil, err
	}
	return &row, nil
}

// ListSnapshots returns a volume's snapshots.
func (c *Client) ListSnapshots(ctx context.Context, volume string) ([]SnapshotRow, error) {
	var resp struct {
		Snapshots []SnapshotRow `json:"snapshots"`
	}
	if err := c.do(ctx, http.MethodGet,
		"/api/v1/volumes/"+url.PathEscape(volume)+"/snapshots", nil, &resp); err != nil {
		return nil, err
	}
	return resp.Snapshots, nil
}

// DeleteSnapshot removes a snapshot. A missing snapshot returns IsNotFound,
// which callers treat as success.
func (c *Client) DeleteSnapshot(ctx context.Context, volume, snapshot string) error {
	return c.do(ctx, http.MethodDelete,
		"/api/v1/volumes/"+url.PathEscape(volume)+"/snapshots/"+url.PathEscape(snapshot), nil, nil)
}

// CloneVolume creates a new writable volume from a source volume, optionally
// seeded from one of its snapshots (req.FromSnapshot). Returns the clone's row.
func (c *Client) CloneVolume(ctx context.Context, source string, req CloneVolumeRequest) (*VolumeRow, error) {
	var row VolumeRow
	if err := c.do(ctx, http.MethodPost,
		"/api/v1/volumes/"+url.PathEscape(source)+"/clone", req, &row); err != nil {
		return nil, err
	}
	return &row, nil
}
