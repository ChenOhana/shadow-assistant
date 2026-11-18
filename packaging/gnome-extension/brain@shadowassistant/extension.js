// brain's focus reporter for GNOME Shell: the GNOME counterpart of the KWin
// script brain loads on KDE Plasma.
//
// Wayland gives ordinary programs no way to ask "which window is focused?",
// so this extension runs inside GNOME Shell and offers, on the Shell's own
// session-bus connection, at /org/shadowassistant/GnomeShell:
//
//   signal Focus(desktopFile, windowClass, title, renamed)
//       on every focus change, and on title changes of the focused window
//       (renamed = true: the same window with a new title).
//   signal InputSourceChanged(id)      the layout switched (Super+Space)
//   signal InputSourcesChanged()       the layout list changed
//   method Current() -> (desktopFile, windowClass, title)
//   method InputSource() -> id         the active input source ("us", "il")
//   method Clipboard() -> text         the clipboard text, read on request
//
// brain trusts these only when they come from the owner of org.gnome.Shell,
// so no other program can pose as this extension.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';
import * as Keyboard from 'resource:///org/gnome/shell/ui/status/keyboard.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const OBJECT_PATH = '/org/shadowassistant/GnomeShell';
const IFACE_XML = `
<node>
  <interface name="org.shadowassistant.GnomeShell">
    <method name="Current">
      <arg type="s" direction="out" name="desktopFile"/>
      <arg type="s" direction="out" name="windowClass"/>
      <arg type="s" direction="out" name="title"/>
    </method>
    <method name="InputSource">
      <arg type="s" direction="out" name="id"/>
    </method>
    <method name="Clipboard">
      <arg type="s" direction="out" name="text"/>
    </method>
    <signal name="Focus">
      <arg type="s" name="desktopFile"/>
      <arg type="s" name="windowClass"/>
      <arg type="s" name="title"/>
      <arg type="b" name="renamed"/>
    </signal>
    <signal name="InputSourceChanged">
      <arg type="s" name="id"/>
    </signal>
    <signal name="InputSourcesChanged"/>
  </interface>
</node>`;

// The app of a window: its desktop file (as KWin reports it), and its class
// (the Wayland app id) for windows without one.
function describe(window) {
    if (!window)
        return ['', '', ''];
    const app = Shell.WindowTracker.get_default().get_window_app(window);
    let desktopFile = app ? app.get_id() ?? '' : '';
    if (desktopFile.startsWith('window:'))
        desktopFile = ''; // the tracker's made-up id for windows without one
    const windowClass = window.get_gtk_application_id() || window.get_wm_class() || '';
    return [desktopFile, windowClass, window.get_title() ?? ''];
}

export default class BrainFocusExtension extends Extension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);

        this._window = null;
        this._titleId = 0;
        this._focusId = global.display.connect('notify::focus-window',
            () => this._onFocus());

        this._sources = Keyboard.getInputSourceManager();
        this._sourceId = this._sources.connect('current-source-changed', () =>
            this._dbus.emit_signal('InputSourceChanged',
                new GLib.Variant('(s)', [this.InputSource()])));
        this._sourcesId = this._sources.connect('sources-changed', () =>
            this._dbus.emit_signal('InputSourcesChanged', null));

        // A brain already running (started before GNOME loaded this) learns
        // the current window and layout now.
        this._onFocus();
        this._dbus.emit_signal('InputSourceChanged',
            new GLib.Variant('(s)', [this.InputSource()]));
    }

    disable() {
        this._unwatchTitle();
        global.display.disconnect(this._focusId);
        this._sources.disconnect(this._sourceId);
        this._sources.disconnect(this._sourcesId);
        this._sources = null;
        this._dbus.unexport();
        this._dbus = null;
    }

    _onFocus() {
        this._unwatchTitle();
        const window = global.display.focus_window;
        if (window) {
            this._window = window;
            this._titleId = window.connect('notify::title', () =>
                this._report(window, true));
        }
        this._report(window, false);
    }

    _unwatchTitle() {
        if (this._window && this._titleId)
            this._window.disconnect(this._titleId);
        this._window = null;
        this._titleId = 0;
    }

    _report(window, renamed) {
        const [desktopFile, windowClass, title] = describe(window);
        this._dbus.emit_signal('Focus',
            new GLib.Variant('(sssb)', [desktopFile, windowClass, title, renamed]));
    }

    // D-Bus methods.

    Current() {
        return describe(global.display.focus_window);
    }

    InputSource() {
        return this._sources?.currentSource?.id ?? '';
    }

    ClipboardAsync(_params, invocation) {
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD,
            (_clipboard, text) =>
                invocation.return_value(new GLib.Variant('(s)', [text ?? ''])));
    }
}
