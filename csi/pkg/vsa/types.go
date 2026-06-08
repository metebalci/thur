// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

// The request/response types below mirror the daemon's wire structs exactly
// (field names = serde names). Sources of truth:
//   - vsa/daemon/src/admin/handlers.rs  (CreateVolumeRequest, VolumeRow,
//     ResizeVolumeRequest, CloneVolumeRequest)
//   - vsa/daemon/src/admin/snapshots.rs (CreateSnapshotRequest + responses)
//   - shared/admin-iscsi/src/lib.rs      (AddRequest, GrantRequest,
//     RevokeRequest, NameOnlyRequest, UserRow)
// They are also documented in docs/reference/openapi-admin.yaml, guarded by
// vsa/daemon/tests/admin_openapi_sync.rs.

// CreateVolumeRequest is the POST /api/v1/volumes body. Optional fields are
// omitted on the wire so the daemon's serde defaults apply.
type CreateVolumeRequest struct {
	Name          string  `json:"name"`
	SizeBytes     uint64  `json:"size_bytes"`
	PageSizeBytes uint32  `json:"page_size_bytes,omitempty"`
	Backend       string  `json:"backend,omitempty"`
	Dedup         string  `json:"dedup,omitempty"`
	Worm          bool    `json:"worm,omitempty"`
	Encrypt       bool    `json:"encrypt,omitempty"`
	KeyHex        string  `json:"key_hex,omitempty"`
	Keystore      string  `json:"keystore,omitempty"`
	DekSource     string  `json:"dek_source,omitempty"`
	SyncAfter     string  `json:"sync_after,omitempty"`
	Lun           *uint64 `json:"lun,omitempty"`
}

// VolumeRow is the create + clone + list response row.
type VolumeRow struct {
	Lun           uint64 `json:"lun"`
	Name          string `json:"name"`
	SizeBytes     uint64 `json:"size_bytes"`
	SectorBytes   uint32 `json:"sector_bytes"`
	PageSizeBytes uint32 `json:"page_size_bytes"`
	Backend       string `json:"backend"`
	DedupScope    string `json:"dedup_scope"`
	Worm          bool   `json:"worm"`
	UUID          string `json:"uuid"`
}

// ResizeRequest is the POST .../resize body. The driver only ever grows, so it
// sets SizeBytes and omits ShrinkToFit (the daemon requires exactly one).
type ResizeRequest struct {
	SizeBytes   uint64 `json:"size_bytes,omitempty"`
	ShrinkToFit bool   `json:"shrink_to_fit,omitempty"`
}

// ResizeResponse is the .../resize response.
type ResizeResponse struct {
	Volume    string `json:"volume"`
	Previous  uint64 `json:"previous"`
	SizeBytes uint64 `json:"size_bytes"`
}

// CloneVolumeRequest is the POST .../clone body.
type CloneVolumeRequest struct {
	NewName      string  `json:"new_name"`
	FromSnapshot string  `json:"from_snapshot,omitempty"`
	Lun          *uint64 `json:"lun,omitempty"`
}

// CreateSnapshotRequest is the POST .../snapshots body.
type CreateSnapshotRequest struct {
	Snapshot string `json:"snapshot"`
}

// SnapshotRow is a snapshot create/list response row. The create response also
// carries Volume; list rows omit it (it is on the list envelope).
type SnapshotRow struct {
	Volume    string `json:"volume,omitempty"`
	Snapshot  string `json:"snapshot"`
	SizeBytes uint64 `json:"size_bytes"`
	CreatedAt string `json:"created_at"`
}

// AddUserRequest is the POST /api/v1/iscsi/users body. Volumes must be
// non-empty (the daemon rejects an empty admission set).
type AddUserRequest struct {
	Username   string   `json:"username"`
	Password   string   `json:"password"`
	MutualChap bool     `json:"mutual_chap,omitempty"`
	Volumes    []string `json:"volumes,omitempty"`
}

// GrantRequest is the grant and revoke body (the daemon uses one shape for
// both: {name, volumes}).
type GrantRequest struct {
	Name    string   `json:"name"`
	Volumes []string `json:"volumes"`
}

// NameOnlyRequest is the remove/disable/enable body.
type NameOnlyRequest struct {
	Name string `json:"name"`
}

// UserRow is the iscsi-user response row (subset the driver needs).
type UserRow struct {
	Username   string   `json:"username"`
	MutualChap bool     `json:"mutual_chap"`
	Volumes    []string `json:"volumes,omitempty"`
	Disabled   bool     `json:"disabled"`
}
