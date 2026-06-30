# aish — build & install
#
# `make install` builds the release binary, copies it onto your PATH, and
# (on Apple Silicon macOS) re-signs it. That last step is NOT optional: a
# cargo-built arm64 binary carries a *linker-signed* ad-hoc signature, and
# macOS's AMFI SIGKILLs it on exec the moment it's copied to a new path
# (`zsh: killed  aish`). A fresh `codesign --force --sign -` gives it a valid
# cdhash so it launches. Always install through this target — a bare
# `cp target/release/aish ~/.local/bin` will reintroduce the kill-on-launch bug.

CARGO   ?= cargo
PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin
BIN      = aish
RELEASE  = target/release/$(BIN)
DEST     = $(BINDIR)/$(BIN)
UNAME_S := $(shell uname -s)

.PHONY: all build test test-local install sign uninstall clean register-shell worker-image worker-image-multiarch

all: build

build:
	$(CARGO) build --release

# Test build policy: mistralrs-core (the `local` in-process model) is dropped
# from test builds by default — it's huge, slow to compile, and irrelevant to
# the unit/oracle/pty suites. Run `make test-local` (or
# `cargo test --features local`) only when you explicitly need to exercise the
# local-inference path. This mirrors the CI gate (.github/workflows/ci.yml).
test:
	$(CARGO) test --no-default-features $(CARGO_TEST_ARGS)

# Opt back in to the mistralrs-backed local-inference path for tests.
test-local:
	$(CARGO) test --features local $(CARGO_TEST_ARGS)

# Build, copy onto PATH, then re-sign (macOS). Depends on `build` so the
# binary is always current.
install: build
	mkdir -p $(BINDIR)
	install -m 0755 $(RELEASE) $(DEST)
	@$(MAKE) --no-print-directory sign DEST=$(DEST)
	@$(MAKE) --no-print-directory register-shell DEST=$(DEST)
	@echo "installed $(DEST)"

# Register the installed binary in /etc/shells so `chsh -s $(DEST)` accepts it
# as a login shell. Idempotent (skips when already listed) and best-effort: a
# failure (no sudo, read-only /etc) warns but never fails the install.
register-shell:
	@if grep -qxF "$(DEST)" /etc/shells 2>/dev/null; then \
		echo "/etc/shells already lists $(DEST)"; \
	else \
		echo "registering $(DEST) in /etc/shells (may prompt for sudo)"; \
		echo "$(DEST)" | sudo tee -a /etc/shells >/dev/null \
			&& echo "registered $(DEST) — now run: chsh -s $(DEST)" \
			|| echo "could not write /etc/shells — add $(DEST) manually to use chsh"; \
	fi

# Re-sign the installed binary with a fresh ad-hoc signature. No-op off macOS.
sign:
ifeq ($(UNAME_S),Darwin)
	codesign --force --sign - $(DEST)
	@codesign -v $(DEST) && echo "signature OK: $(DEST)"
endif

uninstall:
	rm -f $(DEST)

clean:
	$(CARGO) clean

# ---------------------------------------------------------------------------
# Container worker image (S9.1)
# ---------------------------------------------------------------------------
# `worker-image` builds the LOCAL, single-arch image the container worker
# backend (src/container.rs) launches each background coordinator in. The tag is
# version-pinned (`aish-worker:<version>`) so a new binary rebuilds it; the
# build-on-first-use path in worker.rs shells out to exactly this target. Prefers
# docker, falls back to podman. `worker-image-multiarch` is the release-time
# multi-arch publish (linux/amd64 + linux/arm64) per AC5.

WORKER_IMAGE ?= aish-worker
# Default the tag to the Cargo package version; an env override (passed by the
# build-on-first-use path) wins via `?=`.
VERSION      ?= $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
WORKER_TAG    = $(WORKER_IMAGE):$(VERSION)
PLATFORMS    ?= linux/amd64,linux/arm64

worker-image: build
	@echo "building $(WORKER_TAG) (local, single-arch) …"
	@if command -v docker >/dev/null 2>&1; then 		docker build -t $(WORKER_TAG) -f Dockerfile.worker . ; 	elif command -v podman >/dev/null 2>&1; then 		podman build -t $(WORKER_TAG) -f Dockerfile.worker . ; 	else 		echo "worker-image: neither docker nor podman is on PATH" >&2 ; exit 1 ; 	fi
	@echo "built $(WORKER_TAG)"

# Multi-arch publish (AC5). Requires a glibc-matching aish build per platform;
# wire this into CI where cross-compiled binaries are available. `PUSH=1` pushes.
worker-image-multiarch:
	@echo "building $(WORKER_TAG) for $(PLATFORMS) …"
	@if command -v docker >/dev/null 2>&1; then 		docker buildx build --platform $(PLATFORMS) -t $(WORKER_TAG) 			-f Dockerfile.worker $(if $(PUSH),--push,--load) . ; 	elif command -v podman >/dev/null 2>&1; then 		podman build --platform $(PLATFORMS) -t $(WORKER_TAG) -f Dockerfile.worker . ; 	else 		echo "worker-image-multiarch: neither docker nor podman is on PATH" >&2 ; exit 1 ; 	fi
