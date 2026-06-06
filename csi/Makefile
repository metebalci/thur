BINARY := thurvsa-csi
VERSION ?= dev

.PHONY: build vet test lint tidy clean

build:
	go build -ldflags "-X main.version=$(VERSION)" -o bin/$(BINARY) ./cmd/$(BINARY)

vet:
	go vet ./...

test:
	go test ./...

lint:
	golangci-lint run

tidy:
	go mod tidy

clean:
	rm -rf bin
