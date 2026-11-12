.DEFAULT_GOAL := server

GO ?= go
GOOS ?= linux
GOARCH ?= amd64
OUTPUT ?= dist/mamla-$(GOOS)-$(GOARCH)

.PHONY: server
server:
	mkdir -p "$(dir $(OUTPUT))"
	CGO_ENABLED=0 GOOS="$(GOOS)" GOARCH="$(GOARCH)" $(GO) build -trimpath -ldflags="-s -w" -o "$(OUTPUT)" .
