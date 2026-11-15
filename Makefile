.DEFAULT_GOAL := server

GO ?= go
GOOS ?= linux
GOARCH ?= amd64
OUTPUT ?= dist/mamla-$(GOOS)-$(GOARCH)
CARGO ?= cargo
CLIENT_OUTPUT ?= dist/mamla-client

.PHONY: server
server:
	mkdir -p "$(dir $(OUTPUT))"
	CGO_ENABLED=0 GOOS="$(GOOS)" GOARCH="$(GOARCH)" $(GO) build -trimpath -ldflags='-s -w -extldflags "-static"' -o "$(OUTPUT)" .

.PHONY: client
client:
	@test "$$(uname -s)" = Linux || { printf '%s\n' 'Build the PipeWire client on Linux with GTK4/GStreamer development packages installed.'; exit 1; }
	cd linux-client && $(CARGO) build --release --locked --target-dir target
	mkdir -p "$(dir $(CLIENT_OUTPUT))"
	cp linux-client/target/release/mamla-linux "$(CLIENT_OUTPUT)"
