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

.PHONY: all build install sign uninstall clean register-shell

all: build

build:
	$(CARGO) build --release

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
