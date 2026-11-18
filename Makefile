# Build `brain` and install it system-wide.
#
#   make                    build (release) and install to $(BINDIR), plus
#                           the GNOME Shell extension when GNOME is installed
#   make build              build only
#   make install-gnome-extension    the GNOME Shell extension only (needed
#                           for focus tracking on GNOME; log out and in once)
#   make install-service    install + enable the systemd user service
#   make uninstall-service  stop and remove the service and its data
#                           (KEEP_DATA=1: keep the data)
#   make uninstall          remove the installed binary (and the extension)
#
# cargo runs as you; only the copy uses sudo, so target/ never becomes
# root-owned and root needs no Rust toolchain.

PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
BIN    := target/release/brain

.PHONY: all build install uninstall install-service uninstall-service \
	install-gnome-extension uninstall-gnome-extension

all: install

build:
	cargo build --release

install: build
	sudo install -m 0755 $(BIN) $(BINDIR)/brain
	@if command -v gnome-shell >/dev/null; then $(MAKE) --no-print-directory install-gnome-extension; fi

uninstall:
	sudo rm -f $(BINDIR)/brain
	@if [ -d "$(GNOME_EXT_DIR)" ]; then $(MAKE) --no-print-directory uninstall-gnome-extension; fi

# brain's GNOME Shell extension, for the current user (no sudo).
GNOME_EXT_DIR := $(or $(XDG_DATA_HOME),$(HOME)/.local/share)/gnome-shell/extensions/brain@shadowassistant

install-gnome-extension:
	sh scripts/gnome-extension.sh install

uninstall-gnome-extension:
	sh scripts/gnome-extension.sh uninstall

# The service runs as you (no root); see packaging/brain.service. Its data
# lives in its systemd state directory.
UNITDIR  ?= $(HOME)/.config/systemd/user
STATEDIR := $(or $(XDG_STATE_HOME),$(HOME)/.local/state)/brain

install-service: install
	install -Dm 0644 packaging/brain.service $(UNITDIR)/brain.service
	systemctl --user daemon-reload
	systemctl --user enable --now brain.service
	@echo "brain service running; data in $(STATEDIR)"

uninstall-service:
	-systemctl --user disable --now brain.service
ifeq ($(KEEP_DATA),1)
	@echo "keeping the service's data in $(STATEDIR)"
else
	@echo "deleting the service's data in $(STATEDIR)"
	-systemctl --user clean --what=state brain.service
endif
	rm -f $(UNITDIR)/brain.service
	systemctl --user daemon-reload
