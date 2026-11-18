//! A synthetic input source for tests and benchmarks (`brain run --synthetic RATE`).
//!
//! It types a fixed corpus as raw evdev key transitions, at `RATE` key presses
//! per second, through the same [`KeyProcessor`] → [`Recorder`] → log → ingest
//! path as real input. After every sentence it switches to a terminal, runs a
//! shell command (Enter commits that session) and switches back (the editor
//! session resumes), so sessions open, commit and resume all the time. Only
//! the evdev device read is replaced, and desktop integration is off.
//! Crash-safety, consistency and resource tests can therefore run unattended,
//! without root access to `/dev/uinput` or a desktop.

use super::keys::{KeyOut, KeyProcessor, RepeatConfig};
use super::xkb::{Keyboard, LayoutSpec};
use crate::capture::platform::{Capture, Platform};
use crate::capture::recorder::{NoClipboard, Recorder};
use crate::datadir::DataDir;
use crate::event::SystemState;
use crate::eventlog::LogWriter;
use crate::timefmt::now_ms;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The synthetic source as a [`Platform`]: types at `rate` key presses per
/// second on the US layout.
pub struct Synthetic {
    /// Key presses per second.
    pub rate: f64,
}

impl Platform for Synthetic {
    fn start(&self, _data_dir: &DataDir, log: LogWriter) -> Result<Box<dyn Capture>> {
        let recorder = Arc::new(Recorder::new(log, Box::new(NoClipboard)));
        recorder.system(SystemState::Start);
        let kb = Keyboard::new(&LayoutSpec {
            layout: "us".into(),
            ..Default::default()
        })?;
        let keys = KeyProcessor::new(kb, RepeatConfig::default());
        let stop = Arc::new(AtomicBool::new(false));
        let rate = self.rate;
        let thread = {
            let (recorder, stop) = (recorder.clone(), stop.clone());
            std::thread::Builder::new()
                .name("synthetic".into())
                .spawn(move || run(keys, recorder, rate, stop))?
        };
        eprintln!("brain: synthetic input at {rate} keys/s");
        Ok(Box::new(SyntheticCapture {
            recorder,
            stop,
            thread,
        }))
    }
}

struct SyntheticCapture {
    recorder: Arc<Recorder>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Capture for SyntheticCapture {
    fn recorder(&self) -> &Arc<Recorder> {
        &self.recorder
    }

    fn stop(self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        if self.thread.join().is_err() {
            eprintln!("brain: warning: synthetic input thread panicked");
        }
    }
}

/// The app the synthetic editor window belongs to.
pub const SYNTHETIC_APP: &str = "synthetic";
/// The synthetic editor window's title.
pub const SYNTHETIC_WINDOW: &str = "corpus.txt";
/// The app id of the synthetic terminal (a real terminal class, so Enter commits).
pub const SYNTHETIC_TERMINAL: &str = "org.kde.konsole";
const COMMANDS: [&str; 3] = ["git status", "cargo test", "ls -la"];

const CORPUS: &str = "the migration is blocked on the schema change. \
    Dana asked whether we can ship search on Friday; I said yes if the index rebuild \
    finishes overnight. Next: run the backfill on prod and check the error rate.\n";

const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSPACE: u16 = 14;
const KEY_ENTER: u16 = 28;

/// evdev keycode and whether Shift is needed, for the US layout.
pub fn us_keycode(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    Some(match c {
        'a'..='z' => (LETTERS[(c as u8 - b'a') as usize], false),
        'A'..='Z' => (LETTERS[(c as u8 - b'A') as usize], true),
        '1'..='9' => (2 + (c as u8 - b'1') as u16, false),
        '0' => (11, false),
        ' ' => (57, false),
        '\n' => (28, false),
        '.' => (52, false),
        ',' => (51, false),
        ';' => (39, false),
        ':' => (39, true),
        '?' => (53, true),
        '-' => (12, false),
        '\'' => (40, false),
        _ => return None,
    })
}

enum Step {
    Key(u16, bool),
    Focus(&'static str, &'static str),
}

/// Types the corpus until `stop` is set. Every 50th word gets a typo that is
/// immediately corrected with Backspace, to exercise editing keys.
pub fn run(mut keys: KeyProcessor, recorder: Arc<Recorder>, rate_hz: f64, stop: Arc<AtomicBool>) {
    recorder.set_focus(SYNTHETIC_APP, SYNTHETIC_WINDOW, false);
    let period = Duration::from_secs_f64(1.0 / rate_hz.max(0.1));
    let start = Instant::now();
    let mut presses: u64 = 0;
    let mut words: u64 = 0;
    let mut sentences: usize = 0;
    let mut emit = |k: KeyOut| recorder.key(k.ts, k.key, k.mods, k.repeat);
    'outer: loop {
        for c in CORPUS.chars() {
            let Some((code, shift)) = us_keycode(c) else {
                continue;
            };
            let mut strokes: Vec<(u16, bool)> = vec![(code, shift)];
            if c == ' ' {
                words += 1;
                if words.is_multiple_of(50) {
                    strokes.insert(0, (us_keycode('x').unwrap().0, false));
                    strokes.insert(1, (KEY_BACKSPACE, false));
                }
            }
            // After a sentence: a shell command in the terminal, then back.
            let detour = c == '.';
            let mut steps: Vec<Step> = strokes.into_iter().map(|(c, s)| Step::Key(c, s)).collect();
            if detour {
                let cmd = COMMANDS[sentences % COMMANDS.len()];
                sentences += 1;
                steps.push(Step::Focus(SYNTHETIC_TERMINAL, "~"));
                steps.extend(
                    cmd.chars()
                        .filter_map(us_keycode)
                        .map(|(c, s)| Step::Key(c, s)),
                );
                steps.push(Step::Key(KEY_ENTER, false));
                steps.push(Step::Focus(SYNTHETIC_APP, SYNTHETIC_WINDOW));
            }
            for step in steps {
                let (code, shift) = match step {
                    Step::Key(code, shift) => (code, shift),
                    Step::Focus(app, window) => {
                        recorder.set_focus(app, window, false);
                        continue;
                    }
                };
                if stop.load(Ordering::Relaxed) {
                    break 'outer;
                }
                // Pace against the start time so the long-run rate is exact.
                presses += 1;
                let due = start + period.mul_f64(presses as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                let ts = now_ms();
                if shift {
                    keys.key(0, KEY_LEFTSHIFT, 1, ts, &mut emit);
                }
                keys.key(0, code, 1, ts, &mut emit);
                keys.key(0, code, 0, ts, &mut emit);
                if shift {
                    keys.key(0, KEY_LEFTSHIFT, 0, ts, &mut emit);
                }
            }
        }
    }
    keys.release_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_fully_typeable() {
        for c in CORPUS.chars().chain(COMMANDS.concat().chars()) {
            assert!(us_keycode(c).is_some(), "no keycode for {c:?}");
        }
    }
}
