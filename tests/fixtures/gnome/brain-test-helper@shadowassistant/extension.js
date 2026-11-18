// Test only, loaded into the private headless GNOME Shell of
// tests/gnome_e2e.rs (never installed for real): a headless shell has no
// keyboard or mouse, so this does what a user would: leave the overview GNOME
// opens at startup, click a window, set the clipboard, press Super+Space.

import Gio from 'gi://Gio';
import St from 'gi://St';
import * as Keyboard from 'resource:///org/gnome/shell/ui/status/keyboard.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const IFACE_XML = `
<node>
  <interface name="org.shadowassistant.TestHelper">
    <method name="Ready"><arg type="b" direction="out" name="ready"/></method>
    <method name="ActivateWindow"><arg type="s" direction="in" name="title"/></method>
    <method name="SetClipboard"><arg type="s" direction="in" name="text"/></method>
    <method name="ActivateInputSource"><arg type="s" direction="in" name="id"/></method>
  </interface>
</node>`;

export default class TestHelper extends Extension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, '/org/shadowassistant/TestHelper');
        // GNOME opens the overview at startup; new windows get no focus there.
        this._started = !Main.layoutManager._startingUp;
        this._startupId = Main.layoutManager.connect('startup-complete', () => {
            this._started = true;
            Main.overview.hide();
        });
        if (this._started)
            Main.overview.hide();
    }

    disable() {
        Main.layoutManager.disconnect(this._startupId);
        this._dbus.unexport();
        this._dbus = null;
    }

    Ready() {
        return this._started && !Main.overview.visible;
    }

    ActivateWindow(title) {
        const window = global.get_window_actors()
            .map(actor => actor.get_meta_window())
            .find(w => w.get_title() === title);
        if (!window)
            throw new Error(`no window titled ${title}`);
        Main.activateWindow(window);
    }

    SetClipboard(text) {
        St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
    }

    ActivateInputSource(id) {
        const manager = Keyboard.getInputSourceManager();
        const source = Object.values(manager.inputSources).find(s => s.id === id);
        if (!source)
            throw new Error(`no input source ${id}`);
        source.activate(true);
    }
}
