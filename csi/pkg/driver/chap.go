// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"sync"
)

// Per-node CHAP isolation: a node gets ONE CHAP user (csi-node-<nodeID>),
// admitted to every volume published to that node. iSCSI keys a session by
// (target IQN, portal) and all VSA volumes share one IQN, so a node has a
// single session that must see all its LUNs — which requires a single CHAP
// identity. ControllerPublishVolume grants the volume to the node's user; the
// node logs in once and rescans to pick up newly-granted LUNs. The isolation
// boundary is the node (node A can't authenticate as node B), which is the
// meaningful Kubernetes boundary. The secret is per-node, random, and persisted
// (see chapStore) so a retried publish returns the identical value.

// chapCreds is a node's CHAP login pair.
type chapCreds struct {
	username string
	secret   string
}

// chapUsername derives the CHAP username for a node. Deterministic so every
// controller op addresses the same user without a lookup. Node ids are
// normally short DNS names; the "csi-node-" prefix namespaces them away from
// operator-created users. Hashed only if it would exceed the daemon's 256-byte
// username cap.
func chapUsername(nodeID string) string {
	u := "csi-node-" + nodeID
	if len(u) <= 256 {
		return u
	}
	sum := sha256.Sum256([]byte(nodeID))
	return "csi-node-" + hex.EncodeToString(sum[:16])
}

// chapSecretName derives the name of the Kubernetes Secret that stores a node's
// CHAP credentials. The node id may contain characters invalid in a DNS-1123
// Secret name, so it is hashed to a stable lowercase-hex token.
func chapSecretName(nodeID string) string {
	sum := sha256.Sum256([]byte(nodeID))
	return "thurvsa-chap-node-" + hex.EncodeToString(sum[:8])
}

// mintSecret returns a fresh 32-byte CHAP secret as 64 hex chars (well over the
// daemon's 12-byte floor).
func mintSecret() (string, error) {
	var b [32]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(b[:]), nil
}

// chapStore persists per-node CHAP secrets so a publish is idempotent: a retry
// (attacher re-drive, controller restart) must return the exact secret the
// daemon's CHAP user was created with, or the node's login desyncs. Keyed by
// node id.
type chapStore interface {
	// ensure returns the node's CHAP credentials, minting and persisting a
	// fresh secret on first call and returning the stored one thereafter.
	ensure(ctx context.Context, nodeID string) (chapCreds, error)
	// remove deletes the node's stored credentials. Idempotent: a missing
	// entry is a no-op success.
	remove(ctx context.Context, nodeID string) error
}

// memoryChapStore keeps secrets in process memory. It is not durable across a
// controller restart, so it is for tests and non-cluster runs (csi-sanity)
// only; production uses the Kubernetes Secret-backed store.
type memoryChapStore struct {
	mu    sync.Mutex
	creds map[string]chapCreds
}

func newMemoryChapStore() *memoryChapStore {
	return &memoryChapStore{creds: map[string]chapCreds{}}
}

func (m *memoryChapStore) ensure(_ context.Context, nodeID string) (chapCreds, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if c, ok := m.creds[nodeID]; ok {
		return c, nil
	}
	secret, err := mintSecret()
	if err != nil {
		return chapCreds{}, err
	}
	c := chapCreds{username: chapUsername(nodeID), secret: secret}
	m.creds[nodeID] = c
	return c, nil
}

func (m *memoryChapStore) remove(_ context.Context, nodeID string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.creds, nodeID)
	return nil
}
