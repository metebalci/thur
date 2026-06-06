// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"testing"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"
)

func TestK8sChapStoreReuse(t *testing.T) {
	ctx := context.Background()
	client := fake.NewSimpleClientset()
	store := newK8sChapStore(client, "thurvsa-system")

	c1, err := store.ensure(ctx, "pvc-k8s")
	if err != nil {
		t.Fatalf("ensure: %v", err)
	}
	if c1.username != chapUsername("pvc-k8s") {
		t.Errorf("username = %q", c1.username)
	}
	if len(c1.secret) != 64 {
		t.Errorf("secret = %q (want 64 hex chars)", c1.secret)
	}

	// A second ensure must return the persisted secret, not a fresh one.
	c2, err := store.ensure(ctx, "pvc-k8s")
	if err != nil {
		t.Fatalf("second ensure: %v", err)
	}
	if c1 != c2 {
		t.Errorf("ensure not idempotent: %+v vs %+v", c1, c2)
	}

	// The backing Secret exists in the configured namespace.
	if _, err := client.CoreV1().Secrets("thurvsa-system").Get(ctx, chapSecretName("pvc-k8s"), metav1.GetOptions{}); err != nil {
		t.Fatalf("backing secret not found: %v", err)
	}

	if err := store.remove(ctx, "pvc-k8s"); err != nil {
		t.Fatalf("remove: %v", err)
	}
	if _, err := client.CoreV1().Secrets("thurvsa-system").Get(ctx, chapSecretName("pvc-k8s"), metav1.GetOptions{}); !apierrors.IsNotFound(err) {
		t.Fatalf("secret should be gone, got %v", err)
	}
	// Remove is idempotent.
	if err := store.remove(ctx, "pvc-k8s"); err != nil {
		t.Fatalf("second remove must be a no-op, got %v", err)
	}

	// A fresh ensure after removal mints a new secret.
	c3, err := store.ensure(ctx, "pvc-k8s")
	if err != nil {
		t.Fatalf("re-ensure: %v", err)
	}
	if c3.secret == c1.secret {
		t.Errorf("re-minted secret equals the old one")
	}
}
