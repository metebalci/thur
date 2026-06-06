// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package driver

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
)

const maxVolumeNameLen = 64

// volumeName maps a CSI-provided name to a valid VSA volume name (1-64 chars,
// [A-Za-z0-9_-]). The external provisioner passes pvc-<uid>, which is already
// valid and returned unchanged. Anything else is sanitized deterministically:
// invalid bytes become '-', truncated to fit, with an 8-hex suffix of the
// original to keep distinct inputs distinct.
//
// The returned name is also the CSI VolumeId: it is unique (derived from the
// immutable PVC UID) and stable for the volume's lifetime, and the admin API is
// name-keyed, so every later controller op addresses the daemon directly.
func volumeName(csiName string) (string, error) {
	if csiName == "" {
		return "", fmt.Errorf("empty volume name")
	}
	if isValidVolumeName(csiName) {
		return csiName, nil
	}
	b := []byte(csiName)
	for i, c := range b {
		if !isNameByte(c) {
			b[i] = '-'
		}
	}
	sum := sha256.Sum256([]byte(csiName))
	suffix := "-" + hex.EncodeToString(sum[:4])
	san := string(b)
	if len(san)+len(suffix) > maxVolumeNameLen {
		san = san[:maxVolumeNameLen-len(suffix)]
	}
	return san + suffix, nil
}

func isValidVolumeName(s string) bool {
	if len(s) == 0 || len(s) > maxVolumeNameLen {
		return false
	}
	for i := 0; i < len(s); i++ {
		if !isNameByte(s[i]) {
			return false
		}
	}
	return true
}

func isNameByte(c byte) bool {
	return (c >= 'A' && c <= 'Z') ||
		(c >= 'a' && c <= 'z') ||
		(c >= '0' && c <= '9') ||
		c == '-' || c == '_'
}
