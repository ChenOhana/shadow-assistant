//! Tier 2 for VS Code: replay a session's keystrokes into a real, invisible
//! VS Code and read back what it saved.
//!
//! Running the real editor with the user's own settings and keybindings
//! reproduces everything tier 0 can only approximate: auto-closing brackets
//! and quotes, auto-indent on Enter, multi-cursor and line-editing shortcuts,
//! undo grouping, format-on-save, and the user's keybinding overrides.
//!
//! # The sandbox
//!
//! - **No display server.** Electron's headless Ozone backend
//!   (`--ozone-platform=headless`): nothing is drawn on screen and no Xvfb or
//!   nested compositor is needed. To watch replays, set
//!   `BRAIN_VSCODE_VISIBLE=1`: VS Code then opens as a normal Wayland window
//!   (the Wayland socket is a file, so the network namespace does not block
//!   it); everything else about the sandbox stays the same.
//! - **No network.** The instance runs in a fresh network namespace
//!   (`unshare --map-current-user -n`, unprivileged): only a down loopback
//!   device, so extensions, telemetry and sync cannot reach anything. The
//!   DevTools protocol therefore runs over `--remote-debugging-pipe`
//!   (fds 3 and 4) instead of a TCP port. The session bus is cut too
//!   (`DBUS_SESSION_BUS_ADDRESS` points nowhere) so nothing pops up on the
//!   user's desktop, and secrets use `--password-store=basic`.
//! - **Resource caps.** A transient systemd user scope with `CPUQuota=50%`
//!   and `MemoryMax=1500M`, separate from `brain run`'s own budget. If
//!   `systemd-run --user` is unavailable it runs without the cgroup (warned
//!   once on stderr).
//! - **Own state.** A private `--user-data-dir` and `HOME` under the work
//!   directory. The user's `settings.json`, `keybindings.json` and `snippets/`
//!   are copied in at each start so their real editing behaviour applies; a
//!   few keys are overridden (no autosave, no telemetry/updates/trust
//!   prompts/welcome page, a `window.title` the replayer can parse) and a few
//!   private keybindings on Ctrl+Alt+Shift+F13…F18 are appended to drive the
//!   workbench (quick open, save, revert-and-close, cursor to end, focus
//!   editor). Extensions: a copy of `~/.vscode/extensions` if it is under
//!   300 MB (refreshed when it changes), otherwise `--disable-extensions`.
//!
//! # A replay
//!
//! One warm instance serves all jobs, one at a time. Each job writes the
//! session-start snapshot to `jobs/<n>/<file name>` (the real basename, so the
//! language mode matches), opens it through quick open, moves the cursor to
//! the end (its real position is unknown, the same assumption as tier 0),
//! sends the keys over CDP exactly as captured, saves, waits for the save to
//! land and reads the file back. Shortcuts that would take focus away from
//! the editor are skipped; see `keys.rs`. The instance is killed on any error
//! or timeout and restarted by the next job; it is also shut down when idle
//! ([`VsCodeReplayer::reap_idle`]) and when the replayer is dropped.
//!
//! # Cost
//!
//! Measured on the dev machine (8 cores, VS Code 1.141) under the 50 % CPU
//! cap: a cold start plus first job takes about 20–25 s, a warm job about 2 s
//! plus about 22 ms per key; the instance's processes total about 1.2 GiB RSS
//! (shared pages counted per process). Without the cap it is about 5× faster
//! (1.4 s to a ready workbench, about 1.5 ms per key). Callers should allow
//! a generous timeout (60–120 s) and run replays off the hot path.
//!
//! # Lifetime
//!
//! The instance is started with `PR_SET_PDEATHSIG = SIGKILL`, so it dies with
//! the *thread* that started it, including on `kill -9` of the whole process
//! (measured: gone about 0.2 s later). Call [`VsCodeReplayer::replay`] from
//! one long-lived thread; if that thread exits, the next replay simply starts
//! a new instance. As a second line of defence, an instance left over by a
//! dead owner is found through `instance.json` and killed before a new one
//! starts.

mod cdp;
mod diff;
mod keys;
mod title;

pub use diff::{changed_lines, unified_diff};
pub use title::{is_vscode, resolve_file, resolve_file_in, title_is_dirty, title_is_untitled};

use crate::event::Mod;
use anyhow::{Context, Result, anyhow, bail};
use cdp::Cdp;
use keys::{Action, CdpKey, MOD_ALT, MOD_CTRL, MOD_SHIFT};
use serde_json::{Value, json};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// One captured event of a VS Code session, in log order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayEvent {
    Key { key: String, mods: Vec<Mod> },
    Paste { text: String },
}

pub struct ReplayInput<'a> {
    /// Basename of the edited file (picks VS Code's language mode).
    pub file_name: &'a str,
    /// File content at session start.
    pub initial: &'a [u8],
    pub events: &'a [ReplayEvent],
}

/// Extensions are copied into the sandbox only below this size.
const MAX_EXTENSIONS_BYTES: u64 = 300 << 20;
/// Grace period after the replay deadline before the watchdog kills VS Code.
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

/// Private workbench keybindings (Ctrl+Alt+Shift+F13…F18), appended to the
/// user's keybindings in the sandbox so the replayer never depends on the
/// user's own bindings for its housekeeping.
const BINDINGS: &[(&str, &str, u32)] = &[
    ("F13", "workbench.action.quickOpen", 124),
    ("F14", "workbench.action.revertAndCloseActiveEditor", 125),
    ("F15", "workbench.action.files.save", 126),
    ("F16", "cursorBottom", 127),
    ("F17", "workbench.action.focusActiveEditorGroup", 128),
    ("F18", "workbench.action.closeQuickOpen", 129),
];

#[derive(Clone, Copy)]
enum Cmd {
    QuickOpen = 0,
    RevertAndClose = 1,
    Save = 2,
    CursorBottom = 3,
    FocusEditor = 4,
    CloseQuickOpen = 5,
}

/// Settings forced in the sandbox on top of the user's own.
fn setting_overrides() -> Value {
    json!({
        "files.autoSave": "off",
        "files.hotExit": "off",
        "files.simpleDialog.enable": true,
        "window.title": "${dirty}${activeEditorLong}",
        "window.restoreWindows": "none",
        "window.confirmBeforeClose": "never",
        "workbench.startupEditor": "none",
        "workbench.enableExperiments": false,
        "workbench.tips.enabled": false,
        "workbench.welcomePage.walkthroughs.openOnInstall": false,
        "workbench.editor.enablePreview": false,
        "workbench.secondarySideBar.defaultVisibility": "hidden",
        "telemetry.telemetryLevel": "off",
        "update.mode": "none",
        "extensions.autoUpdate": false,
        "extensions.autoCheckUpdates": false,
        "extensions.ignoreRecommendations": true,
        "security.workspace.trust.enabled": false,
        "git.enabled": false,
        "git.autoRepositoryDetection": false,
        "chat.disableAIFeatures": true,
        "search.quickOpen.includeHistory": false,
    })
}

/// Everything needed to kill a running instance from any thread.
#[derive(Clone, Debug)]
struct Ids {
    /// Process group (the instance is its leader; `setsid` at spawn).
    pgid: i32,
    /// The transient scope's cgroup directory, once known.
    cgroup: Option<PathBuf>,
    /// Shared with the instance's CDP client, which fails every call once set.
    killed: Arc<AtomicBool>,
}

impl Ids {
    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        if let Some(cg) = &self.cgroup {
            // cgroup v2: kills every process in the scope, including any that
            // left the process group.
            let _ = std::fs::write(cg.join("cgroup.kill"), "1");
        }
        // SAFETY: plain syscall. The group leader is our unreaped child, so
        // its pid (= pgid) cannot have been reused.
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
    }
}

/// Terminates the current VS Code instance from any thread. An in-flight
/// [`VsCodeReplayer::replay`] then fails fast with an error. Idempotent, and a
/// no-op when nothing is running.
#[derive(Clone)]
pub struct KillHandle(Arc<Mutex<Option<Ids>>>);

impl KillHandle {
    pub fn kill(&self) {
        if let Ok(guard) = self.0.lock()
            && let Some(ids) = guard.as_ref()
        {
            ids.kill();
        }
    }
}

struct Instance {
    child: Child,
    cdp: Cdp,
}

/// Set to `1` to show the replay instance as a normal window (for watching
/// tests or replays) instead of running it headless.
pub const VISIBLE_ENV: &str = "BRAIN_VSCODE_VISIBLE";

pub struct VsCodeReplayer {
    work_dir: PathBuf,
    inst: Option<Instance>,
    ids: Arc<Mutex<Option<Ids>>>,
    last_used: Instant,
    job_seq: u64,
    /// Whether `systemd-run --user --scope` works here (probed once).
    systemd: Option<bool>,
}

impl VsCodeReplayer {
    /// No process is started until the first replay. `work_dir` is private
    /// scratch space (`data_dir/vscode`).
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
            inst: None,
            ids: Arc::new(Mutex::new(None)),
            last_used: Instant::now(),
            job_seq: 0,
            systemd: None,
        }
    }

    pub fn kill_handle(&self) -> KillHandle {
        KillHandle(self.ids.clone())
    }

    /// Pid of the running instance's main process (tests use it to inspect
    /// the sandbox).
    #[doc(hidden)]
    pub fn instance_pid(&self) -> Option<u32> {
        self.inst.as_ref().map(|i| i.child.id())
    }

    /// Replays into a fresh scratch copy of `initial` named `file_name`, saves,
    /// and returns the saved content. On any error or timeout the instance is
    /// killed (the next call restarts it) and `Err` is returned.
    pub fn replay(&mut self, input: &ReplayInput, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        // The watchdog enforces the deadline even if VS Code hangs in a way
        // that blocks a pipe write: killing it turns every blocked or later
        // CDP call into an immediate error.
        let (done_tx, done_rx) = channel::<()>();
        let killer = self.kill_handle();
        let watchdog = std::thread::Builder::new()
            .name("brain-vscode-watchdog".into())
            .spawn(move || {
                let wait = (deadline + WATCHDOG_GRACE).saturating_duration_since(Instant::now());
                if let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(wait) {
                    killer.kill();
                }
            })
            .context("spawning watchdog thread")?;
        let result = self.replay_inner(input, deadline);
        drop(done_tx);
        let _ = watchdog.join();
        self.last_used = Instant::now();
        match result {
            Ok(text) => Ok(text),
            Err(e) => {
                self.shutdown();
                if Instant::now() >= deadline {
                    Err(e.context(format!("VS Code replay timed out after {timeout:?}")))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Shuts the warm instance down if it has been idle longer than `idle`.
    pub fn reap_idle(&mut self, idle: Duration) {
        if self.inst.is_some() && self.last_used.elapsed() > idle {
            self.shutdown();
        }
    }

    pub fn shutdown(&mut self) {
        let ids = self.ids.lock().ok().and_then(|mut g| g.take());
        if let Some(ids) = &ids {
            ids.kill();
        }
        if let Some(mut inst) = self.inst.take() {
            if ids.is_none() {
                let _ = inst.child.kill();
            }
            let _ = inst.child.wait();
        }
        let _ = std::fs::remove_file(self.work_dir.join("instance.json"));
    }

    fn replay_inner(&mut self, input: &ReplayInput, deadline: Instant) -> Result<String> {
        trace("replay start");
        let file_name = sanitize_file_name(input.file_name);
        if let Some(inst) = &mut self.inst
            && inst.child.try_wait().ok().flatten().is_some()
        {
            self.shutdown(); // died since the last job
        }
        if self.inst.is_none() {
            self.start(deadline)?;
        }

        self.job_seq += 1;
        let jobs = self.work_dir.join("jobs");
        let job_dir = jobs.join(self.job_seq.to_string());
        let _ = std::fs::remove_dir_all(&jobs);
        std::fs::create_dir_all(&job_dir)
            .with_context(|| format!("creating {}", job_dir.display()))?;
        let path = job_dir.join(&file_name);
        std::fs::write(&path, input.initial)
            .with_context(|| format!("writing {}", path.display()))?;
        let suffix = format!("/jobs/{}/{}", self.job_seq, file_name);

        let inst = self.inst.as_mut().ok_or_else(|| anyhow!("no instance"))?;
        let cdp = &mut inst.cdp;
        close_job_editors(cdp, deadline)?;
        trace("editors closed");
        open_file(cdp, &path, &file_name, &suffix, deadline)?;
        trace("file open");

        replay_actions(cdp, &keys::plan(input.events), deadline)?;
        trace("keys sent");
        // Save, and wait until the title loses the dirty marker for good.
        std::thread::sleep(POLL);
        send_cmd(cdp, Cmd::Save, deadline)?;
        let mut clean_polls = 0;
        loop {
            let t = title(cdp, deadline)?;
            if !t.ends_with(&suffix) {
                bail!("the replayed editor lost focus or closed (title {t:?})");
            }
            if title_is_dirty(&t) {
                clean_polls = 0;
            } else {
                clean_polls += 1;
                if clean_polls >= 3 {
                    break;
                }
            }
            sleep_until(deadline, POLL)?;
        }
        trace("saved");
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        // Leave the instance clean for the next job.
        let _ = send_cmd(cdp, Cmd::RevertAndClose, deadline);
        let _ = std::fs::remove_dir_all(&job_dir);
        Ok(match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    /// Starts the warm instance and waits until its workbench is ready.
    fn start(&mut self, deadline: Instant) -> Result<()> {
        kill_stale(&self.work_dir);
        let bin = code_binary()?;
        let unshare = which("unshare").ok_or_else(|| {
            anyhow!("`unshare` not found: cannot isolate VS Code from the network")
        })?;
        let user_data = self.work_dir.join("user-data");
        let home = self.work_dir.join("home");
        std::fs::create_dir_all(user_data.join("User"))?;
        std::fs::create_dir_all(&home)?;
        prepare_user_config(&user_data.join("User"))?;
        let extensions = prepare_extensions(&self.work_dir, deadline);

        if self.systemd.is_none() {
            let ok = Command::new("systemd-run")
                .args(["--user", "--scope", "-q", "--collect", "--", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                eprintln!(
                    "brain: warning: systemd-run --user is unavailable; \
                     VS Code replay runs without CPU/memory caps"
                );
            }
            self.systemd = Some(ok);
        }
        let unit = format!(
            "brain-vscode-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let mut cmd = if self.systemd == Some(true) {
            let mut c = Command::new("systemd-run");
            c.args([
                "--user",
                "--scope",
                "-q",
                "--collect",
                "-p",
                "CPUQuota=50%",
                "-p",
                "MemoryMax=1500M",
                "--unit",
                &unit,
                "--",
            ]);
            c.arg(&unshare);
            c
        } else {
            Command::new(&unshare)
        };
        cmd.args(["--map-current-user", "-n"]).arg(&bin);
        let visible = std::env::var_os(VISIBLE_ENV).is_some_and(|v| v == "1");
        cmd.arg(if visible {
            "--ozone-platform=wayland"
        } else {
            "--ozone-platform=headless"
        })
        .arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", user_data.display()));
        match &extensions {
            Some(dir) => cmd.arg(format!("--extensions-dir={}", dir.display())),
            None => cmd
                .arg(format!(
                    "--extensions-dir={}",
                    self.work_dir.join("no-extensions").display()
                ))
                .arg("--disable-extensions"),
        };
        cmd.args([
            "--disable-workspace-trust",
            "--skip-welcome",
            "--skip-release-notes",
            "--disable-telemetry",
            "--disable-crash-reporter",
            "--disable-gpu",
            // A headless window counts as hidden; without these Chromium
            // throttles its timers and input handling like a background tab.
            "--disable-renderer-backgrounding",
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--password-store=basic",
            "--new-window",
        ]);
        for (k, _) in std::env::vars_os() {
            let ks = k.to_string_lossy();
            if ks.starts_with("VSCODE_") || ks.starts_with("ELECTRON_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env_remove("DISPLAY");
        if !visible {
            cmd.env_remove("WAYLAND_DISPLAY");
        }
        cmd.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"));
        let log = std::fs::File::create(self.work_dir.join("vscode.log"))?;
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);

        // CDP pipes: the child gets `to_child_r` as fd 3 and `from_child_w` as
        // fd 4. Both are moved above fd 10 first so the dup2s cannot collide.
        let (to_child_r, to_child_w) = pipe_cloexec()?;
        let (from_child_r, from_child_w) = pipe_cloexec()?;
        let to_child_r = dup_above(&to_child_r, 10)?;
        let from_child_w = dup_above(&from_child_w, 10)?;
        let (rfd, wfd) = (to_child_r.as_raw_fd(), from_child_w.as_raw_fd());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(rfd, 3) < 0 || libc::dup2(wfd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::setsid();
                // The instance dies with the thread that started it, even
                // on `kill -9` of brain.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        drop(to_child_r);
        drop(from_child_w);
        let pid = child.id() as i32;
        let killed = Arc::new(AtomicBool::new(false));
        if let Ok(mut g) = self.ids.lock() {
            *g = Some(Ids {
                pgid: pid,
                cgroup: None,
                killed: killed.clone(),
            });
        }
        let _ = std::fs::write(
            self.work_dir.join("instance.json"),
            json!({ "pid": pid, "user_data_dir": user_data }).to_string(),
        );
        let cdp = match Cdp::new(to_child_w, from_child_r, killed) {
            Ok(c) => c,
            Err(e) => {
                let mut child = child;
                if let Some(ids) = self.ids.lock().ok().and_then(|mut g| g.take()) {
                    ids.kill();
                }
                let _ = child.wait();
                return Err(e);
            }
        };
        self.inst = Some(Instance { child, cdp });

        // systemd-run moves itself into the scope before exec'ing the
        // instance; record the scope as soon as it exists so a kill during
        // startup takes every process with it.
        if self.systemd == Some(true) {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(cg) = scope_cgroup(pid as u32, &unit) {
                    if let Ok(mut g) = self.ids.lock()
                        && let Some(ids) = g.as_mut()
                    {
                        ids.cgroup = Some(cg);
                    }
                    break;
                }
                if Instant::now() >= until.min(deadline) {
                    break; // killpg still covers the process group
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        trace("spawned");
        let inst = self.inst.as_mut().ok_or_else(|| anyhow!("no instance"))?;
        attach_workbench(&mut inst.cdp, deadline)?;
        trace("workbench ready");
        Ok(())
    }
}

impl Drop for VsCodeReplayer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The scope's cgroup directory, if `pid` is in a scope named after `unit`
/// (never brain's own cgroup: `cgroup.kill` there would kill brain).
fn scope_cgroup(pid: u32, unit: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    if !rel.ends_with(&format!("/{unit}.scope")) {
        return None;
    }
    let p = PathBuf::from(format!("/sys/fs/cgroup{rel}"));
    p.join("cgroup.kill").exists().then_some(p)
}

/// Kills an instance left behind by a previous `brain run` that was killed
/// before it could shut VS Code down (identified by its user-data-dir).
fn kill_stale(work_dir: &Path) {
    let f = work_dir.join("instance.json");
    let Some(v) = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return;
    };
    let _ = std::fs::remove_file(&f);
    let (Some(pid), Some(udd)) = (
        v.get("pid").and_then(Value::as_i64),
        v.get("user_data_dir").and_then(Value::as_str),
    ) else {
        return;
    };
    let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains(udd))
        .unwrap_or(false);
    if ours {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            && let Some(rel) = text.lines().find_map(|l| l.strip_prefix("0::"))
            && rel.contains("/brain-vscode-")
            && rel.ends_with(".scope")
        {
            let _ = std::fs::write(format!("/sys/fs/cgroup{rel}/cgroup.kill"), "1");
        }
        // SAFETY: plain syscall on a process verified above to be ours.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}

/// Attaches to the workbench page and waits until it can take input.
fn attach_workbench(cdp: &mut Cdp, deadline: Instant) -> Result<()> {
    let target_id = loop {
        let r = cdp.call_browser("Target.getTargets", json!({}), deadline)?;
        let found = r
            .get("targetInfos")
            .and_then(Value::as_array)
            .and_then(|ts| {
                ts.iter().find(|t| {
                    t.get("type").and_then(Value::as_str) == Some("page")
                        && t.get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|u| u.contains("workbench.html"))
                })
            })
            .and_then(|t| t.get("targetId").and_then(Value::as_str))
            .map(str::to_string);
        if let Some(id) = found {
            break id;
        }
        sleep_until(deadline, Duration::from_millis(100))?;
    };
    let r = cdp.call_browser(
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
        deadline,
    )?;
    let session = r
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("attachToTarget returned no session"))?;
    cdp.session = Some(session.to_string());
    // Headless windows never have OS focus; without emulation the editor's
    // focus tracking (and so its keybinding contexts) would be off.
    cdp.call(
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
        deadline,
    )?;
    loop {
        let ready = cdp.eval(
            "document.readyState === 'complete' && !!document.querySelector('.monaco-workbench .part.editor')",
            deadline,
        )?;
        if ready == Value::Bool(true) {
            return Ok(());
        }
        sleep_until(deadline, Duration::from_millis(100))?;
    }
}

/// Step timing on stderr when `BRAIN_VSCODE_TRACE` is set (diagnostics).
fn trace(step: &str) {
    use std::sync::OnceLock;
    static T0: OnceLock<Option<Instant>> = OnceLock::new();
    if let Some(t0) =
        T0.get_or_init(|| std::env::var_os("BRAIN_VSCODE_TRACE").map(|_| Instant::now()))
    {
        eprintln!("brain: vscode: {step} at {:?}", t0.elapsed());
    }
}

fn title(cdp: &mut Cdp, deadline: Instant) -> Result<String> {
    Ok(cdp
        .eval("document.title", deadline)?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Closes editors left over from earlier jobs (they are saved, so revert is
/// only a safety net against a confirmation dialog).
fn close_job_editors(cdp: &mut Cdp, deadline: Instant) -> Result<()> {
    send_cmd(cdp, Cmd::CloseQuickOpen, deadline)?;
    for _ in 0..10 {
        if !title(cdp, deadline)?.contains("/jobs/") {
            return Ok(());
        }
        send_cmd(cdp, Cmd::RevertAndClose, deadline)?;
        sleep_until(deadline, POLL)?;
    }
    bail!("could not close the previous replay's editor")
}

/// Opens `path` through quick open (VS Code accepts an absolute path there)
/// and waits until it is the focused, active editor.
fn open_file(
    cdp: &mut Cdp,
    path: &Path,
    file_name: &str,
    suffix: &str,
    deadline: Instant,
) -> Result<()> {
    let name_js = serde_json::to_string(file_name)?;
    // The result row shows the parent folder ("…/jobs/<n>"), which tells the
    // job's file apart from an earlier job's file with the same name.
    let dir_js = serde_json::to_string(suffix.rsplit_once('/').map_or(suffix, |(d, _)| d))?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        if attempt > 8 {
            bail!("could not open {} in VS Code", path.display());
        }
        send_cmd(cdp, Cmd::CloseQuickOpen, deadline)?;
        send_cmd(cdp, Cmd::FocusEditor, deadline)?;
        send_cmd(cdp, Cmd::QuickOpen, deadline)?;
        // Right after startup the workbench may not take keybindings yet:
        // retry soon rather than wait long.
        if !wait_for(
            cdp,
            "!!(document.activeElement && document.activeElement.closest('.quick-input-widget'))",
            Duration::from_secs(1),
            deadline,
        )? {
            trace("quick open did not take focus; retrying");
            continue;
        }
        trace("quick open focused");
        cdp.call(
            "Input.insertText",
            json!({ "text": path.to_string_lossy() }),
            deadline,
        )?;
        let has_row = format!(
            "Array.from(document.querySelectorAll('.quick-input-widget .monaco-list-row'))\
             .some(r => {{ const l = r.getAttribute('aria-label') || r.textContent || ''; \
             return l.includes({name_js}) && l.includes({dir_js}); }})"
        );
        if !wait_for(cdp, &has_row, Duration::from_secs(3), deadline)? {
            if let Ok(rows) = cdp.eval(
                "Array.from(document.querySelectorAll('.quick-input-widget .monaco-list-row'))\
                 .map(r => r.getAttribute('aria-label') || r.textContent).join(' | ')",
                deadline,
            ) {
                trace(&format!("no matching quick open result ({rows}); retrying"));
            }
            continue;
        }
        trace("quick open result shown");
        send_key(
            cdp,
            &keys::map_key("Enter", &[]).ok_or_else(|| anyhow!("no Enter"))?,
            deadline,
        )?;
        let opened = format!(
            "document.title.endsWith({}) && !!(document.activeElement && \
             document.activeElement.closest('.monaco-editor') && \
             !document.activeElement.closest('.quick-input-widget'))",
            serde_json::to_string(suffix)?
        );
        if wait_for(cdp, &opened, Duration::from_secs(5), deadline)? {
            return Ok(());
        }
        trace("file did not open; retrying");
        send_cmd(cdp, Cmd::CloseQuickOpen, deadline)?;
    }
}

/// Polls a boolean page expression until it is true (`Ok(true)`), `within`
/// elapses (`Ok(false)`), or the replay deadline passes (`Err`).
fn wait_for(cdp: &mut Cdp, expr: &str, within: Duration, deadline: Instant) -> Result<bool> {
    let until = (Instant::now() + within).min(deadline);
    loop {
        if cdp.eval(expr, deadline)? == Value::Bool(true) {
            return Ok(true);
        }
        if Instant::now() >= until {
            if Instant::now() >= deadline {
                bail!("timed out waiting for VS Code");
            }
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

fn sleep_until(deadline: Instant, d: Duration) -> Result<()> {
    let now = Instant::now();
    if now + d >= deadline {
        bail!("timed out waiting for VS Code");
    }
    std::thread::sleep(d);
    Ok(())
}

/// Commands per pipelined batch: small enough never to fill the pipe.
const BATCH: usize = 64;
/// Settle time after a shortcut, which may open a widget (find, go to line)
/// that the following keys are typed into.
const SHORTCUT_SETTLE: Duration = Duration::from_millis(30);

/// Sends the planned actions. Runs of plain typing are pipelined; every
/// shortcut is sent on its own, after the keys before it have been handled
/// and followed by a short settle, so widgets it opens are ready for the keys
/// that follow, as they were for the user.
fn replay_actions(cdp: &mut Cdp, actions: &[Action], deadline: Instant) -> Result<()> {
    let mut batch: Vec<(&'static str, Value)> = Vec::with_capacity(BATCH + 2);
    for action in actions {
        let barrier = match action {
            Action::Key(k) => k.modifiers & (MOD_CTRL | MOD_ALT) != 0,
            Action::Insert(_) => false,
            Action::CursorToEnd => true,
        };
        if barrier {
            cdp.pipeline(std::mem::take(&mut batch), deadline)?;
        }
        match action {
            Action::Key(k) => batch.extend(key_events(k)),
            Action::Insert(text) => batch.push(("Input.insertText", json!({ "text": text }))),
            Action::CursorToEnd => {
                for cmd in [Cmd::CloseQuickOpen, Cmd::FocusEditor, Cmd::CursorBottom] {
                    batch.extend(key_events(&cmd_key(cmd)));
                }
            }
        }
        if barrier {
            cdp.pipeline(std::mem::take(&mut batch), deadline)?;
            std::thread::sleep(SHORTCUT_SETTLE);
        } else if batch.len() >= BATCH {
            cdp.pipeline(std::mem::take(&mut batch), deadline)?;
        }
    }
    cdp.pipeline(batch, deadline)
}

/// `Input.dispatchKeyEvent` down and up events for one key.
fn key_events(k: &CdpKey) -> [(&'static str, Value); 2] {
    let mut down = json!({
        "type": if k.text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": k.key,
        "code": k.code,
        "windowsVirtualKeyCode": k.vk,
        "modifiers": k.modifiers,
    });
    if let Some(t) = &k.text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let up = json!({
        "type": "keyUp",
        "key": k.key,
        "code": k.code,
        "windowsVirtualKeyCode": k.vk,
        "modifiers": k.modifiers,
    });
    [
        ("Input.dispatchKeyEvent", down),
        ("Input.dispatchKeyEvent", up),
    ]
}

fn send_key(cdp: &mut Cdp, k: &CdpKey, deadline: Instant) -> Result<()> {
    cdp.pipeline(key_events(k).into(), deadline)
}

fn cmd_key(cmd: Cmd) -> CdpKey {
    let (code, _, vk) = BINDINGS[cmd as usize];
    CdpKey {
        key: code.to_string(),
        code,
        vk,
        text: None,
        modifiers: MOD_CTRL | MOD_ALT | MOD_SHIFT,
    }
}

fn send_cmd(cdp: &mut Cdp, cmd: Cmd, deadline: Instant) -> Result<()> {
    send_key(cdp, &cmd_key(cmd), deadline)
}

/// A safe basename for the scratch copy; keeps the real name when possible
/// so VS Code's language detection (by extension or name) matches.
fn sanitize_file_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        "untitled.txt".to_string()
    } else {
        base.to_string()
    }
}

fn code_binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("BRAIN_VSCODE_BIN").map(PathBuf::from) {
        return Ok(p);
    }
    for c in [
        "/usr/share/code/code",
        "/usr/lib/code/code",
        "/opt/visual-studio-code/code",
        "/snap/code/current/usr/share/code/code",
    ] {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("VS Code's Electron binary not found (set BRAIN_VSCODE_BIN)")
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: valid out-pointer for two fds.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2");
    }
    // SAFETY: freshly created fds owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn dup_above(fd: &OwnedFd, min: i32) -> Result<OwnedFd> {
    // SAFETY: plain fcntl on a valid fd.
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, min) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_DUPFD_CLOEXEC)");
    }
    // SAFETY: fresh fd.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

// ---------------------------------------------------------------------------
// Sandbox configuration
// ---------------------------------------------------------------------------

fn user_vscode_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("Code/User"))
}

/// Writes the sandbox's settings.json, keybindings.json and snippets/.
fn prepare_user_config(dst: &Path) -> Result<()> {
    let src = user_vscode_dir();
    let read = |name: &str| -> Option<String> {
        src.as_ref()
            .and_then(|s| std::fs::read_to_string(s.join(name)).ok())
    };

    let mut settings = read("settings.json")
        .and_then(|t| serde_json::from_str::<Value>(&strip_jsonc(&t)).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let (Some(obj), Value::Object(over)) = (settings.as_object_mut(), setting_overrides()) {
        obj.extend(over);
    }
    std::fs::write(
        dst.join("settings.json"),
        serde_json::to_vec_pretty(&settings)?,
    )?;

    let mut bindings: Vec<Value> = read("keybindings.json")
        .and_then(|t| serde_json::from_str::<Value>(&strip_jsonc(&t)).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for (key, command, _) in BINDINGS {
        bindings.push(
            json!({ "key": format!("ctrl+alt+shift+{}", key.to_lowercase()), "command": command }),
        );
    }
    std::fs::write(
        dst.join("keybindings.json"),
        serde_json::to_vec_pretty(&bindings)?,
    )?;

    let snippets = dst.join("snippets");
    let _ = std::fs::remove_dir_all(&snippets);
    if let Some(s) = &src
        && s.join("snippets").is_dir()
    {
        copy_tree(&s.join("snippets"), &snippets, None)?;
    }
    Ok(())
}

/// The sandbox's extensions dir: a copy of the user's if under the size cap,
/// refreshed when the source changes. `None` = run with extensions disabled.
fn prepare_extensions(work_dir: &Path, deadline: Instant) -> Option<PathBuf> {
    let src = PathBuf::from(std::env::var_os("HOME")?).join(".vscode/extensions");
    if !src.is_dir() {
        return None;
    }
    let size = tree_size(&src, MAX_EXTENSIONS_BYTES)?;
    let stamp = {
        let meta = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        };
        format!(
            "{}:{}:{}",
            meta(&src),
            meta(&src.join("extensions.json")),
            size
        )
    };
    let dst = work_dir.join("extensions");
    let stamp_file = work_dir.join("extensions.stamp");
    if std::fs::read_to_string(&stamp_file).ok().as_deref() == Some(stamp.as_str()) && dst.is_dir()
    {
        return Some(dst);
    }
    let tmp = work_dir.join("extensions.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    if copy_tree(&src, &tmp, Some(deadline)).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return None;
    }
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::rename(&tmp, &dst).ok()?;
    std::fs::write(&stamp_file, stamp).ok()?;
    Some(dst)
}

/// Total size of regular files under `root`, or `None` once it exceeds `cap`.
fn tree_size(root: &Path, cap: u64) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let ft = e.file_type().ok()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().ok()?.len();
                if total > cap {
                    return None;
                }
            }
        }
    }
    Some(total)
}

/// Recursive copy; symlinks are recreated as symlinks, never followed.
fn copy_tree(src: &Path, dst: &Path, deadline: Option<Instant>) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            bail!("timed out copying {}", src.display());
        }
        let ft = e.file_type()?;
        let to = dst.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &to, deadline)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &to)?;
        } else if ft.is_file() {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// JSON with comments (VS Code's settings format) → strict JSON: drops `//`
/// and `/* */` comments outside strings and trailing commas.
fn strip_jsonc(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b',' => {
                // Drop the comma if only whitespace/comments lead to } or ].
                let mut j = i + 1;
                while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                if !matches!(b.get(j), Some(b'}') | Some(b']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_is_stripped() {
        let src = r#"{
            // comment
            "a": "x // not a comment", /* block */
            "b": [1, 2,],
            "c": "q\"uote",
        }"#;
        let v: Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"], "x // not a comment");
        assert_eq!(v["b"], json!([1, 2]));
        assert_eq!(v["c"], "q\"uote");
    }

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(sanitize_file_name("main.rs"), "main.rs");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name(""), "untitled.txt");
        assert_eq!(sanitize_file_name(".."), "untitled.txt");
        assert_eq!(sanitize_file_name("Makefile"), "Makefile");
    }

    #[test]
    fn user_config_gets_overrides_and_private_bindings() {
        let tmp = tempfile::tempdir().unwrap();
        prepare_user_config(tmp.path()).unwrap();
        let s: Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(s["files.autoSave"], "off");
        assert_eq!(s["window.title"], "${dirty}${activeEditorLong}");
        let k: Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("keybindings.json")).unwrap())
                .unwrap();
        let arr = k.as_array().unwrap();
        assert!(
            arr.iter().any(|b| b["key"] == "ctrl+alt+shift+f15"
                && b["command"] == "workbench.action.files.save")
        );
    }
}
