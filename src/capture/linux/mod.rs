//! Linux: keys from the kernel's input devices, everything else from the
//! desktop environment.
//!
//! * **Keys** ([`input`], [`keys`], [`xkb`]) work the same on every Linux
//!   desktop: evdev is read passively and each key is resolved with
//!   xkbcommon into what the app received. Only the keyboard *settings*
//!   (layouts, layout switches, the repeat schedule) come from the desktop.
//! * **Sleep and wake** come from systemd-logind (`logind.rs`), on any
//!   desktop.
//! * **Focus, clipboard, screen lock and keyboard settings** come from a
//!   [`desktop::Desktop`] backend, chosen at start ([`desktop::detect`]):
//!   KDE Plasma today. A desktop without a backend still gets keys, filed
//!   under the app `unknown`, with a warning.

pub mod desktop;
pub mod input;
pub mod keys;
pub mod synthetic;
pub mod xkb;

mod dbus;
mod kde;
mod layout;
mod logind;

use super::platform::{Capture, Platform};
use super::recorder::Recorder;
use crate::datadir::DataDir;
use crate::event::SystemState;
use crate::eventlog::LogWriter;
use anyhow::Result;
use desktop::{DesktopContext, DesktopSession};
use input::{Control, ControlHandle, InputLoop};
use keys::KeyProcessor;
use std::sync::Arc;
use std::thread::JoinHandle;
use xkb::{Keyboard, LayoutSpec};

/// Real capture on Linux: input devices plus desktop integration.
pub struct Linux;

impl Platform for Linux {
    fn start(&self, data_dir: &DataDir, log: LogWriter) -> Result<Box<dyn Capture>> {
        let desktop = desktop::detect();
        let recorder = Arc::new(Recorder::new(log, desktop.clipboard()));
        recorder.system(SystemState::Start);

        let system_bus = match zbus::blocking::Connection::system() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("brain: warning: no system bus, sleep/wake will not be detected: {e:#}");
                None
            }
        };
        let (spec, repeat) = desktop.keyboard(system_bus.as_ref());
        let kb = Keyboard::new(&spec).or_else(|e| {
            eprintln!("brain: warning: layout {spec:?} failed ({e:#}); falling back to us");
            Keyboard::new(&LayoutSpec::default())
        })?;
        let (mut input, control) = InputLoop::new(KeyProcessor::new(kb, repeat), recorder.clone())?;
        input.open_initial()?;
        let keyboards = input.keyboard_count();

        let session = desktop.start(DesktopContext {
            recorder: recorder.clone(),
            input: control.clone(),
            system_bus: system_bus.clone(),
            data_dir,
        });
        if let Some(sys) = &system_bus {
            logind::watch_sleep(sys, recorder.clone(), control.clone());
        }

        let thread = std::thread::Builder::new()
            .name("input".into())
            .spawn(move || input.run())?;
        eprintln!(
            "brain: capturing from {keyboards} keyboard(s), desktop {}, focus tracking {}, \
             layout {}; data in {}",
            desktop.name(),
            session.focus_tracking(),
            if spec.layout.is_empty() {
                "us"
            } else {
                &spec.layout
            },
            data_dir.root().display()
        );
        Ok(Box::new(LinuxCapture {
            recorder,
            control,
            thread,
            session,
        }))
    }
}

struct LinuxCapture {
    recorder: Arc<Recorder>,
    control: ControlHandle,
    thread: JoinHandle<()>,
    session: Box<dyn DesktopSession>,
}

impl Capture for LinuxCapture {
    fn recorder(&self) -> &Arc<Recorder> {
        &self.recorder
    }

    fn stop(self: Box<Self>) {
        self.control.send(Control::Stop);
        if self.thread.join().is_err() {
            eprintln!("brain: warning: input thread panicked");
        }
        self.session.stop();
    }
}
