#!/bin/sh
# Grants the one narrow permission brain's virtual keyboard tests need.
# Run once:
#   sudo sh brain/scripts/setup-test-permissions.sh          # install
#   sudo sh brain/scripts/setup-test-permissions.sh --undo   # remove
#
# It gives the user logged in at the local seat (systemd "uaccess")
# read/write access to /dev/uinput, so tests can type through a real kernel
# input device. Any program running as that user can then inject
# keystrokes, which is the trade-off.
set -eu

if [ "$(id -u)" -ne 0 ] || [ -z "${SUDO_USER:-}" ]; then
    echo "run with sudo from your own account: sudo sh $0 [--undo]" >&2
    exit 1
fi
rule=/etc/udev/rules.d/70-brain-uinput.rules

if [ "${1:-}" = "--undo" ]; then
    rm -f "$rule"
    udevadm control --reload
    udevadm trigger --name-match=uinput
    echo "removed $rule"
    exit 0
fi

printf '%s\n' 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' > "$rule"
chmod 0644 "$rule"
udevadm control --reload
udevadm trigger --name-match=uinput

echo "installed $rule:"
cat "$rule"
