//! Sleep and wake from systemd-logind, the same on every desktop.

use super::dbus;
use super::input::{Control, ControlHandle};
use crate::capture::recorder::Recorder;
use crate::event::SystemState;
use std::sync::Arc;
use zbus::blocking::Connection;

/// Marks sleep/wake in the log and re-opens input devices on resume (their
/// file descriptors can go stale across suspend).
pub(super) fn watch_sleep(system: &Connection, recorder: Arc<Recorder>, control: ControlHandle) {
    dbus::spawn_signal_watch(
        system,
        "org.freedesktop.login1",
        "org.freedesktop.login1.Manager",
        "PrepareForSleep",
        None,
        move |msg| match msg.body().deserialize::<bool>() {
            Ok(true) => recorder.system(SystemState::Sleep),
            Ok(false) => {
                recorder.system(SystemState::Wake);
                control.send(Control::Reopen);
            }
            Err(_) => {}
        },
    );
}
