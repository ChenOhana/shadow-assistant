//! The platform layer: what `brain run` needs from the operating system and
//! the desktop, behind interfaces, so that another platform is a new module
//! rather than a change to the core.
//!
//! A [`Platform`] starts capture and writes everything into a [`Recorder`]:
//!
//! * the keys as the focused app received them ([`Recorder::key`]),
//! * the focused app and window title ([`Recorder::set_focus`]),
//! * screen lock, which pauses recording ([`Recorder::set_paused`]),
//! * sleep and wake ([`Recorder::system`]),
//! * the clipboard text at a paste shortcut, through the
//!   [`ClipboardSource`](super::recorder::ClipboardSource) the recorder is
//!   built with.
//!
//! Everything after the recorder (the log, ingest, sessions, reconstruction,
//! storage and queries) is platform-independent.
//!
//! **The contract for a key source**, whatever the platform:
//!
//! * keys are reported as the app received them: characters resolved with
//!   the user's active layout, shortcuts named on the Latin layout (Ctrl+A,
//!   not Ctrl+ש), repeats on the schedule the app sees;
//! * in time order, with key transitions delivered before the repeats due at
//!   the same moment;
//! * passively: capture never grabs the keyboard or delays a key, and the
//!   source blocks on nothing but the recorder's single log write;
//! * keys held on a device that disappears are released, so a modifier can
//!   never stay stuck.
//!
//! Platforms today: [`Linux`](super::linux::Linux) (keys from evdev resolved
//! with xkbcommon, plus a backend per desktop environment, see
//! [`super::linux::desktop`]) and the synthetic test source
//! [`Synthetic`](super::linux::synthetic::Synthetic). A macOS or Windows port
//! would add a module here with its own key source (a CGEventTap, a
//! low-level keyboard hook), focus source and lock/sleep events.

use super::recorder::Recorder;
use crate::datadir::DataDir;
use crate::eventlog::LogWriter;
use anyhow::Result;
use std::sync::Arc;

/// An operating system (or test source) that `brain run` can capture from.
pub trait Platform {
    /// Starts capturing into a new [`Recorder`] over `log`, after writing
    /// the `start` marker. Desktop integration that is unavailable is
    /// reported on stderr and skipped; only what makes capture impossible
    /// (no readable keyboard device, say) is an error.
    fn start(&self, data_dir: &DataDir, log: LogWriter) -> Result<Box<dyn Capture>>;
}

/// A running capture, started by [`Platform::start`].
pub trait Capture {
    /// The recorder every source writes into.
    fn recorder(&self) -> &Arc<Recorder>;
    /// Stops every source and waits for their threads, leaving nothing
    /// behind on the desktop (a KWin script, say).
    fn stop(self: Box<Self>);
}

/// The platform of the machine `brain` runs on.
pub fn native() -> Box<dyn Platform> {
    Box::new(super::linux::Linux)
}
