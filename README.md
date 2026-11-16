# brain — a shadow assistant for Linux

`brain` runs on your computer, follows what you type in each application and
lets you search it later.

```
brain run                      # capture keystrokes + focus changes, store them
brain query search "<words>"   # (stage 2) full-text search over writing sessions
brain query text --app A       # (stage 2) the raw typed stream of an app
```

> **Status: capture, sessions, reconstruction and indexing are done;
> `brain query` is next.** `brain run` captures, groups what you type into
> writing sessions, reconstructs each session's text and indexes it for
> full-text search. You can inspect the result in `brain.db` (see below).

## Supported OS

Ubuntu (tested on 26.04) with **KDE Plasma 6 on Wayland** (KWin). Key capture
works on any Linux. Focus tracking needs KWin (Wayland or X11). On other
desktops, keys are recorded under the app `unknown`.

## Build

```sh
# Rust toolchain (https://rustup.rs), then:
cargo build --release
# binary: target/release/brain

# or build and install to /usr/bin/brain (asks for sudo for the copy only):
make
```

SQLite is bundled. libxkbcommon is loaded from the runtime library
`libxkbcommon.so.0` that every Wayland desktop ships, so no `-dev`
packages are needed.

## Permissions

`brain` reads keyboards directly from `/dev/input/event*` (read-only,
never grabbed, so it cannot add input lag). Your user must be in the `input`
group:

```sh
sudo usermod -aG input "$USER"   # then log out and back in
```

Note that the `input` group can read every input device. That is the price of
display-server-independent capture (see PLAN.md for the alternatives).

## Run

```sh
target/release/brain run --data-dir ~/brain-data
```

`run` stops cleanly on Ctrl+C or SIGTERM. A `kill -9` is also safe. The next
start repairs a torn last line, if there is one, and resumes ingest exactly
where it stopped.

Add `--vscode-replay` to reconstruct VS Code sessions in a sandboxed,
invisible copy of VS Code (tier 2, below). It is off by default.

Data directory (mode 0700):

| File           | What                                                   |
|----------------|--------------------------------------------------------|
| `events.jsonl` | append-only capture log, the ground truth              |
| `brain.db`     | SQLite (WAL) index derived from the log                |
| `run.lock`     | held by the running `brain run` (one per data dir)     |
| `kwin/`        | generated KWin focus-tracking script                   |
| `vscode/`      | VS Code replay sandbox and file snapshots (tier 2 only) |

Example log lines:

```json
{"ts":"2026-11-15T17:20:01.112Z","app":"org.kde.konsole","type":"focus","window":"~ : bash — Konsole"}
{"ts":"2026-11-15T17:20:02.348Z","app":"org.kde.konsole","type":"key","key":"H","mods":["shift"],"window":"~ : bash — Konsole"}
```

## Sessions and reconstruction

Ingest (inside `brain run`, about once a second) turns the log into
**writing sessions**, stored in the `sessions` table with an FTS5 index.

* A session is one `(app, window title)`. It closes after **30 s without
  typing in that window**, or right away when a single-line field is
  submitted (Enter at a shell prompt, in a chat box or a browser's address
  bar), cancelled (Ctrl+C at a prompt), or left for another field of the
  same window (Tab, Ctrl+L in a browser). Switching to another app and back
  within 30 s continues the same session. Stopping or restarting `brain run`
  closes all sessions.
* While a session is open, its text so far is already in the index, so a
  search finds what you are typing right now.
* **Tier 0**: keys are replayed into a virtual text field with a cursor,
  selection, word-wise movement and deletion, and undo/redo. Only keyboard
  cursor movement is followed (see "Decisions and edge cases").
* **Tier 1**: a small table of app classes decides what keys mean. In
  terminals: readline shortcuts (Ctrl+A/E/U/K/W) and Up/Down history recall
  from the commands `brain` saw you run. In browsers: Tab and Ctrl+L/K/T/W
  move to another field. Enter submits single-line fields and inserts a
  newline elsewhere. Editors and unknown apps keep their document across
  sessions. Single-line fields start empty each time.
* **Tier 2** (`--vscode-replay`): when a VS Code session closes, its
  keystrokes are replayed into a headless VS Code with your settings, in a
  network-less sandbox with capped CPU and memory. The session text then
  comes from VS Code itself (auto-indent, bracket pairs, your keybindings).
  The session stores the changed lines. If replay is impossible or fails,
  the tier 0/1 text stays.

Inspect the result:

```sh
sqlite3 ~/brain-data/brain.db \
  "SELECT id, app, started_at, ended_at, status, end_reason, tier_used, reconstructed_text FROM sessions"
```

## Decisions and edge cases

* **The mouse is not captured.** `brain` reads keyboards only. A mouse
  click can move the text cursor, but its position on screen tells `brain`
  nothing about where in the text the cursor went, so reconstruction cannot
  use it. Tiers 0/1 (and tier 2) therefore **do not support text edited
  after the cursor was moved with the mouse** (clicking into the middle of a
  line to fix a typo, selecting with the mouse, drag and drop): typing
  continues from where the *keyboard* last left the cursor. Cursor moves
  with the keyboard (arrows, Home/End, Ctrl+arrows, Shift+selection,
  Ctrl+A) are followed.
* **Cursor at session start.** When a session starts on text `brain` did
  not see being typed (VS Code replay, or a document after `brain run`
  restarted), the cursor is assumed to be at the end.

## Tests

```sh
cargo test                                   # unit + integration tests
cargo test --release --test crash_safety     # 25 random kill -9s under load;
                                             # sessions must equal a rebuild from the log
```
