//! The resource budget from the assignment: `brain run` uses at most 1 %
//! average CPU while typing and at most 32 MB of memory.
//!
//! Types at human speed (8 keys/s, about 95 words per minute) through the
//! real pipeline (key processing, log, ingest, sessions, index) for 30 s,
//! with `brain query` running every few seconds as it would in use, and
//! measures `brain run`'s own CPU time and peak resident memory from /proc.
//! The synthetic source replaces only the evdev read, which is one `read`
//! per key.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KEYS_PER_SECOND: u32 = 8;
const MEASURE: Duration = Duration::from_secs(30);
const MAX_CPU_PERCENT: f64 = 1.0;
const MAX_RSS_KB: u64 = 32 * 1024;

/// utime + stime of `pid`, in clock ticks.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // Fields after the parenthesised command name; utime and stime are the
    // 14th and 15th fields overall.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// Peak resident set size (VmHWM) of `pid`, in kB.
fn peak_rss_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[test]
fn cpu_and_memory_stay_within_budget_while_typing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let mut run = Command::new(env!("CARGO_BIN_EXE_brain"))
        .args([
            "run",
            "--synthetic",
            &KEYS_PER_SECOND.to_string(),
            "--data-dir",
        ])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = run.id();
    std::thread::sleep(Duration::from_secs(2)); // past startup

    // SAFETY: sysconf is always safe to call.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let start_ticks = cpu_ticks(pid);
    let start = Instant::now();
    let mut queries = 0;
    while start.elapsed() < MEASURE {
        std::thread::sleep(Duration::from_secs(3));
        let out = Command::new(env!("CARGO_BIN_EXE_brain"))
            .args(["query", "search", "migration", "--data-dir"])
            .arg(&dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        queries += 1;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu = (cpu_ticks(pid) - start_ticks) as f64 / hz / elapsed * 100.0;
    let rss = peak_rss_kb(pid);
    // SAFETY: SIGTERM to our own child.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    assert!(run.wait().unwrap().success());

    // The typing really went through the whole pipeline.
    let log = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    let keys = log
        .lines()
        .filter(|l| l.contains("\"type\":\"key\""))
        .count();
    eprintln!(
        "{keys} keys in {elapsed:.0}s with {queries} queries: \
         CPU {cpu:.3}% (budget {MAX_CPU_PERCENT}%), peak RSS {} kB (budget {MAX_RSS_KB} kB)",
        rss
    );
    assert!(
        keys as f64 > KEYS_PER_SECOND as f64 * elapsed * 0.8,
        "only {keys} keys"
    );
    assert!(cpu <= MAX_CPU_PERCENT, "CPU {cpu:.3}% over budget");
    assert!(rss <= MAX_RSS_KB, "peak RSS {rss} kB over budget");
}
