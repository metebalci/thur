// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package fake is an in-memory stand-in for thurvsad's admin socket,
// implementing the subset of routes the CSI driver uses. Test-only: it mirrors
// the real daemon's status codes (409 on duplicate, 404 on missing, 400 on
// invalid CHAP) so client and controller tests can run without a real daemon.
package fake

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/metebalci/thur/csi/pkg/vsa"
)

// Daemon is the fake admin server and its in-memory state.
type Daemon struct {
	mu        sync.Mutex
	volumes   map[string]*vsa.VolumeRow            // by name
	snapshots map[string]map[string]vsa.SnapshotRow // volume -> snapshot -> row
	users     map[string]*vsa.UserRow             // by username
	nextLUN   uint64

	server *http.Server
}

// StartUnix starts the fake listening on socketPath. Call Close to stop it.
func StartUnix(socketPath string) (*Daemon, error) {
	d := &Daemon{
		volumes:   map[string]*vsa.VolumeRow{},
		snapshots: map[string]map[string]vsa.SnapshotRow{},
		users:     map[string]*vsa.UserRow{},
	}
	lis, err := net.Listen("unix", socketPath)
	if err != nil {
		return nil, err
	}
	d.server = &http.Server{Handler: d.routes()}
	go func() { _ = d.server.Serve(lis) }()
	return d, nil
}

// Close stops the fake server.
func (d *Daemon) Close() error { return d.server.Close() }

// VolumeCount returns the number of live volumes (test assertions).
func (d *Daemon) VolumeCount() int {
	d.mu.Lock()
	defer d.mu.Unlock()
	return len(d.volumes)
}

// UserCount returns the number of CHAP users (test assertions).
func (d *Daemon) UserCount() int {
	d.mu.Lock()
	defer d.mu.Unlock()
	return len(d.users)
}

func (d *Daemon) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/health", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"status": "ok"})
	})
	mux.HandleFunc("GET /api/v1/volumes", d.listVolumes)
	mux.HandleFunc("POST /api/v1/volumes", d.createVolume)
	mux.HandleFunc("DELETE /api/v1/volumes/{name}", d.deleteVolume)
	mux.HandleFunc("POST /api/v1/volumes/{name}/resize", d.resizeVolume)
	mux.HandleFunc("POST /api/v1/volumes/{name}/clone", d.cloneVolume)
	mux.HandleFunc("GET /api/v1/volumes/{name}/snapshots", d.listSnapshots)
	mux.HandleFunc("POST /api/v1/volumes/{name}/snapshots", d.createSnapshot)
	mux.HandleFunc("DELETE /api/v1/volumes/{name}/snapshots/{snap}", d.deleteSnapshot)
	mux.HandleFunc("POST /api/v1/iscsi/users", d.addUser)
	mux.HandleFunc("POST /api/v1/iscsi/users/grant", d.grantUser)
	mux.HandleFunc("POST /api/v1/iscsi/users/revoke", d.revokeUser)
	mux.HandleFunc("POST /api/v1/iscsi/users/remove", d.removeUser)
	return mux
}

// ---- volumes ----

func (d *Daemon) createVolume(w http.ResponseWriter, r *http.Request) {
	var req vsa.CreateVolumeRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.volumes[req.Name]; ok {
		writeErr(w, http.StatusConflict, fmt.Sprintf("volume '%s' already exists", req.Name))
		return
	}
	page := req.PageSizeBytes
	if page == 0 {
		page = 65536
	}
	dedup := req.Dedup
	if dedup == "" {
		dedup = "local"
	}
	row := &vsa.VolumeRow{
		Lun:           d.nextLUN,
		Name:          req.Name,
		SizeBytes:     req.SizeBytes,
		SectorBytes:   4096,
		PageSizeBytes: page,
		Backend:       orDefault(req.Backend, "primary"),
		DedupScope:    dedup,
		Worm:          req.Worm,
		UUID:          fakeUUID(req.Name),
	}
	d.nextLUN++
	d.volumes[req.Name] = row
	writeJSON(w, http.StatusCreated, row)
}

func (d *Daemon) listVolumes(w http.ResponseWriter, _ *http.Request) {
	d.mu.Lock()
	defer d.mu.Unlock()
	rows := make([]vsa.VolumeRow, 0, len(d.volumes))
	for _, v := range d.volumes {
		rows = append(rows, *v)
	}
	writeJSON(w, http.StatusOK, map[string]any{"volumes": rows})
}

func (d *Daemon) deleteVolume(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.volumes[name]; !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("volume '%s' not found", name))
		return
	}
	delete(d.volumes, name)
	delete(d.snapshots, name)
	writeJSON(w, http.StatusOK, map[string]any{"volume": name})
}

func (d *Daemon) resizeVolume(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	var req vsa.ResizeRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	v, ok := d.volumes[name]
	if !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("volume '%s' is not registered", name))
		return
	}
	prev := v.SizeBytes
	v.SizeBytes = req.SizeBytes
	writeJSON(w, http.StatusOK, vsa.ResizeResponse{Volume: name, Previous: prev, SizeBytes: req.SizeBytes})
}

func (d *Daemon) cloneVolume(w http.ResponseWriter, r *http.Request) {
	src := r.PathValue("name")
	var req vsa.CloneVolumeRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	source, ok := d.volumes[src]
	if !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("source volume '%s' not found", src))
		return
	}
	if _, ok := d.volumes[req.NewName]; ok {
		writeErr(w, http.StatusConflict, fmt.Sprintf("volume '%s' already exists", req.NewName))
		return
	}
	row := &vsa.VolumeRow{
		Lun:           d.nextLUN,
		Name:          req.NewName,
		SizeBytes:     source.SizeBytes,
		SectorBytes:   source.SectorBytes,
		PageSizeBytes: source.PageSizeBytes,
		Backend:       source.Backend,
		DedupScope:    source.DedupScope,
		UUID:          fakeUUID(req.NewName),
	}
	d.nextLUN++
	d.volumes[req.NewName] = row
	writeJSON(w, http.StatusCreated, row)
}

// ---- snapshots ----

func (d *Daemon) createSnapshot(w http.ResponseWriter, r *http.Request) {
	vol := r.PathValue("name")
	var req vsa.CreateSnapshotRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	v, ok := d.volumes[vol]
	if !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("volume '%s' is not registered", vol))
		return
	}
	if d.snapshots[vol] == nil {
		d.snapshots[vol] = map[string]vsa.SnapshotRow{}
	}
	if _, ok := d.snapshots[vol][req.Snapshot]; ok {
		writeErr(w, http.StatusConflict, fmt.Sprintf("snapshot '%s' already exists for volume '%s'", req.Snapshot, vol))
		return
	}
	row := vsa.SnapshotRow{
		Volume:    vol,
		Snapshot:  req.Snapshot,
		SizeBytes: v.SizeBytes,
		CreatedAt: time.Now().UTC().Format(time.RFC3339),
	}
	d.snapshots[vol][req.Snapshot] = row
	writeJSON(w, http.StatusCreated, row)
}

func (d *Daemon) listSnapshots(w http.ResponseWriter, r *http.Request) {
	vol := r.PathValue("name")
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.volumes[vol]; !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("volume '%s' not found", vol))
		return
	}
	rows := make([]vsa.SnapshotRow, 0, len(d.snapshots[vol]))
	for _, s := range d.snapshots[vol] {
		rows = append(rows, vsa.SnapshotRow{Snapshot: s.Snapshot, SizeBytes: s.SizeBytes, CreatedAt: s.CreatedAt})
	}
	writeJSON(w, http.StatusOK, map[string]any{"volume": vol, "snapshots": rows})
}

func (d *Daemon) deleteSnapshot(w http.ResponseWriter, r *http.Request) {
	vol, snap := r.PathValue("name"), r.PathValue("snap")
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.snapshots[vol][snap]; !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("snapshot '%s' not found for volume '%s'", snap, vol))
		return
	}
	delete(d.snapshots[vol], snap)
	writeJSON(w, http.StatusOK, map[string]any{"volume": vol, "snapshot": snap})
}

// ---- iscsi users ----

func (d *Daemon) addUser(w http.ResponseWriter, r *http.Request) {
	var req vsa.AddUserRequest
	if !decode(w, r, &req) {
		return
	}
	if len(req.Password) < 12 {
		writeErr(w, http.StatusBadRequest, "password must be at least 12 bytes")
		return
	}
	if len(req.Volumes) == 0 {
		writeErr(w, http.StatusBadRequest, "user must be admitted to at least one volume")
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.users[req.Username]; ok {
		writeErr(w, http.StatusConflict, fmt.Sprintf("user '%s' already exists", req.Username))
		return
	}
	row := &vsa.UserRow{Username: req.Username, MutualChap: req.MutualChap, Volumes: append([]string(nil), req.Volumes...)}
	d.users[req.Username] = row
	writeJSON(w, http.StatusCreated, row)
}

func (d *Daemon) grantUser(w http.ResponseWriter, r *http.Request) {
	var req vsa.GrantRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	u, ok := d.users[req.Name]
	if !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("user '%s' not found", req.Name))
		return
	}
	u.Volumes = union(u.Volumes, req.Volumes)
	writeJSON(w, http.StatusOK, u)
}

func (d *Daemon) revokeUser(w http.ResponseWriter, r *http.Request) {
	var req vsa.GrantRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	u, ok := d.users[req.Name]
	if !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("user '%s' not found", req.Name))
		return
	}
	remaining := difference(u.Volumes, req.Volumes)
	if len(remaining) == 0 {
		writeErr(w, http.StatusConflict, "revoke would empty the admission set; use remove instead")
		return
	}
	u.Volumes = remaining
	writeJSON(w, http.StatusOK, u)
}

func (d *Daemon) removeUser(w http.ResponseWriter, r *http.Request) {
	var req vsa.NameOnlyRequest
	if !decode(w, r, &req) {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	if _, ok := d.users[req.Name]; !ok {
		writeErr(w, http.StatusNotFound, fmt.Sprintf("user '%s' not found", req.Name))
		return
	}
	delete(d.users, req.Name)
	writeJSON(w, http.StatusOK, map[string]any{"removed": req.Name})
}

// ---- helpers ----

func decode(w http.ResponseWriter, r *http.Request, v any) bool {
	if err := json.NewDecoder(r.Body).Decode(v); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid request body: "+err.Error())
		return false
	}
	return true
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]any{"error": msg})
}

func fakeUUID(name string) string {
	sum := sha256.Sum256([]byte(name))
	return hex.EncodeToString(sum[:16])
}

func orDefault(v, def string) string {
	if v == "" {
		return def
	}
	return v
}

func union(a, b []string) []string {
	seen := map[string]bool{}
	out := make([]string, 0, len(a)+len(b))
	for _, x := range append(append([]string(nil), a...), b...) {
		if !seen[x] {
			seen[x] = true
			out = append(out, x)
		}
	}
	return out
}

func difference(a, remove []string) []string {
	drop := map[string]bool{}
	for _, x := range remove {
		drop[x] = true
	}
	out := make([]string, 0, len(a))
	for _, x := range a {
		if !drop[x] {
			out = append(out, x)
		}
	}
	return out
}
