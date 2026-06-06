// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

import (
	"context"
	"net/http"
)

// AddUser creates a CHAP user admitted to req.Volumes. A duplicate username
// returns IsConflict; a password under 12 bytes or an empty volume set returns
// IsBadRequest.
func (c *Client) AddUser(ctx context.Context, req AddUserRequest) (*UserRow, error) {
	var row UserRow
	if err := c.do(ctx, http.MethodPost, "/api/v1/iscsi/users", req, &row); err != nil {
		return nil, err
	}
	return &row, nil
}

// GrantUser unions volumes into a user's admission set (idempotent).
func (c *Client) GrantUser(ctx context.Context, name string, volumes []string) (*UserRow, error) {
	var row UserRow
	if err := c.do(ctx, http.MethodPost, "/api/v1/iscsi/users/grant",
		GrantRequest{Name: name, Volumes: volumes}, &row); err != nil {
		return nil, err
	}
	return &row, nil
}

// RevokeUser removes volumes from a user's admission set. The daemon refuses to
// empty the set (returns IsConflict); use RemoveUser for the terminal case.
func (c *Client) RevokeUser(ctx context.Context, name string, volumes []string) (*UserRow, error) {
	var row UserRow
	if err := c.do(ctx, http.MethodPost, "/api/v1/iscsi/users/revoke",
		GrantRequest{Name: name, Volumes: volumes}, &row); err != nil {
		return nil, err
	}
	return &row, nil
}

// RemoveUser deletes a CHAP user entirely. A missing user returns IsNotFound,
// which callers treat as success.
func (c *Client) RemoveUser(ctx context.Context, name string) error {
	return c.do(ctx, http.MethodPost, "/api/v1/iscsi/users/remove",
		NameOnlyRequest{Name: name}, nil)
}
