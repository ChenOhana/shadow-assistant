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

### As a service

`brain` can run as a **systemd user service**: it starts with your Plasma
session, restarts if it crashes, stops at logout, and runs as you (no
root; it needs your desktop session for focus tracking):

```sh
make install-service          # installs /usr/bin/brain + the unit, enables it
journalctl --user -u brain -f # its messages
systemctl --user stop brain   # pause capture
make uninstall-service        # remove it
```

The service uses `~/brain-data`. Stop it before running `brain run` by hand
on the same directory (only one `brain run` per data directory).

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

* A session is one `(app, window title)`. Markers that change while you
  type are removed from the title first: an editor's unsaved-file dot or
  `*`, and the spinner glyphs terminal programs animate (`◐ ◑ ⠹ ✳`).
  When the title changes *because of the typing* (VS Code names a new file
  after its first line, a mail window after its subject), the session
  continues: a title change within 1 s of a keystroke in that window, with
  no shortcut in between, is treated as a rename. It closes after **30 s without
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

**Platform and capture**
* **Linux, KDE Plasma 6 on Wayland.** Wayland gives applications no way
  to observe other apps' keystrokes, so `brain` reads the kernel's input
  devices (evdev) passively: read-only, never grabbed, so it cannot add
  input lag. Focus (app + window title) comes from a small KWin script
  that calls `brain` over D-Bus. PLAN.md compares the alternatives.
* **Keys are resolved at capture time** with libxkbcommon and your real
  layouts (us/il, Alt+Shift switching, layout changes from the tray), so
  the log holds the characters the app received (`"ש"`, `"H"`), as in the
  assignment's example event. Key repeat is regenerated on KDE's schedule.
* **Shortcuts are logged on the Latin layout** (Ctrl+A, not Ctrl+ש), the
  way applications match them; this also makes paste detection work in
  Hebrew.
* **Paste** (Ctrl+V, Ctrl+Shift+V, Shift+Insert): the clipboard text is
  read from Klipper at that moment and logged, up to 64 KiB.
* **Screen lock pauses capture**, so the unlock password is never logged.
* **Sleep/wake**: on resume (logind's PrepareForSleep signal) all
  keyboards are reopened. Tested with a real suspend.
* **The mouse is not captured.** `brain` reads keyboards only. A mouse
  click can move the text cursor, but its position on screen tells `brain`
  nothing about where in the text the cursor went, so reconstruction cannot
  use it. Tiers 0/1 (and tier 2) therefore **do not support text edited
  after the cursor was moved with the mouse** (clicking into the middle of a
  line to fix a typo, selecting with the mouse, drag and drop): typing
  continues from where the *keyboard* last left the cursor. Cursor moves
  with the keyboard (arrows, Home/End, Ctrl+arrows, Shift+selection,
  Ctrl+A) are followed.

**Storage**
* **The JSONL log is the ground truth; SQLite is derived.** Capture only
  appends one line per event (never blocks on SQLite). Ingest builds the
  index from the log once a second, in one transaction together with the
  log offset, so a crash never duplicates or skips anything. Deleting
  `brain.db` rebuilds it from the log. Raw keystrokes are not copied into
  SQLite.
* **No backward compatibility yet.** This is the first version: a database
  with another schema version is refused (delete it to rebuild).

**Sessions and reconstruction**
* **Session key `(app, window title)`**, with markers that change while
  typing removed from the title (unsaved-file dots, terminal spinners), and
  titles renamed by the typing itself followed as the same session. A real
  switch to another window or tab within 1 s of the last keystroke made
  with the mouse (invisible to brain) would wrongly count as a rename.
* **30 s idle per window; focus loss alone never ends a session.** Enter
  ends a session only in single-line fields (shell prompts, chat boxes,
  browser address/search bars, launchers such as KRunner, by app class);
  elsewhere it is a newline.
  In browsers, Shift+Enter is a newline.
* **Single-line fields start each session empty**; editors keep their
  document across sessions. Window titles of single-line fields are
  reused by unrelated fields ("New tab", "~"), so carrying text over would
  mix them.
* **Reconstruction is incremental**: each key is applied once to the open
  session's in-memory buffer, and open sessions' text is refreshed in the
  index every second, so search sees what is being typed now. (The design
  reconstructed at close and rebuilt open sessions at query time; the
  result is the same, the cost is not.) VS Code tier 2 runs at close.
* **Unknown cursor = end of the text**: after `brain run` restarts, and at
  the start of a VS Code replay.
* **Shell history recall** (Up) uses only the commands `brain` saw being
  run in that terminal app; history from before, tab completion and shell
  autosuggestions (zsh) are not visible, so those lines can be wrong.
* **Sessions that changed nothing leave no row** (cursor movement only, a
  typo typed and deleted, whitespace).
* **Search matches substrings.** The index uses SQLite FTS5's trigram
  tokenizer: a query word matches anywhere inside a word, ignoring case and
  accents (`run` finds "running" and "rerun", `בית` finds "בבית", `cafe`
  finds "Café"). This replaces the design's English-only porter stemmer,
  which cannot find Hebrew words with attached prefixes. Query words
  shorter than 3 characters cannot use the trigram index.
* **VS Code tier 2 is opt-in** (`--vscode-replay`): it needs the file's
  content at session start (snapshot within 10 s, no unsaved changes),
  costs ~25 s for the first replay and ~1.2 GB while VS Code runs (capped,
  outside brain's budget), and runs with extensions disabled when your
  extensions exceed 300 MB. Suggestion acceptance may not replay the same.

**Running**
* **Service as you, not root**: a systemd user service, because desktop
  integration needs your session's D-Bus, and a root keylogger is the
  worst case if it is ever compromised.

## Tests

```sh
cargo test                                   # unit + integration tests
cargo test --release --test crash_safety     # 25 random kill -9s under load;
                                             # sessions must equal a rebuild from the log
```

Tests that use real hardware need two narrow permissions, granted once by
`sudo sh scripts/setup-test-permissions.sh` (undo with `--undo`):
passwordless `sudo /usr/sbin/rtcwake -m no -s 30` only (sets a wake alarm;
the sleep/wake test then suspends with `systemctl suspend`), and access to `/dev/uinput` for
the user at the local seat (virtual keyboard tests).

## Where the agent failed

This project was built by Claude Code (Opus 5.5) with the owner reviewing
every stage. These are the mistakes it made, in order, and how they were
caught. Transcripts are in `ai-logs/`.

1. **Wrong library assumption.** It wrote D-Bus code for zbus 6 with a
   feature that does not exist there; the build failed and it moved to
   zbus 5.
2. **A commit that did not build.** The first commit declared a module
   that was only added in the next one. Caught by building each commit
   in a clean worktree; the history was rewritten before anything was
   pushed.
3. **A background agent's PLAN.md described things that were never
   built** (a "durable offset", timerfds, text session ids, an in-memory
   index for open sessions). It was kept out of the repo until it can be
   rewritten against the code.
4. **Unlocking the owner's screen.** To test lock handling it ran
   `loginctl lock-session` and `unlock-session` without first checking the
   screen state. The screen was already locked (the owner was away), so it
   unlocked the machine for 23 seconds before noticing and re-locking it.
   It now checks and restores desktop state before such tests.
5. **A sleep/wake test that could not work.** `rtcwake -m mem` suspends
   behind logind's back, so brain (correctly) saw nothing. The test was
   redone through `systemctl suspend`.
6. **Bugs only found on the owner's real data**, not by its own tests:
   shortcuts on the Hebrew layout logged as Hebrew letters (so Ctrl+V was
   not recognised as paste), text leaking between Chrome tabs that share
   the title "New tab", and an animated terminal title (Claude Code's own
   spinner) splitting a session every second.
7. **Work the owner did not want.** It built touchpad tap detection for
   click logging, then the owner pointed out that clicks without a cursor
   position cannot help reconstruction and capture of the mouse was
   removed. It also added backward-compatibility code (a schema migration,
   a catch-all event type) for a first version that has no users yet.
8. **Imprecise explanation.** It told the owner the porter stemmer makes
   "a deleted word still appear" in search; the real effect is narrower
   (a different word with the same stem matches). Corrected when asked.
9. **Not flagging a design deviation.** Reconstruction is incremental, not
   "at session close" as the design says; it only explained this when the
   owner asked how reconstruction works.
10. **A test weaker than it reads.** The kill -9 test never actually
    killed brain in the middle of a log write (0 torn lines across runs);
    torn-line repair is covered by unit tests, not by that test.
