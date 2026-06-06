// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

package vsa

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
)

// APIError is a non-2xx response from the admin API. The daemon's error bodies
// are JSON objects shaped {"error": "..."}.
type APIError struct {
	Status  int
	Message string
}

func (e *APIError) Error() string {
	if e.Message != "" {
		return fmt.Sprintf("admin API %d: %s", e.Status, e.Message)
	}
	return fmt.Sprintf("admin API %d", e.Status)
}

func parseAPIError(status int, body []byte) *APIError {
	var env struct {
		Error string `json:"error"`
	}
	_ = json.Unmarshal(body, &env)
	return &APIError{Status: status, Message: env.Error}
}

// IsNotFound reports whether err is a 404 from the admin API.
func IsNotFound(err error) bool {
	var e *APIError
	return errors.As(err, &e) && e.Status == http.StatusNotFound
}

// IsConflict reports whether err is a 409 from the admin API.
func IsConflict(err error) bool {
	var e *APIError
	return errors.As(err, &e) && e.Status == http.StatusConflict
}

// IsBadRequest reports whether err is a 400 from the admin API.
func IsBadRequest(err error) bool {
	var e *APIError
	return errors.As(err, &e) && e.Status == http.StatusBadRequest
}
