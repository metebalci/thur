// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"context"
	"fmt"
	"os"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"
)

const (
	chapSecretManagedByLabel = "app.kubernetes.io/managed-by"
	chapSecretManagedByValue = "thurvsa-csi"
	chapSecretNodeAnno       = DefaultDriverName + "/node"
	chapSecretUsernameKey    = "username"
	chapSecretSecretKey      = "secret"
)

// k8sChapStore persists per-volume CHAP credentials in Kubernetes Secrets in
// the driver's own namespace. Durable across controller restarts, which is what
// makes a retried publish return the identical secret the daemon was set with.
type k8sChapStore struct {
	client    kubernetes.Interface
	namespace string
}

func newK8sChapStore(client kubernetes.Interface, namespace string) *k8sChapStore {
	return &k8sChapStore{client: client, namespace: namespace}
}

func (k *k8sChapStore) secrets() typedSecrets {
	return k.client.CoreV1().Secrets(k.namespace)
}

func (k *k8sChapStore) ensure(ctx context.Context, nodeID string) (chapCreds, error) {
	name := chapSecretName(nodeID)
	existing, err := k.secrets().Get(ctx, name, metav1.GetOptions{})
	if err == nil {
		return credsFromSecret(existing)
	}
	if !apierrors.IsNotFound(err) {
		return chapCreds{}, err
	}

	secret, err := mintSecret()
	if err != nil {
		return chapCreds{}, err
	}
	creds := chapCreds{username: chapUsername(nodeID), secret: secret}
	obj := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:        name,
			Labels:      map[string]string{chapSecretManagedByLabel: chapSecretManagedByValue},
			Annotations: map[string]string{chapSecretNodeAnno: nodeID},
		},
		Type: corev1.SecretTypeOpaque,
		Data: map[string][]byte{
			chapSecretUsernameKey: []byte(creds.username),
			chapSecretSecretKey:   []byte(creds.secret),
		},
	}
	created, err := k.secrets().Create(ctx, obj, metav1.CreateOptions{})
	if err == nil {
		return credsFromSecret(created)
	}
	// A concurrent publish won the create race: re-read the winner's secret so
	// both callers return the same value.
	if apierrors.IsAlreadyExists(err) {
		existing, gerr := k.secrets().Get(ctx, name, metav1.GetOptions{})
		if gerr != nil {
			return chapCreds{}, gerr
		}
		return credsFromSecret(existing)
	}
	return chapCreds{}, err
}

func (k *k8sChapStore) remove(ctx context.Context, nodeID string) error {
	err := k.secrets().Delete(ctx, chapSecretName(nodeID), metav1.DeleteOptions{})
	if err != nil && !apierrors.IsNotFound(err) {
		return err
	}
	return nil
}

// typedSecrets is the slice of the corev1 SecretInterface this store uses.
// Declaring it keeps the secrets() helper readable.
type typedSecrets interface {
	Get(ctx context.Context, name string, opts metav1.GetOptions) (*corev1.Secret, error)
	Create(ctx context.Context, secret *corev1.Secret, opts metav1.CreateOptions) (*corev1.Secret, error)
	Delete(ctx context.Context, name string, opts metav1.DeleteOptions) error
}

func credsFromSecret(s *corev1.Secret) (chapCreds, error) {
	user := string(s.Data[chapSecretUsernameKey])
	secret := string(s.Data[chapSecretSecretKey])
	if user == "" || secret == "" {
		return chapCreds{}, fmt.Errorf("chap secret %q is missing %q/%q data", s.Name, chapSecretUsernameKey, chapSecretSecretKey)
	}
	return chapCreds{username: user, secret: secret}, nil
}

// buildChapStore selects the CHAP secret store for the running mode. The
// "memory" store is for csi-sanity and non-cluster runs; "kubernetes" (the
// default) requires an in-cluster service account.
func buildChapStore(cfg Config) (chapStore, error) {
	switch cfg.ChapStoreKind {
	case "memory":
		return newMemoryChapStore(), nil
	case "", "kubernetes":
		client, ns, err := inClusterSecrets(cfg.SecretNamespace)
		if err != nil {
			return nil, err
		}
		return newK8sChapStore(client, ns), nil
	default:
		return nil, fmt.Errorf("unknown chap-secret-store %q (want kubernetes or memory)", cfg.ChapStoreKind)
	}
}

// inClusterSecrets builds a clientset from the pod's service account and
// resolves the namespace to store CHAP secrets in (the flag wins, else the
// service-account namespace).
func inClusterSecrets(namespace string) (kubernetes.Interface, string, error) {
	rc, err := rest.InClusterConfig()
	if err != nil {
		return nil, "", fmt.Errorf("in-cluster config (controller must run in a pod, or pass --chap-secret-store=memory): %w", err)
	}
	client, err := kubernetes.NewForConfig(rc)
	if err != nil {
		return nil, "", err
	}
	if namespace == "" {
		namespace = serviceAccountNamespace()
	}
	if namespace == "" {
		return nil, "", fmt.Errorf("could not resolve CHAP secret namespace: set --secret-namespace or POD_NAMESPACE")
	}
	return client, namespace, nil
}

func serviceAccountNamespace() string {
	b, err := os.ReadFile("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
	if err != nil {
		return ""
	}
	return string(b)
}
