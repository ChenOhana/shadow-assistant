# Build `brain` and install it system-wide.
#
#   make            build (release) and install to $(BINDIR)
#   make build      build only
#   make uninstall  remove the installed binary
#
# cargo runs as you; only the copy uses sudo, so target/ never becomes
# root-owned and root needs no Rust toolchain.

PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
BIN    := target/release/brain

.PHONY: all build install uninstall

all: install

build:
	cargo build --release

install: build
	sudo install -m 0755 $(BIN) $(BINDIR)/brain

uninstall:
	sudo rm -f $(BINDIR)/brain
