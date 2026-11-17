//! `brain run`: capture → log → ingest, until SIGINT/SIGTERM.
//!
//! Threads:
//! * **input**: one `poll` loop over all keyboards (the hot path, see [`input`]).
//! * **ingest**: tails the log into SQLite once per second, and fdatasyncs the log.
//! * **D-Bus**: zbus's connection executors plus one small thread per watched
//!   signal (focus calls from KWin, lock, sleep, layout).
//! * **tier2** (only with `--vscode-replay`): VS Code replays, one at a time.
//! * **main**: waits for a termination signal, then shuts down in order.

pub mod desktop;
pub mod input;
pub mod keys;
pub mod recorder;
pub mod synthetic;
pub mod xkb;

use crate::datadir::DataDir;
use crate::event::SystemState;
use crate::eventlog::LogWriter;
use crate::ingest::Ingester;
use crate::reconstruct::tier2::{Tier2Result, Tier2Worker, VsCodeSnapshots};
use crate::sessions::Tier2Snapshotter;
use crate::store;
use anyhow::{Context, Result};
use input::{Control, ControlHandle, InputLoop};
use keys::{KeyProcessor, RepeatConfig};
use recorder::{ClipboardSource, NoClipboard, Recorder};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use xkb::{Keyboard, LayoutSpec};

/// How often ingest tails the log (and fdatasyncs it).
pub const INGEST_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Test hook: type a built-in corpus at this many keys per second instead
    /// of reading input devices (see [`synthetic`]). Desktop integration is off.
    pub synthetic_rate: Option<f64>,
    /// Reconstruct VS Code sessions by replaying them into a sandboxed VS
    /// Code (tier 2). Off by default.
    pub vscode_replay: bool,
}

pub fn run(data_dir: &DataDir, opts: &RunOptions) -> Result<()> {
    // Block termination signals before any thread exists, so every thread
    // inherits the mask and only `wait_for_signal` on the main thread sees them.
    let signals = block_termination_signals()?;
    // Everything brain writes is private to the user.
    // SAFETY: umask is always safe to call.
    unsafe { libc::umask(0o077) };

    data_dir.create()?;
    let _lock = data_dir.lock_exclusive()?;
    let log = LogWriter::open(&data_dir.log_path())?;
    let conn = store::open_writer(&data_dir.db_path())?;
    let tier2: Option<Box<dyn Tier2Snapshotter>> = if opts.vscode_replay {
        let snaps = VsCodeSnapshots::new(data_dir.vscode_dir().join("snapshots"))?;
        snaps.prune(&conn)?;
        Some(Box::new(snaps))
    } else {
        None
    };
    let ingester = Ingester::with_tier2(conn, &data_dir.log_path(), tier2)?;
    let tier2_worker = if opts.vscode_replay {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = Tier2Worker::spawn(
            data_dir.db_path(),
            data_dir.log_path(),
            data_dir.vscode_dir(),
            tx,
        )?;
        eprintln!("brain: VS Code replay (tier 2) is on");
        Some((worker, rx))
    } else {
        None
    };

    let source = match opts.synthetic_rate {
        Some(rate) => start_synthetic(log, rate)?,
        None => start_capture(data_dir, log)?,
    };
    let recorder = source.recorder().clone();
    let ingest = IngestThread::spawn(ingester, recorder.clone(), tier2_worker)?;
    if opts.vscode_replay {
        recorder.wake_on_focus(
            ingest.thread.thread().clone(),
            crate::reconstruct::vscode::is_vscode,
        );
    }

    let sig = wait_for_signal(&signals)?;
    eprintln!("brain: received signal {sig}, stopping");

    source.stop();
    recorder.system(SystemState::Stop);
    ingest.stop();
    Ok(())
}

/// A running input source; [`Source::stop`] shuts it down.
enum Source {
    Devices {
        recorder: Arc<Recorder>,
        control: ControlHandle,
        thread: JoinHandle<()>,
        kwin: Option<Arc<desktop::KwinFocus>>,
    },
    Synthetic {
        recorder: Arc<Recorder>,
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
    },
}

impl Source {
    fn recorder(&self) -> &Arc<Recorder> {
        match self {
            Source::Devices { recorder, .. } | Source::Synthetic { recorder, .. } => recorder,
        }
    }

    fn stop(self) {
        match self {
            Source::Devices {
                control,
                thread,
                kwin,
                ..
            } => {
                control.send(Control::Stop);
                if thread.join().is_err() {
                    eprintln!("brain: warning: input thread panicked");
                }
                if let Some(k) = kwin {
                    k.unload();
                }
            }
            Source::Synthetic { stop, thread, .. } => {
                stop.store(true, Ordering::Relaxed);
                if thread.join().is_err() {
                    eprintln!("brain: warning: synthetic input thread panicked");
                }
            }
        }
    }
}

/// Real capture: input devices + KDE desktop integration.
fn start_capture(data_dir: &DataDir, log: LogWriter) -> Result<Source> {
    let clipboard: Box<dyn ClipboardSource> = match desktop::KlipperClipboard::connect() {
        Ok(c) => Box::new(c),
        Err(e) => {
            eprintln!("brain: warning: no session bus, pastes will not be resolved: {e:#}");
            Box::new(NoClipboard)
        }
    };
    let recorder = Arc::new(Recorder::new(log, clipboard));
    recorder.system(SystemState::Start);

    let system_bus = match zbus::blocking::Connection::system() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("brain: warning: no system bus, sleep/wake will not be detected: {e:#}");
            None
        }
    };
    let spec = desktop::resolve_layout(system_bus.as_ref());
    let kb = Keyboard::new(&spec).or_else(|e| {
        eprintln!("brain: warning: layout {spec:?} failed ({e:#}); falling back to us");
        Keyboard::new(&LayoutSpec::default())
    })?;
    let repeat = desktop::read_kde_config("kcminputrc")
        .map(|t| RepeatConfig::from_kcminputrc(&t))
        .unwrap_or_default();
    let (mut input, control) = InputLoop::new(KeyProcessor::new(kb, repeat), recorder.clone())?;
    input.open_initial()?;
    let keyboards = input.keyboard_count();

    let kwin = match desktop::KwinFocus::start(recorder.clone(), &data_dir.kwin_dir()) {
        Ok(k) => {
            let k = Arc::new(k);
            k.watch_kwin_restarts();
            Some(k)
        }
        Err(e) => {
            eprintln!(
                "brain: warning: focus tracking unavailable, keys will be attributed to \
                 'unknown' (needs KDE Plasma/KWin): {e:#}"
            );
            None
        }
    };
    let session_bus = match &kwin {
        Some(k) => Some(k.connection().clone()),
        None => zbus::blocking::Connection::session().ok(),
    };
    if let Some(s) = &session_bus {
        desktop::watch_screen_lock(s, recorder.clone());
        desktop::watch_layouts(s, system_bus.clone(), control.clone());
    }
    if let Some(sys) = &system_bus {
        desktop::watch_sleep(sys, recorder.clone(), control.clone());
    }

    let thread = std::thread::Builder::new()
        .name("input".into())
        .spawn(move || input.run())?;
    eprintln!(
        "brain: capturing from {keyboards} keyboard(s), focus tracking {}, layout {}; data in {}",
        if kwin.is_some() {
            "via KWin"
        } else {
            "unavailable"
        },
        if spec.layout.is_empty() {
            "us"
        } else {
            &spec.layout
        },
        data_dir.root().display()
    );
    Ok(Source::Devices {
        recorder,
        control,
        thread,
        kwin,
    })
}

fn start_synthetic(log: LogWriter, rate: f64) -> Result<Source> {
    let recorder = Arc::new(Recorder::new(log, Box::new(NoClipboard)));
    recorder.system(SystemState::Start);
    let kb = Keyboard::new(&LayoutSpec {
        layout: "us".into(),
        ..Default::default()
    })?;
    let keys = KeyProcessor::new(kb, RepeatConfig::default());
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (recorder, stop) = (recorder.clone(), stop.clone());
        std::thread::Builder::new()
            .name("synthetic".into())
            .spawn(move || synthetic::run(keys, recorder, rate, stop))?
    };
    eprintln!("brain: synthetic input at {rate} keys/s");
    Ok(Source::Synthetic {
        recorder,
        stop,
        thread,
    })
}

/// Tails the log into SQLite every [`INGEST_INTERVAL`] and fdatasyncs the log.
struct IngestThread {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl IngestThread {
    fn spawn(
        mut ingester: Ingester,
        recorder: Arc<Recorder>,
        tier2: Option<(Tier2Worker, std::sync::mpsc::Receiver<Tier2Result>)>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ingest".into())
            .spawn(move || {
                loop {
                    // Read the flag first: the final pass after `stop()` must
                    // still sync and ingest everything written before it.
                    let stopping = flag.load(Ordering::Acquire);
                    if let Err(e) = recorder.sync() {
                        eprintln!("brain: warning: {e:#}");
                    }
                    if let Some((_, results)) = &tier2 {
                        while let Ok((id, r)) = results.try_recv() {
                            ingester.add_tier2_result(id, r);
                        }
                    }
                    if let Err(e) = ingester.catch_up(1_000) {
                        eprintln!("brain: warning: ingest failed (will retry): {e:#}");
                    }
                    let jobs = ingester.take_tier2_jobs();
                    if let Some((worker, _)) = &tier2 {
                        for id in jobs {
                            worker.submit(id);
                        }
                    }
                    if stopping {
                        break;
                    }
                    std::thread::park_timeout(INGEST_INTERVAL);
                }
                // In-flight and queued replays stay pending in the database
                // and are retried on the next start.
                if let Some((worker, _)) = tier2 {
                    worker.stop();
                }
            })?;
        Ok(Self { stop, thread })
    }

    /// Runs a final sync + ingest pass and waits for it.
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        if self.thread.join().is_err() {
            eprintln!("brain: warning: ingest thread panicked");
        }
    }
}

fn block_termination_signals() -> Result<libc::sigset_t> {
    // SAFETY: sigset manipulation on a local, then pthread_sigmask on this thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc)).context("pthread_sigmask");
        }
        Ok(set)
    }
}

fn wait_for_signal(set: &libc::sigset_t) -> Result<i32> {
    let mut sig: libc::c_int = 0;
    // SAFETY: `set` is a valid sigset; `sig` is a valid out-pointer.
    let rc = unsafe { libc::sigwait(set, &mut sig) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc)).context("sigwait");
    }
    Ok(sig)
}
