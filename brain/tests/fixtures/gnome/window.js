// Test only: a GTK 4 window of app ARGV[2], titled ARGV[0], that renames
// itself to "ARGV[0] renamed" after ARGV[1] milliseconds (tests/gnome_e2e.rs).
import Gtk from 'gi://Gtk?version=4.0';
import GLib from 'gi://GLib';

const [title, renameAfterMs, appId] = ARGV;
const app = new Gtk.Application({application_id: appId});
app.connect('activate', () => {
    const window = new Gtk.ApplicationWindow({application: app, title});
    window.present();
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, Number(renameAfterMs), () => {
        window.set_title(`${title} renamed`);
        return GLib.SOURCE_REMOVE;
    });
});
app.run([]);
