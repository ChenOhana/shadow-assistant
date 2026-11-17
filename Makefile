# Build `brain` and install it system-wide.
#
#   make                    build (release) and install to $(BINDIR)
#   make build              build only
#   make install-service    install + enable the systemd user service
#   make uninstall-service  stop and remove the service and its data
#                           (KEEP_DATA=1: keep the data)
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
