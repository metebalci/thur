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

// Per-volume CHAP isolation: ControllerPublishVolume mints one CHAP user per
// volume, admitted only to that volume, so a node logging in with those creds
// sees only its own LUN. The username is derived deterministically from the
// volume name; only the secret is random, and it is persisted (see chapStore)
// so a retried publish returns the identical value the daemon was configured
// with.

// chapCreds is a volume's CHAP login pair.
type chapCreds struct {
	username string
	secret   string
}

// chapUsername derives the CHAP username for a volume. Deterministic so every
// controller op (publish, unpublish, delete-cleanup) addresses the same user
// without a lookup. The volume name is already unique and <=64 bytes; the
// "csi-" prefix namespaces it away from operator-created users. The daemon
// caps usernames at 256 bytes, easily satisfied.
func chapUsername(volume string) string {
	return "csi-" + volume
}

// chapSecretName derives the name of the Kubernetes Secret that stores a
// volume's CHAP credentials. Volume names may contain uppercase and '_', which
// are invalid in a DNS-1123 Secret name, so the volume is hashed to a stable
// lowercase-hex token.
func chapSecretName(volume string) string {
	sum := sha256.Sum256([]byte(volume))
	return "thurvsa-chap-" + hex.EncodeToString(sum[:8])
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

// chapStore persists per-volume CHAP secrets so a publish is idempotent: a
// retry (attacher re-drive, controller restart) must return the exact secret
// the daemon's CHAP user was created with, or the node's login desyncs.
type chapStore interface {
	// ensure returns the volume's CHAP credentials, minting and persisting a
	// fresh secret on first call and returning the stored one thereafter.
	ensure(ctx context.Context, volume string) (chapCreds, error)
	// remove deletes the volume's stored credentials. Idempotent: a missing
	// entry is a no-op success.
	remove(ctx context.Context, volume string) error
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

func (m *memoryChapStore) ensure(_ context.Context, volume string) (chapCreds, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if c, ok := m.creds[volume]; ok {
		return c, nil
	}
	secret, err := mintSecret()
	if err != nil {
		return chapCreds{}, err
	}
	c := chapCreds{username: chapUsername(volume), secret: secret}
	m.creds[volume] = c
	return c, nil
}

func (m *memoryChapStore) remove(_ context.Context, volume string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.creds, volume)
	return nil
}
