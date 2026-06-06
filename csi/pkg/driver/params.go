// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"fmt"
	"strconv"
	"strings"
)

// storageClassParams are the recognized StorageClass parameters.
type storageClassParams struct {
	backend    string
	pageSize   uint32
	dedupScope string
	worm       bool
	fsType     string
	encrypt    bool
	keystore   string
	dekSource  string
	syncAfter  string
}

func parseParams(p map[string]string) (storageClassParams, error) {
	var s storageClassParams
	s.backend = p["backend"]
	s.dedupScope = p["dedupScope"]
	s.fsType = p["fsType"]
	s.keystore = p["keystore"]
	s.dekSource = p["dekSource"]
	s.syncAfter = p["syncAfter"]

	if v := p["pageSize"]; v != "" {
		n, err := parseBytes(v)
		if err != nil {
			return s, fmt.Errorf("invalid pageSize %q: %w", v, err)
		}
		s.pageSize = uint32(n)
	}
	if v := p["worm"]; v != "" {
		b, err := strconv.ParseBool(v)
		if err != nil {
			return s, fmt.Errorf("invalid worm %q: %w", v, err)
		}
		s.worm = b
	}
	if v := p["encrypt"]; v != "" {
		b, err := strconv.ParseBool(v)
		if err != nil {
			return s, fmt.Errorf("invalid encrypt %q: %w", v, err)
		}
		s.encrypt = b
	}
	return s, nil
}

// parseBytes parses an integer with an optional binary unit suffix
// (Ki/Mi/Gi/Ti or K/M/G/T) into bytes.
func parseBytes(s string) (uint64, error) {
	s = strings.TrimSpace(s)
	mult := uint64(1)
	for _, u := range []struct {
		suf string
		m   uint64
	}{
		{"Ti", 1 << 40}, {"Gi", 1 << 30}, {"Mi", 1 << 20}, {"Ki", 1 << 10},
		{"T", 1 << 40}, {"G", 1 << 30}, {"M", 1 << 20}, {"K", 1 << 10},
	} {
		if strings.HasSuffix(s, u.suf) {
			mult = u.m
			s = strings.TrimSuffix(s, u.suf)
			break
		}
	}
	n, err := strconv.ParseUint(strings.TrimSpace(s), 10, 64)
	if err != nil {
		return 0, err
	}
	return n * mult, nil
}
