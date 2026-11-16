#!/bin/sh
# Grants the two narrow permissions brain's hardware tests need. Run once:
#   sudo sh scripts/setup-test-permissions.sh          # install
#   sudo sh scripts/setup-test-permissions.sh --undo   # remove
#
# 1. Sleep/wake test: lets the invoking user run exactly
#    `sudo /usr/sbin/rtcwake -m mem -s 20` (suspend, auto-wake after 20 s)
#    without a password. No other command or argument is allowed.
# 2. Virtual keyboard tests: gives the user logged in at the local seat
#    (systemd "uaccess") read/write access to /dev/uinput, so tests can type
#    through a real kernel input device. Any program running as that user
#    can then inject keystrokes, which is the trade-off.
set -eu

user="${SUDO_USER:-}"
if [ "$(id -u)" -ne 0 ] || [ -z "$user" ]; then
    echo "run with sudo from your own account: sudo sh $0 [--undo]" >&2
    exit 1
fi
sudoers=/etc/sudoers.d/brain-tests
rule=/etc/udev/rules.d/70-brain-uinput.rules

if [ "${1:-}" = "--undo" ]; then
    rm -f "$sudoers" "$rule"
    udevadm control --reload
    udevadm trigger --name-match=uinput
    echo "removed $sudoers and $rule"
    exit 0
fi

tmp=$(mktemp)
printf '%s ALL=(root) NOPASSWD: /usr/sbin/rtcwake -m mem -s 20\n' "$user" > "$tmp"
visudo -cf "$tmp" >/dev/null
install -m 0440 -o root -g root "$tmp" "$sudoers"
rm -f "$tmp"

printf '%s\n' 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' > "$rule"
chmod 0644 "$rule"
udevadm control --reload
udevadm trigger --name-match=uinput

echo "installed $sudoers:"
cat "$sudoers"
echo "installed $rule:"
cat "$rule"
