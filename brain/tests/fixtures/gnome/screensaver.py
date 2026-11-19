"""Test only: stands in for GNOME Shell's org.gnome.ScreenSaver on the private
bus of tests/gnome_e2e.rs. The private headless shell has no login session
(on purpose, so it cannot touch the real session's lock state), so it does
not provide the lock screen itself. Lock() emits ActiveChanged(true), as
GNOME Shell does when the screen locks."""

from gi.repository import Gio, GLib

XML = """
<node>
  <interface name="org.gnome.ScreenSaver">
    <method name="Lock"/>
    <method name="GetActive"><arg type="b" direction="out"/></method>
    <signal name="ActiveChanged"><arg type="b"/></signal>
  </interface>
</node>"""
NODE = Gio.DBusNodeInfo.new_for_xml(XML)
active = False


def on_call(conn, sender, path, iface, method, params, invocation):
    global active
    if method == "GetActive":
        invocation.return_value(GLib.Variant("(b)", (active,)))
    elif method == "Lock":
        active = True
        invocation.return_value(None)
        conn.emit_signal(None, path, iface, "ActiveChanged", GLib.Variant("(b)", (True,)))


def on_bus(conn, _name):
    conn.register_object("/org/gnome/ScreenSaver", NODE.interfaces[0], on_call, None, None)


Gio.bus_own_name(Gio.BusType.SESSION, "org.gnome.ScreenSaver", Gio.BusNameOwnerFlags.NONE,
                 on_bus, None, None)
GLib.MainLoop().run()
