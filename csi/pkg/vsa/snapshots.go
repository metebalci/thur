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

// FindSnapshot locates a snapshot by name across all volumes, returning the
// owning volume and its row, or ("", nil, nil) if no volume carries a snapshot
// with that name. One round trip: the daemon does the cross-volume scan, which
// replaces a client-side ListVolumes + per-volume ListSnapshots fan-out
// (issue #294).
func (c *Client) FindSnapshot(ctx context.Context, snapshot string) (string, *SnapshotRow, error) {
	var row SnapshotRow
	if err := c.do(ctx, http.MethodGet,
		"/api/v1/snapshots?name="+url.QueryEscape(snapshot), nil, &row); err != nil {
		if IsNotFound(err) {
			return "", nil, nil // absent across all volumes
		}
		return "", nil, err
	}
	return row.Volume, &row, nil
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
