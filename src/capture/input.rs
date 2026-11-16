//! The input thread: reads every keyboard with one `poll(2)` loop.
//!
//! Mice and touchpads are not read: the mouse position is invisible to
//! `brain`, so a click could only say "the cursor moved somewhere", which
//! reconstruction cannot use (see README, "Decisions and edge cases").
//!
//! * **Passive.** Devices are opened read-only and never grabbed, so the
//!   compositor receives every event exactly as before. `brain` cannot add
//!   input lag, whatever it does.
//! * **Cheap.** Each device gets an evdev event mask (`EVIOCSMASK`) so the
//!   kernel only wakes us for key transitions. LED, `MSC_SCAN` and other
//!   events never reach userspace. When nothing is pressed, the thread
//!   sleeps in `poll` with no timeout.
//! * **Robust.** `/dev/input` is watched with inotify for hotplug. A vanished
//!   device releases its held keys. On resume from sleep, every device is
//!   closed and re-opened.

use super::keys::{KeyOut, KeyProcessor};
use super::recorder::Recorder;
use crate::timefmt::{Millis, now_ms, system_time_ms};
use anyhow::{Context, Result, bail};
use evdev::raw_stream::RawDevice;
use evdev::{EventType, KeyCode};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

const INPUT_DIR: &str = "/dev/input";

// evdev codes not exported as consts we can match on.
const SYN_DROPPED: u16 = 3;

/// Messages from other threads to the input thread.
pub enum Control {
    /// Close and re-open all devices (after resume from sleep).
    Reopen,
    /// KDE's layout switcher changed the active layout.
    SetLayout(u32),
    /// Replace the keymap (the layout list changed).
    SetKeyboard(Box<super::xkb::Keyboard>),
    Stop,
}

/// Sends [`Control`] messages and wakes the input thread's `poll`.
#[derive(Clone)]
pub struct ControlHandle {
    tx: Sender<Control>,
    wake: Arc<OwnedFd>,
}

impl ControlHandle {
    pub fn send(&self, msg: Control) {
        if self.tx.send(msg).is_ok() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a valid eventfd.
            unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
        }
    }
}

struct Device {
    id: u32,
    path: PathBuf,
    dev: RawDevice,
}

pub struct InputLoop {
    devices: Vec<Device>,
    next_id: u32,
    inotify: OwnedFd,
    wake: Arc<OwnedFd>,
    rx: Receiver<Control>,
    keys: KeyProcessor,
    recorder: Arc<Recorder>,
    /// Paths that failed with EACCES, reported once.
    denied: HashSet<PathBuf>,
}

impl InputLoop {
    pub fn new(keys: KeyProcessor, recorder: Arc<Recorder>) -> Result<(Self, ControlHandle)> {
        // SAFETY: plain syscalls; return values are checked.
        let wake = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake < 0 {
            return Err(std::io::Error::last_os_error()).context("eventfd");
        }
        // SAFETY: `wake` is a fresh, owned fd.
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(wake) });
        let inotify = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify_init1");
        }
        // SAFETY: fresh, owned fd.
        let inotify = unsafe { OwnedFd::from_raw_fd(inotify) };
        let dir = CString::new(INPUT_DIR).unwrap();
        // IN_ATTRIB: udev applies the `input` group permissions after creating
        // the node, so a device may only become openable on that event.
        let wd = unsafe {
            libc::inotify_add_watch(
                inotify.as_raw_fd(),
                dir.as_ptr(),
                libc::IN_CREATE | libc::IN_ATTRIB,
            )
        };
        if wd < 0 {
            return Err(std::io::Error::last_os_error()).context("watching /dev/input");
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = ControlHandle {
            tx,
            wake: wake.clone(),
        };
        let me = Self {
            devices: Vec::new(),
            next_id: 0,
            inotify,
            wake,
            rx,
            keys,
            recorder,
            denied: HashSet::new(),
        };
        Ok((me, handle))
    }

    /// Opens all current devices. Fails if there are input devices but none can
    /// be read, which almost always means missing permissions.
    pub fn open_initial(&mut self) -> Result<()> {
        self.scan();
        if self.devices.is_empty() {
            if !self.denied.is_empty() {
                bail!(
                    "permission denied reading {INPUT_DIR}/event*: add your user to the `input` \
                     group (`sudo usermod -aG input $USER`, then log out and back in)"
                );
            }
            eprintln!("brain: warning: no keyboard found yet; waiting for one to be plugged in");
        }
        Ok(())
    }

    pub fn keyboard_count(&self) -> usize {
        self.devices.len()
    }

    /// Opens any `/dev/input/event*` node not already open.
    fn scan(&mut self) {
        let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"))
            })
            .collect();
        paths.sort();
        for path in paths {
            if self.devices.iter().any(|d| d.path == path) {
                continue;
            }
            match open_device(&path) {
                Ok(Some(dev)) => {
                    self.denied.remove(&path);
                    let id = self.next_id;
                    self.next_id += 1;
                    self.devices.push(Device { id, path, dev });
                }
                Ok(None) => {} // not a keyboard
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    self.denied.insert(path);
                }
                Err(_) => {} // vanished or not ready yet; inotify will tell us again
            }
        }
    }

    fn close_all(&mut self) {
        for d in self.devices.drain(..) {
            self.keys.device_gone(d.id);
        }
        self.keys.release_all();
    }

    /// Runs until [`Control::Stop`].
    pub fn run(mut self) {
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let mut gone: Vec<usize> = Vec::new();
        loop {
            fds.clear();
            fds.push(pollfd(self.wake.as_raw_fd()));
            fds.push(pollfd(self.inotify.as_raw_fd()));
            for d in &self.devices {
                fds.push(pollfd(d.dev.as_raw_fd()));
            }
            let timeout = match self.keys.deadline() {
                Some(at) => (at - now_ms()).clamp(0, i32::MAX as Millis) as i32,
                None => -1,
            };
            // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                eprintln!("brain: poll failed: {err}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            // Synthesized key repeats that came due while we slept.
            let rec = self.recorder.clone();
            self.keys.tick(now_ms(), &mut |k| emit(&rec, k));

            if fds[0].revents != 0 {
                drain_fd(self.wake.as_raw_fd());
                while let Ok(msg) = self.rx.try_recv() {
                    match msg {
                        Control::Stop => {
                            self.close_all();
                            return;
                        }
                        Control::Reopen => {
                            self.close_all();
                            self.scan();
                            eprintln!(
                                "brain: resumed: reopened {} keyboard(s)",
                                self.devices.len()
                            );
                        }
                        Control::SetLayout(i) => self.keys.set_layout_index(i),
                        Control::SetKeyboard(kb) => self.keys.replace_keyboard(*kb),
                    }
                }
                continue; // device set may have changed; rebuild fds
            }
            if fds[1].revents != 0 {
                drain_fd(self.inotify.as_raw_fd());
                self.scan();
                continue;
            }

            gone.clear();
            for (i, pfd) in fds[2..].iter().enumerate() {
                if pfd.revents == 0 {
                    continue;
                }
                if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                    && pfd.revents & libc::POLLIN == 0
                {
                    gone.push(i);
                    continue;
                }
                if !self.read_device(i) {
                    gone.push(i);
                }
            }
            for &i in gone.iter().rev() {
                let d = self.devices.remove(i);
                self.keys.device_gone(d.id);
            }
        }
    }

    /// Drains one device. Returns false if the device is gone.
    fn read_device(&mut self, idx: usize) -> bool {
        let rec = self.recorder.clone();
        let Self { devices, keys, .. } = self;
        let d = &mut devices[idx];
        let mut resync = false;
        loop {
            let events: Vec<evdev::InputEvent> = match d.dev.fetch_events() {
                Ok(it) => it.collect(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false, // ENODEV: unplugged
            };
            for ev in events {
                let ts = system_time_ms(ev.timestamp());
                if ev.event_type() == EventType::SYNCHRONIZATION && ev.code() == SYN_DROPPED {
                    resync = true;
                    continue;
                }
                if ev.event_type() != EventType::KEY {
                    continue;
                }
                let code = ev.code();
                let value = ev.value();
                // A keyboard with a built-in touchpad also reports mouse
                // buttons; those are not keys.
                if !is_button(code) {
                    keys.key(d.id, code, value, ts, &mut |k| emit(&rec, k));
                }
            }
        }
        if resync {
            // The kernel buffer overflowed: re-read which keys are down.
            if let Ok(state) = d.dev.get_key_state() {
                let pressed: HashSet<u16> = state
                    .iter()
                    .map(|k| k.code())
                    .filter(|c| !is_button(*c))
                    .collect();
                keys.resync(d.id, &pressed);
            }
        }
        true
    }
}

fn emit(rec: &Recorder, k: KeyOut) {
    rec.key(k.ts, k.key, k.mods, k.repeat);
}

/// BTN_* codes (mouse, joystick, touch) as opposed to keyboard keys.
fn is_button(code: u16) -> bool {
    (0x100..0x160).contains(&code) || code >= 0x2c0
}

/// A keyboard: has the letter, space and enter keys (power buttons, lid
/// switches and media remotes report EV_KEY too, but not these).
fn is_keyboard(dev: &RawDevice) -> bool {
    let Some(keys) = dev.supported_keys() else {
        return false;
    };
    [
        KeyCode::KEY_A,
        KeyCode::KEY_Z,
        KeyCode::KEY_SPACE,
        KeyCode::KEY_ENTER,
    ]
    .iter()
    .all(|k| keys.contains(*k))
}

/// Opens a device if it is a keyboard. `Ok(None)` for other devices.
fn open_device(path: &Path) -> std::io::Result<Option<RawDevice>> {
    let dev = RawDevice::open(path)?;
    if !is_keyboard(&dev) {
        return Ok(None);
    }
    let fd = dev.as_raw_fd();
    // SAFETY: fcntl on a valid fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    set_event_type_mask(fd);
    Ok(Some(dev))
}

/// Asks the kernel to deliver only EV_SYN and EV_KEY events to this client.
/// Motion-only frames then produce no wakeups at all, because the kernel
/// drops a SYN_REPORT whose frame is empty after masking. Best effort: on
/// kernels without EVIOCSMASK we just filter in userspace.
fn set_event_type_mask(fd: RawFd) {
    #[repr(C)]
    struct InputMask {
        kind: u32,
        codes_size: u32,
        codes_ptr: u64,
    }
    // _IOW('E', 0x93, struct input_mask)
    const EVIOCSMASK: libc::c_ulong = (1 << 30)
        | ((std::mem::size_of::<InputMask>() as libc::c_ulong) << 16)
        | (0x45 << 8)
        | 0x93;
    const EV_SYN: u32 = 0;
    const EV_KEY: u32 = 1;
    // type 0 = "mask of event types": a bitmap over EV_CNT (0x20) types.
    let bitmap: u32 = (1 << EV_SYN) | (1 << EV_KEY);
    let mask = InputMask {
        kind: 0,
        codes_size: 4,
        codes_ptr: &bitmap as *const u32 as u64,
    };
    // SAFETY: the ioctl reads `mask` and the 4-byte bitmap it points to; both outlive the call.
    unsafe { libc::ioctl(fd, EVIOCSMASK as _, &mask) };
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

fn drain_fd(fd: RawFd) {
    let mut buf = [0u8; 4096];
    // SAFETY: reading into a valid buffer from a non-blocking fd.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_ranges() {
        assert!(is_button(0x110)); // BTN_LEFT
        assert!(is_button(0x14a)); // BTN_TOUCH
        assert!(!is_button(30)); // KEY_A
        assert!(!is_button(0x160)); // KEY_OK
        assert!(is_button(0x2c0)); // BTN_TRIGGER_HAPPY1
    }

    #[test]
    fn eviocsmask_number_matches_kernel_header() {
        // From <linux/input.h>: EVIOCSMASK = _IOW('E', 0x93, struct input_mask), sizeof = 16.
        let v: libc::c_ulong = (1 << 30) | (16 << 16) | (0x45 << 8) | 0x93;
        assert_eq!(v, 0x4010_4593);
    }
}
