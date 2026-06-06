// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Package grpcserver hosts the CSI gRPC server on a unix-socket endpoint.
package grpcserver

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/signal"
	"strings"
	"syscall"

	"google.golang.org/grpc"
	"k8s.io/klog/v2"
)

// Server wraps a *grpc.Server bound to a CSI endpoint.
type Server struct {
	endpoint string
	server   *grpc.Server
}

// New creates a Server for the given CSI endpoint (unix:// or tcp://).
func New(endpoint string) *Server {
	return &Server{
		endpoint: endpoint,
		server:   grpc.NewServer(grpc.UnaryInterceptor(logInterceptor)),
	}
}

// Server returns the underlying *grpc.Server for service registration.
func (s *Server) Server() *grpc.Server { return s.server }

// Serve binds the endpoint and serves until SIGTERM/SIGINT, then stops
// gracefully.
func (s *Server) Serve() error {
	network, addr, err := parseEndpoint(s.endpoint)
	if err != nil {
		return err
	}
	if network == "unix" {
		if err := os.Remove(addr); err != nil && !os.IsNotExist(err) {
			return fmt.Errorf("remove stale socket %q: %w", addr, err)
		}
	}
	lis, err := net.Listen(network, addr)
	if err != nil {
		return fmt.Errorf("listen on %s: %w", s.endpoint, err)
	}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		sig := <-stop
		klog.InfoS("shutting down", "signal", sig.String())
		s.server.GracefulStop()
	}()

	klog.InfoS("serving CSI", "endpoint", s.endpoint)
	return s.server.Serve(lis)
}

func parseEndpoint(ep string) (network, addr string, err error) {
	switch {
	case strings.HasPrefix(ep, "unix://"):
		return "unix", strings.TrimPrefix(ep, "unix://"), nil
	case strings.HasPrefix(ep, "tcp://"):
		return "tcp", strings.TrimPrefix(ep, "tcp://"), nil
	default:
		return "", "", fmt.Errorf("unsupported endpoint %q (want unix:// or tcp://)", ep)
	}
}

func logInterceptor(ctx context.Context, req any, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
	klog.V(4).InfoS("gRPC call", "method", info.FullMethod)
	resp, err := handler(ctx, req)
	if err != nil {
		klog.ErrorS(err, "gRPC call failed", "method", info.FullMethod)
	}
	return resp, err
}
