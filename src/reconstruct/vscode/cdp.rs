//! A minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//!
//! The browser reads NUL-terminated JSON commands from its fd 3 and writes
//! NUL-terminated JSON responses and events to its fd 4. A reader thread
//! splits the stream into messages; [`Cdp::call`] writes a command and waits,
//! with a deadline, for the response carrying its id. Events are discarded.
//! When the browser dies the pipe hits EOF, the reader exits and every
//! pending or later call fails immediately.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

/// Largest single message accepted from the browser. Our responses are small
/// (key acks, short `Runtime.evaluate` results); a runaway message must not
/// grow `brain run`'s memory without bound.
const MAX_MESSAGE: usize = 8 << 20;
/// How often a waiting call checks whether the instance was killed.
const KILL_CHECK: Duration = Duration::from_millis(50);

pub(crate) struct Cdp {
    tx: File,
    rx: Receiver<Value>,
    next_id: u64,
    /// Flattened session id of the attached page target.
    pub session: Option<String>,
    reader: Option<JoinHandle<()>>,
    /// Set by a kill from another thread: every call fails at once instead
    /// of waiting for the pipe to close (which needs every process holding
    /// it to be gone).
    killed: Arc<AtomicBool>,
}

impl Cdp {
    /// `to_browser` is our end of the browser's fd 3, `from_browser` of its fd 4.
    pub fn new(
        to_browser: OwnedFd,
        from_browser: OwnedFd,
        killed: Arc<AtomicBool>,
    ) -> Result<Self> {
        let (tx_msg, rx) = channel();
        let mut input = File::from(from_browser);
        let reader = std::thread::Builder::new()
            .name("brain-vscode-cdp".into())
            .spawn(move || {
                let mut buf: Vec<u8> = Vec::with_capacity(64 << 10);
                let mut chunk = vec![0u8; 64 << 10];
                loop {
                    let n = match input.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(pos) = buf.iter().position(|&b| b == 0) {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[..pos])
                            && v.get("id").is_some()
                            && tx_msg.send(v).is_err()
                        {
                            return;
                        }
                        buf.drain(..=pos);
                    }
                    if buf.len() > MAX_MESSAGE {
                        return; // treat as a broken connection
                    }
                }
            })
            .context("spawning CDP reader thread")?;
        Ok(Self {
            tx: File::from(to_browser),
            rx,
            next_id: 0,
            session: None,
            reader: Some(reader),
            killed,
        })
    }

    /// Sends a command to the attached page (or to the browser if no page is
    /// attached yet) and returns its `result`.
    pub fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        let session = self.session.clone();
        self.call_in(session.as_deref(), method, params, deadline)
    }

    /// Sends a browser-level command (no session).
    pub fn call_browser(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.call_in(None, method, params, deadline)
    }

    fn call_in(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let id = self.send_in(session, method, params)?;
        self.wait_ids(id, id, method, deadline)
    }

    /// Sends several commands to the attached page without waiting between
    /// them, then waits for all of them. The browser runs a session's commands
    /// in order, so this keeps their order while paying one round trip (and,
    /// for input, about one frame) per batch instead of per command.
    pub fn pipeline(&mut self, cmds: Vec<(&'static str, Value)>, deadline: Instant) -> Result<()> {
        let Some(first_method) = cmds.first().map(|c| c.0) else {
            return Ok(());
        };
        let session = self.session.clone();
        let mut ids = Vec::with_capacity(cmds.len());
        for (method, params) in cmds {
            ids.push(self.send_in(session.as_deref(), method, params)?);
        }
        let (first, last) = (ids[0], ids[ids.len() - 1]);
        self.wait_ids(first, last, first_method, deadline)?;
        Ok(())
    }

    fn send_in(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<u64> {
        if self.killed.load(Ordering::SeqCst) {
            bail!("VS Code was stopped before {method}");
        }
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        // Commands are small and batches stay far below the pipe buffer, so
        // writes do not block while the browser reads; a dead browser gives
        // EPIPE (SIGPIPE is ignored by Rust's runtime). A browser that is
        // alive but stuck is killed by the replay watchdog, which unblocks us.
        self.tx
            .write_all(&bytes)
            .map_err(|e| anyhow!("CDP write failed ({method}): {e}"))?;
        Ok(id)
    }

    /// Waits for the response to `last`; fails if any response with an id in
    /// `first..=last` is an error. Returns `last`'s result.
    fn wait_ids(
        &mut self,
        first: u64,
        last: u64,
        method: &str,
        deadline: Instant,
    ) -> Result<Value> {
        loop {
            if self.killed.load(Ordering::SeqCst) {
                bail!("VS Code was stopped during {method}");
            }
            let now = Instant::now();
            if now >= deadline {
                bail!("CDP call {method} timed out");
            }
            match self.rx.recv_timeout((deadline - now).min(KILL_CHECK)) {
                Ok(v) => {
                    let Some(id) = v.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    if !(first..=last).contains(&id) {
                        continue; // a late response to an earlier, timed-out call
                    }
                    if let Some(err) = v.get("error") {
                        bail!("CDP {method} failed: {err}");
                    }
                    if id == last {
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("VS Code closed the DevTools pipe during {method}")
                }
            }
        }
    }

    /// Evaluates a JavaScript expression in the page and returns its value.
    pub fn eval(&mut self, expr: &str, deadline: Instant) -> Result<Value> {
        let r = self.call(
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": true }),
            deadline,
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("page script failed: {ex}");
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        // The reader exits on EOF once the browser is gone; do not block on it
        // if the browser is still alive (the owner kills it first).
        if let Some(h) = self.reader.take()
            && h.is_finished()
        {
            let _ = h.join();
        }
    }
}
