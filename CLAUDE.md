# CLAUDE.md — instructions for coding agents working on `brain`

`brain` is a Rust CLI for the Endpoint "Shadow Assistant" home assignment
(`Endpoint Home Assignment.pdf`, not committed). One binary, two commands:
`brain run` captures keystrokes + focus and stores them; `brain query`
(`search`, `text`) answers questions while `run` keeps going.

Original design doc: https://claude.ai/artifact/UTxCHmXHeSBaQASSNNiSP9
(copy in `ai-logs/design-doc.md`). `PLAN.md` is the in-repo research and
architecture write-up of the code as built, marks where it differs from the
design, and must stay consistent with the code.

## Target platform

Ubuntu on Wayland with **KDE Plasma 6** (KWin) or **GNOME** (stock
Ubuntu), behind the platform layer (`src/capture/platform.rs`,
`src/capture/linux/desktop.rs`). Keys come from evdev
(`/dev/input/event*`, user in the `input` group), resolved with
libxkbcommon (`libxkbcommon.so.0`, linked by file name, so no `-dev`
package is needed). Focus comes from inside the compositor: a KWin script
brain loads on KDE, brain's GNOME Shell extension
(`packaging/gnome-extension/`, installed by `make install`) on GNOME.

Tests that run by default must not need hardware, a desktop or privileges:
use `brain run --synthetic RATE` (hidden flag) to drive the real pipeline.
Only the opt-in (`#[ignore]`) tests use `/dev/uinput`, Kate, VS Code or a
GNOME Shell.

## Hard requirements (from the assignment) — never regress these

- Capture is invisible: no input lag (we never grab devices), **≤ 1 % average
  CPU while typing, ≤ 32 MB RSS**, keeps working after sleep/wake.
- `kill -9` of `run` at any moment + restart ⇒ no corruption
  (`tests/crash_safety.rs` must stay green).
- `query` works while `run` writes and always sees a consistent state.
- Both commands take `--data-dir` (default `./brain-data`) and exit non-zero
  with a message on stderr on error. Query output is JSON on stdout, with
  times in local time (the log and database store UTC).

## Decisions already made by the owner — do not re-litigate

- `brain query text` reconstructs "stupidly": the keys typed in the time
  range, per app and window title, run through the tier 0/1 adapters from
  an empty text (no sessions involved); output `windows` + joined `text`.
  It replaced the earlier raw-keystroke output at the owner's request.
  Range: default last 10 min, `--at T` = the 10 minutes ending at T, or
  `--from/--to`. Times accept relative (`-2h`), local and RFC 3339 forms.
  `--app` may be part of the id; `text` resolves it to exactly one app and
  reports the full id.
- Sessions: keyed by (app, normalized window title); close after 30 s idle
  in that context or on Enter in single-line contexts; focus loss alone
  never closes a session; a title change of the typed window (a `renamed`
  focus event) shortly after a keystroke keeps the session. With
  `--vscode-replay`, every VS Code key (not only text edits) opens or
  extends a session, and a session of shortcuts only (Ctrl+D, Ctrl+S) is
  kept for the saved file's diff, or removed when there is none.
- The mouse is not captured (owner's call): cursor moves made with the mouse
  are a documented limitation, not something to work around.
- Reconstruction tiers: 0 = generic buffer replay, 1 = per-app rules,
  2 = VS Code from the real editor, **opt-in** via `brain run
  --vscode-replay`: a git-style diff (3 lines of context) when the file was
  saved after the session, otherwise a best-effort replay; tier 0/1 text
  stays when neither works. Unsaved new files are not supported. Sessions, reconstruction and indexing are built
  before `brain query` (owner's call, changing the original stage order).
- Platforms: capture sits behind `Platform` / `Desktop` interfaces; only
  KDE Plasma and GNOME on Ubuntu are supported for now. Other desktops get
  keys under `unknown` with a warning that the lock screen may not be
  detected (owner's choice: warn and continue, not refuse). On GNOME the
  extension's `Clipboard` method may answer anyone, like Klipper (owner's
  call). Tier 2 needs nothing desktop-specific.
- Search is substring matching (FTS5 trigram, case- and accent-insensitive),
  not the design's porter stemming: `run` must find "rerun", `בית` "בבית".
- Sessions and their text must stay a pure function of the log (tier 2
  aside, which depends on files on disk): the crash test compares the live
  index with a fresh rebuild.
- Work went through stages (capture with sessions and indexing, query,
  end-to-end verification), each approved by the owner. Keep the habit for
  new work: build, run, test it yourself, report, then **stop and wait for
  explicit approval** before starting the next piece.

## Code map

```
src/main.rs              dispatches the commands, error → stderr + exit 1
src/cli.rs               the command-line definition (clap); its /// is --help
src/capture/mod.rs       `run` orchestration: platform, ingest thread, signals
src/capture/platform.rs  the platform interfaces (Platform, Capture) and their contract
src/capture/recorder.rs  the sink every source writes to: stamps focus, appends to
                         the log, clipboard on paste, lock pause
src/capture/linux/       the Linux platform:
  mod.rs                 wires evdev keys, logind sleep and the desktop backend
  input.rs               poll loop over keyboards (evdev), hotplug, EVIOCSMASK
  keys.rs                pure key processing: xkb resolution + repeat synthesis
  xkb.rs                 libxkbcommon FFI wrapper
  desktop.rs             the Desktop interface, detection, unsupported fallback
  kde.rs, kwin_focus.js  KDE Plasma: KWin focus, Klipper, lock, layouts, repeat
  gnome.rs               GNOME: the extension's signals, its clipboard, lock,
                         GSettings layouts and repeat
  logind.rs, layout.rs   sleep/wake; system keyboard layout (env, localed)
  dbus.rs                signal watching shared by the backends
  synthetic.rs           synthetic input source for tests/benchmarks
src/eventlog.rs          append-only JSONL log: torn-tail repair, line reader
src/store.rs             SQLite schema (WAL, FTS5), helpers
src/ingest.rs            log → SQLite, offset advanced in the same transaction
src/sessions.rs          session rules, open/close, live text, crash recovery
src/query.rs             `query search|sessions|apps|text`, time windows, JSON output
src/reconstruct/         buffer.rs tier 0 · apps.rs + editor.rs tier 1 ·
                         tier2.rs + vscode/ tier 2 (VS Code diff / replay)
src/event.rs, timefmt.rs event format, timestamp parsing/formatting
chat/                    brain-chat (Python): Claude over `brain query` tools,
                         redaction, the question loop; `make chat-test`
tests/                   integration tests that drive the real binary;
                         tests/common: `brain run` guard, uinput keyboard,
                         KDE helpers
```

## Working rules

- **No backward compatibility.** This is the first robust version, still
  being built: change the log format, the schema or the CLI freely, with no
  migration, compatibility shims or legacy-format handling. Test data
  directories can be deleted and recreated.
- `cargo fmt`, `cargo clippy --all-targets` (zero warnings) and `cargo test`
  before every commit. Every public item has a `///` doc saying what it
  means (units, where a value comes from), not restating its name:
  `#![warn(missing_docs)]` in lib.rs makes a missing one a warning.
  Every `unsafe` block has its own `// SAFETY:` comment. Run `cargo test --release --test crash_safety` when
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
  vscode_replay --test tier2_e2e -- --ignored --test-threads=1`
  (`tier2_e2e`'s saved-file tests need no VS Code and run by default).
  `BRAIN_VSCODE_VISIBLE=1` shows the replay instance as a normal window
  instead of headless (also for `brain run --vscode-replay`).
- Every `brain run` a test starts goes through `tests/common::spawn_brain`
  (or `start_brain`): the guard stops it even when the test fails, and
  tests end with `assert_no_brain_on(data)`. No test may leave a process or
  a file behind.
- `tests/gnome_e2e.rs` (`#[ignore]`) starts a private headless GNOME Shell
  in its own D-Bus session, home and systemd scope (never the user's
  desktop); its private shell must not get a login session, so it cannot
  touch the real session's lock state. Run it after changing gnome.rs or
  the extension.
- brain-chat (`chat/`) only reads through `brain query`; it never touches
  the data directory itself. Its offline tests (`make chat-test`) must not
  call the API; `chat/live_check.py` does, costs money, and runs only with
  the owner's OK. Keep the redaction in front of every tool result.
- `brain run`s on separate data directories are supported side by side
  (`tests/two_instances.rs`); one data directory allows one run.
- `tests/repeat_e2e.rs`, `tests/real_keyboard_e2e.rs` and the desktop test
  in `tests/two_instances.rs` (`#[ignore]`) type into real Kate windows
  through /dev/uinput: they need an unlocked KDE session,
  `scripts/setup-test-permissions.sh`, and nobody touching the keyboard or
  mouse while they run. They only type while brain's log shows the
  intended window focused.
- The input loop must deliver key transitions before emitting the repeats
  due at the same time (`keys::tests::ticking_past_an_unread_release_adds_repeats`).
- Never change desktop session state you did not set up (lock/unlock,
  suspend); check it first and restore it.
