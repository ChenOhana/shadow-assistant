//! `brain run`: capture → log → ingest, until SIGINT/SIGTERM.
//!
//! Capture itself comes from a [`platform::Platform`] (Linux today, see
//! [`linux`]); this module wires it to the log, ingest and tier 2, which are
//! the same on every platform.
//!
//! Threads (Linux):
//! * **input**: one `poll` loop over all keyboards (the hot path, see
//!   [`linux::input`]).
//! * **ingest**: tails the log into SQLite once per second, and fdatasyncs the log.
//! * **D-Bus**: zbus's connection executors plus one small thread per watched
//!   signal (focus calls from the desktop, lock, sleep, layout).
//! * **tier2** (only with `--vscode-replay`): VS Code tier 2 jobs (a saved
//!   file's diff, or a replay), one at a time.
//! * **main**: waits for a termination signal, then shuts down in order.

#[cfg(not(target_os = "linux"))]
compile_error!(
    "brain captures on Linux only for now; another OS needs a platform module \
     in src/capture/ (see src/capture/platform.rs)"
);

pub mod linux;
pub mod platform;
pub mod recorder;

use crate::datadir::DataDir;
use crate::event::SystemState;
use crate::eventlog::LogWriter;
use crate::ingest::Ingester;
use crate::reconstruct::tier2::{Tier2Result, Tier2Worker, VsCodeSnapshots};
use crate::sessions::Tier2Snapshotter;
use crate::store;
use anyhow::{Context, Result};
use platform::Platform;
use recorder::Recorder;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often ingest tails the log (and fdatasyncs it).
pub const INGEST_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Test hook: type a built-in corpus at this many keys per second instead
    /// of reading input devices (see [`linux::synthetic`]). Desktop
    /// integration is off.
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

    let platform: Box<dyn Platform> = match opts.synthetic_rate {
        Some(rate) => Box::new(linux::synthetic::Synthetic { rate }),
        None => platform::native(),
    };
    let capture = platform.start(data_dir, log)?;
    let recorder = capture.recorder().clone();
    let ingest = IngestThread::spawn(ingester, recorder.clone(), tier2_worker)?;
    if opts.vscode_replay {
        recorder.wake_on_focus(
            ingest.thread.thread().clone(),
            crate::reconstruct::vscode::is_vscode,
        );
    }

    let sig = wait_for_signal(&signals)?;
    eprintln!("brain: received signal {sig}, stopping");

    capture.stop();
    recorder.system(SystemState::Stop);
    ingest.stop();
    Ok(())
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
