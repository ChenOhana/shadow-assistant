//! The data directory layout and the single-writer lock.
//!
//! ```text
//! <data-dir>/            mode 0700: it holds everything the user typed
//!   events.jsonl         capture log, the ground truth (append-only)
//!   brain.db             SQLite index, derived from the log (WAL mode)
//!   brain.db-wal/-shm    SQLite WAL files
//!   run.lock             flock'd by the one `brain run` using this directory
//!   kwin/                generated KWin focus-tracking script
//!   vscode/              VS Code replay sandbox and snapshots (--vscode-replay)
//! ```

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// A brain data directory and the files in it.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// The data directory at `root` (nothing is created yet).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Creates the directory (mode 0700) if needed. Used by `brain run`.
    pub fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating data dir {}", self.root.display()))?;
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", self.root.display()))?;
        fs::create_dir_all(self.kwin_dir())?;
        Ok(())
    }

    /// Checks that the directory holds captured data. Used by `brain query`.
    pub fn ensure_exists(&self) -> Result<()> {
        if !self.root.is_dir() {
            bail!(
                "data dir {} does not exist (start `brain run --data-dir {}` first)",
                self.root.display(),
                self.root.display()
            );
        }
        if !self.db_path().is_file() {
            bail!(
                "no brain database in {} (start `brain run --data-dir {}` first)",
                self.root.display(),
                self.root.display()
            );
        }
        Ok(())
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// `events.jsonl`, the capture log.
    pub fn log_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }
    /// `brain.db`, the SQLite index.
    pub fn db_path(&self) -> PathBuf {
        self.root.join("brain.db")
    }
    /// `run.lock`, held by the running `brain run`.
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("run.lock")
    }
    /// Private working area of the VS Code replay (tier 2).
    pub fn vscode_dir(&self) -> PathBuf {
        self.root.join("vscode")
    }

    /// `kwin/`, where the KWin focus script is written (KDE).
    pub fn kwin_dir(&self) -> PathBuf {
        self.root.join("kwin")
    }

    /// Takes the exclusive writer lock, held for as long as the returned guard lives.
    /// The kernel releases a flock when its process dies, so a `kill -9` never
    /// leaves a stale lock behind.
    pub fn lock_exclusive(&self) -> Result<RunLock> {
        let path = self.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: flock on a valid, owned file descriptor.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!(
                    "another `brain run` is already using {} (lock: {})",
                    self.root.display(),
                    path.display()
                );
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        Ok(RunLock { _file: file })
    }
}

/// Holds the data directory's writer lock.
pub struct RunLock {
    _file: File,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = DataDir::new(tmp.path().join("d"));
        dd.create().unwrap();
        let mode = fs::metadata(dd.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let guard = dd.lock_exclusive().unwrap();
        let err = dd.lock_exclusive().err().expect("second lock must fail");
        assert!(err.to_string().contains("already using"), "{err}");
        drop(guard);
        dd.lock_exclusive().unwrap();
    }

    #[test]
    fn query_on_missing_dir_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = DataDir::new(tmp.path().join("nope"));
        assert!(dd.ensure_exists().is_err());
    }
}
