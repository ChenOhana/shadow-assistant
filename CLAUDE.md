# CLAUDE.md — instructions for coding agents working on `brain`

`brain` is a Rust CLI for the Endpoint "Shadow Assistant" home assignment
(`Endpoint Home Assignment.pdf`, not committed). One binary, two commands:
`brain run` captures keystrokes + focus and stores them; `brain query`
(`search`, `text`) answers questions while `run` keeps going.

Design doc (source of truth for architecture):
https://claude.ai/artifact/UTxCHmXHeSBaQASSNNiSP9 — `PLAN.md` is the in-repo
research/architecture write-up and must stay consistent with the code.

## Target platform

Ubuntu, **KDE Plasma 6 on Wayland** (KWin). Not GNOME. Keys come from evdev
(`/dev/input/event*`, user in the `input` group), resolved with libxkbcommon
(`libxkbcommon.so.0`, linked by file name, so no `-dev` package is needed).
Focus comes from a KWin script calling back over D-Bus. `/dev/uinput` is
root-only on the dev machine: tests must not depend on it. Use
`brain run --synthetic RATE` (hidden flag) to drive the real pipeline without
hardware.

## Hard requirements (from the assignment) — never regress these

- Capture is invisible: no input lag (we never grab devices), **≤ 1 % average
  CPU while typing, ≤ 32 MB RSS**, keeps working after sleep/wake.
- `kill -9` of `run` at any moment + restart ⇒ no corruption
  (`tests/crash_safety.rs` must stay green).
- `query` works while `run` writes and always sees a consistent state.
- Both commands take `--data-dir` (default `./brain-data`) and exit non-zero
  with a message on stderr on error. Query output is JSON on stdout.

## Decisions already made by the owner — do not re-litigate

- `brain query text` returns the **raw decoded keystroke stream** (no
  backspace/cursor editing applied) over a window: default last 10 min,
  `--at T` = 10-min window ending at T, or `--from/--to`.
- Sessions: keyed by (app, window); close after 30 s idle in that context or
  on Enter in single-line contexts; focus loss alone never closes a session.
- Reconstruction tiers: 0 = generic buffer replay, 1 = per-app rules,
  2 = VS Code real-app replay, **opt-in** via `brain run --vscode-replay`,
  falling back to tier 0. Sessions, reconstruction and indexing are built
  before `brain query` (owner's call, changing the original stage order).
- Search is substring matching (FTS5 trigram, case- and accent-insensitive),
  not the design's porter stemming: `run` must find "rerun", `בית` "בבית".
- Sessions and their text must stay a pure function of the log: the crash
  test compares the live index with a fresh rebuild.
- Work proceeds in 4 stages (capture, query, reconstruction, E2E tests).
  After each stage: build, run, test it yourself, report, then **stop and
  wait for explicit approval** before the next stage.

## Code map

```
src/main.rs              CLI (clap), error → stderr + exit 1
src/capture/mod.rs       `run` orchestration: sources, ingest thread, signals
src/capture/input.rs     poll loop over evdev devices, hotplug, EVIOCSMASK
src/capture/keys.rs      pure key processing: xkb resolution + repeat synthesis
src/capture/xkb.rs       libxkbcommon FFI wrapper
src/capture/recorder.rs  stamps focus, appends to log, clipboard on paste, lock pause
src/capture/desktop.rs   D-Bus: KWin focus script, Klipper, lock, sleep, layouts
src/capture/synthetic.rs synthetic input source for tests/benchmarks
src/eventlog.rs          append-only JSONL log: torn-tail repair, line reader
src/store.rs             SQLite schema (WAL, FTS5), helpers
src/ingest.rs            log → SQLite, offset advanced in the same transaction
src/sessions.rs          session rules, open/close, live text, crash recovery
src/reconstruct/         buffer.rs tier 0 · apps.rs + editor.rs tier 1 ·
                         tier2.rs + vscode/ tier 2 (VS Code replay, opt-in)
src/event.rs, timefmt.rs event format, timestamp parsing/formatting
tests/                   integration tests that drive the real binary
```

## Working rules

- **No backward compatibility.** This is the first robust version, still
  being built: change the log format, the schema or the CLI freely, with no
  migration, compatibility shims or legacy-format handling. Test data
  directories can be deleted and recreated.

- `cargo fmt`, `cargo clippy --all-targets` (zero warnings) and `cargo test`
  before every commit. Run `cargo test --release --test crash_safety` when
  touching the log, ingest or store.
- Keep the hot path (input thread) free of allocation-heavy work, blocking
  I/O other than the single log `write(2)`, and SQLite.
- The log is the ground truth; SQLite is derived and rebuildable. Never put
  data only in SQLite.
- Commit messages per https://cbea.ms/git-commit/ (imperative subject
  ≤ 50 chars, blank line, body wrapped at 72 explaining what and why).
  Commit at meaningful checkpoints.
- Prefer verifying behaviour by running the binary over reasoning about it;
  report exactly what was tested and what was not.
- Tests that need VS Code are `#[ignore]`: `cargo test --release --test
  vscode_replay --test tier2_e2e -- --ignored --test-threads=1`.
- Never change desktop session state you did not set up (lock/unlock,
  suspend); check it first and restore it.
