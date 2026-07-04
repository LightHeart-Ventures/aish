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

# Cross-worktree build serialization (OOM mitigation #2).
# Many background-coordinator worktrees can share one host; an unbounded set of
# concurrent `cargo build`s overcommits RAM and trips the kernel OOM-killer (the
# per-build `jobs` cap in .cargo/config.toml only bounds ONE build's parallelism,
# not the number of builds running at once). A single advisory file lock bounds
# *cross-build* concurrency to 1 — each build still uses up to `jobs` cores
# internally, but only one worktree compiles at a time. flock is util-linux
# (Linux only); on macOS / hosts without it, LOCKED collapses to a no-op prefix.
BUILD_LOCK ?= /tmp/aish-build.lock
FLOCK      := $(shell command -v flock 2>/dev/null)
ifeq ($(FLOCK),)
LOCKED      =
else
LOCKED      = $(FLOCK) $(BUILD_LOCK)
endif

.PHONY: all build build-fast test test-local test-repl install sign uninstall clean register-shell worker-image worker-image-multiarch

all: build

build:
	$(LOCKED) $(CARGO) build --release

# Fast, Claude-only build (OOM mitigation #1): drops the heavy `local`
# (mistralrs / candle / gemm) feature entirely — the whole opt-level=3 phase and
# the crate that peaks past 1.5 GB per rustc — so coordinator / CI / worktree
# rebuilds that never touch local inference compile a fraction of the graph.
# Serialized across worktrees like every other build target here. Equivalent to
# `scripts/build.sh --release`.
build-fast:
	$(LOCKED) $(CARGO) build --release --no-default-features

# Test build policy: mistralrs-core (the `local` in-process model) is dropped
# from test builds by default — it's huge, slow to compile, and irrelevant to
# the unit/oracle/pty suites. Run `make test-local` (or
# `cargo test --features local`) only when you explicitly need to exercise the
# local-inference path. This mirrors the CI gate (.github/workflows/ci.yml).
test:
	$(LOCKED) $(CARGO) test --no-default-features $(CARGO_TEST_ARGS)

# Opt back in to the mistralrs-backed local-inference path for tests.
test-local:
	$(LOCKED) $(CARGO) test --features local $(CARGO_TEST_ARGS)

# End-to-end REPL smoke: drive the real aish TUI through coder/agent-tty and
# assert on rendered terminal state (boot banner, :help, clean :quit). Hermetic
# (built-ins only — no model call / API key). Auto-detects the binary; builds a
# fast Claude-only release first if none is present. Needs Node >=24 + jq; SKIPs
# cleanly when a prerequisite is absent (set AISH_REPL_STRICT=1 to hard-fail).
# See tests/repl/README.md.
test-repl:
	@if [ ! -x "$(RELEASE)" ] && [ -z "$(AISH_BIN)" ] && ! command -v aish >/dev/null 2>&1; then \
		$(MAKE) --no-print-directory build-fast ; \
	fi
	tests/repl/agent_tty_smoke.sh

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
#
# NOTE: Dockerfile.worker is a MULTI-STAGE self-build — it compiles aish INSIDE
# a bookworm builder stage so the runtime glibc always matches the binary. There
# is therefore NO host-build prerequisite (a host binary would reintroduce the
# glibc-mismatch bug this design removes), and the build context must include the
# source tree (`.dockerignore` trims target/, .git, worktrees).

WORKER_IMAGE ?= aish-worker
# Default the tag to the Cargo package version; an env override (passed by the
# build-on-first-use path) wins via `?=`.
VERSION      ?= $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
WORKER_TAG    = $(WORKER_IMAGE):$(VERSION)
PLATFORMS    ?= linux/amd64,linux/arm64

worker-image:
	@echo "building $(WORKER_TAG) (local, single-arch, self-building) …"
	@if command -v docker >/dev/null 2>&1; then 		docker build -t $(WORKER_TAG) -f Dockerfile.worker . ; 	elif command -v podman >/dev/null 2>&1; then 		podman build -t $(WORKER_TAG) -f Dockerfile.worker . ; 	else 		echo "worker-image: neither docker nor podman is on PATH" >&2 ; exit 1 ; 	fi
	@echo "built $(WORKER_TAG)"

# Multi-arch publish (AC5). The multi-stage self-build compiles aish per-platform
# INSIDE the image (buildx emulates the non-native arch), so no pre-cross-compiled
# host binary is required. `PUSH=1` pushes.
worker-image-multiarch:
	@echo "building $(WORKER_TAG) for $(PLATFORMS) …"
	@if command -v docker >/dev/null 2>&1; then 		docker buildx build --platform $(PLATFORMS) -t $(WORKER_TAG) 			-f Dockerfile.worker $(if $(PUSH),--push,--load) . ; 	elif command -v podman >/dev/null 2>&1; then 		podman build --platform $(PLATFORMS) -t $(WORKER_TAG) -f Dockerfile.worker . ; 	else 		echo "worker-image-multiarch: neither docker nor podman is on PATH" >&2 ; exit 1 ; 	fi
