#!/bin/sh
# Installs or removes brain's GNOME Shell extension for the current user
# (no root). brain needs it on GNOME for focus tracking, pastes and layout
# switches: Wayland gives other programs no way to see the focused window.
#
#   sh scripts/gnome-extension.sh install     # copy + enable
#   sh scripts/gnome-extension.sh uninstall   # disable + remove
#
# GNOME on Wayland loads a newly installed extension only after you log out
# and back in once; brain picks it up as soon as it runs.
set -eu

UUID=brain@shadowassistant
SRC="$(dirname "$0")/../packaging/gnome-extension/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

enabled() { gsettings get org.gnome.shell enabled-extensions; }

case "${1:-}" in
install)
    mkdir -p "$DEST"
    cp "$SRC/metadata.json" "$SRC/extension.js" "$DEST/"
    # `gnome-extensions enable` only knows extensions the running shell has
    # loaded, which a new one is not until the next login: edit the list.
    list=$(enabled)
    case "$list" in
    *"'$UUID'"*) ;;
    "@as []" | "[]") gsettings set org.gnome.shell enabled-extensions "['$UUID']" ;;
    *) gsettings set org.gnome.shell enabled-extensions "${list%]}, '$UUID']" ;;
    esac
    if [ "$(gsettings get org.gnome.shell disable-user-extensions)" = "true" ]; then
        echo "note: user extensions are turned off in GNOME (Extensions app); brain's needs them on"
    fi
    echo "installed $DEST; log out and back in once so GNOME Shell loads it"
    ;;
uninstall)
    list=$(enabled)
    new=$(printf '%s' "$list" | sed "s/, '$UUID'//; s/'$UUID', //; s/'$UUID'//")
    [ "$new" = "$list" ] || gsettings set org.gnome.shell enabled-extensions "$new"
    rm -rf "$DEST"
    echo "removed $DEST"
    ;;
*)
    echo "usage: sh $0 install|uninstall" >&2
    exit 1
    ;;
esac
