# Build `brain` and install it system-wide.
#
#   make                    build (release) and install to $(BINDIR)
#   make build              build only
#   make install-service    install + enable the systemd user service
#   make uninstall-service  stop, disable and remove the service
#   make uninstall          remove the installed binary
#
# cargo runs as you; only the copy uses sudo, so target/ never becomes
# root-owned and root needs no Rust toolchain.

PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
BIN    := target/release/brain

.PHONY: all build install uninstall install-service uninstall-service

all: install

build:
	cargo build --release

install: build
	sudo install -m 0755 $(BIN) $(BINDIR)/brain

uninstall:
	sudo rm -f $(BINDIR)/brain

# The service runs as you (no root); see packaging/brain.service.
UNITDIR ?= $(HOME)/.config/systemd/user

install-service: install
	install -Dm 0644 packaging/brain.service $(UNITDIR)/brain.service
	systemctl --user daemon-reload
	systemctl --user enable --now brain.service

uninstall-service:
	-systemctl --user disable --now brain.service
	rm -f $(UNITDIR)/brain.service
	systemctl --user daemon-reload
