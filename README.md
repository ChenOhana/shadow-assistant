# brain — a shadow assistant for Linux

`brain` runs on your computer, follows what you type in each application and
lets you search it later.

```
brain run                      # capture keystrokes + focus changes, store them
brain query search "<words>"   # full-text search over writing sessions
brain query text --app A       # what was typed into an app, reconstructed
chat/brain-chat                # ask Claude about it in plain language
```

> **Status: complete.** Research and architecture are in [PLAN.md](PLAN.md);
> the AI transcripts are in [ai-logs/](ai-logs/).

## Supported OS

Ubuntu (tested on 26.04), on Wayland, with either desktop:

| Desktop | Focus tracking | Paste text | Screen lock | Layouts |
|---|---|---|---|---|
| **KDE Plasma 6** (Kubuntu, or Ubuntu with Plasma) | KWin script, loaded by brain itself | Klipper | `org.freedesktop.ScreenSaver` | kxkbrc, Alt+Shift and tray switches |
| **GNOME** (stock Ubuntu) | brain's GNOME Shell extension (`make install`, then log out and in once) | the extension | `org.gnome.ScreenSaver` | GNOME's input sources, Super+Space switches |

Key capture itself works on any Linux. On another desktop, `brain run`
warns and still captures keys, attributed to the app `unknown`, without
paste text, and detects the lock screen only if the desktop implements
`org.freedesktop.ScreenSaver`, so **an unlock password may be recorded
there**. The optional VS Code tier 2 (`--vscode-replay`) works on both
desktops; it replays into VS Code when a file was not saved, which needs
VS Code installed (`/usr/bin/code`, or the snap).

## Build

```sh
# Rust toolchain (https://rustup.rs), then:
cargo build --release
# binary: target/release/brain

# or build and install to /usr/bin/brain (asks for sudo for the copy only),
# plus brain's GNOME Shell extension for your user when GNOME is installed:
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

**On GNOME**, brain also needs its GNOME Shell extension: Wayland gives
programs no way to see the focused window, and on KDE brain loads a KWin
script for that by itself, but GNOME only loads extensions at login.
`make install` installs and enables it for your user when GNOME is
installed (or `make install-gnome-extension` alone); log out and back in
once. Until then brain warns and files keys under `unknown`. The extension
answers brain's clipboard reads at a paste, and any program in your session
could ask it the same, as with KDE's Klipper.

## Run

```sh
target/release/brain run --data-dir ~/brain-data
```

`run` stops cleanly on Ctrl+C or SIGTERM. A `kill -9` is also safe. The next
start repairs a torn last line, if there is one, and resumes ingest exactly
where it stopped.

Add `--vscode-replay` to reconstruct VS Code sessions with the real editor
(tier 2, below): a diff of the file when you saved it, otherwise a replay in
a sandboxed, invisible copy of VS Code. It is off by default.

### As a service

`brain` can run as a **systemd user service**: it starts with your desktop
session, restarts if it crashes, stops at logout, and runs as you (no
root; it needs your desktop session for focus tracking):

```sh
make install-service                 # installs /usr/bin/brain + the unit, enables it
journalctl --user -u brain -f        # its messages
systemctl --user stop brain          # pause capture
brain query search "words" --data-dir ~/.local/state/brain
make uninstall-service               # remove the service AND its captured data
make uninstall-service KEEP_DATA=1   # remove the service, keep the data
```

The service keeps its data in its systemd **state directory**,
`~/.local/state/brain` (`$XDG_STATE_HOME/brain`), which systemd creates
private to you (mode 0700) before starting it. Pass that directory to
`brain query`. `make uninstall-service` deletes it with
`systemctl --user clean --what=state brain`, unless `KEEP_DATA=1` is given.
Only one `brain run` can use a data directory at a time; runs on separate
directories work side by side (a test run next to the service, say), and
each records everything you type. The service runs without
`--vscode-replay`; add it to `ExecStart` in
`~/.config/systemd/user/brain.service` to turn it on.

Data directory (mode 0700):

| File           | What                                                   |
|----------------|--------------------------------------------------------|
| `events.jsonl` | append-only capture log, the ground truth              |
| `brain.db`     | SQLite (WAL) index derived from the log                |
| `run.lock`     | held by the running `brain run` (one per data dir)     |
| `kwin/`        | generated KWin focus-tracking script (KDE only)        |
| `vscode/`      | VS Code replay sandbox and file snapshots (tier 2 only) |

Example log lines:

```json
{"ts":"2026-11-15T17:20:01.112Z","app":"org.kde.konsole","type":"focus","window":"~ : bash — Konsole"}
{"ts":"2026-11-15T17:20:02.348Z","app":"org.kde.konsole","type":"key","key":"H","mods":["shift"],"window":"~ : bash — Konsole"}
{"ts":"2026-11-15T17:20:03.020Z","app":"org.kde.konsole","type":"key","key":"Backspace","mods":[],"repeat":true,"window":"~ : bash — Konsole"}
{"ts":"2026-11-15T17:20:05.400Z","app":"google-chrome","type":"paste","text":"pasted words","window":"New tab - Google Chrome"}
{"ts":"2026-11-15T17:20:07.801Z","app":"com.microsoft.VSCode","type":"focus","window":"● notes • Untitled-1 - proj - Visual Studio Code","renamed":true}
{"ts":"2026-11-15T17:21:00.000Z","app":"com.microsoft.VSCode","type":"system","state":"lock"}
```

Every line has a time (UTC), the app and a type: `key` (`repeat: true` for
key repeats), `focus` (a window activated, or with `renamed: true` the
active window's title changed), `paste` (the clipboard text at a paste
shortcut) and `system` (`start`, `stop`, `sleep`, `wake`, `lock`,
`unlock`).

## Query

All query commands run while `brain run` is capturing and print JSON.
Errors go to stderr with a non-zero exit code.

```sh
brain query search "migration blocked" [--app A] [--from T] [--to T] [--at T] [--limit N]
brain query text --app A [--from T] [--to T] [--at T]
brain query sessions [--app A] [--window W] [--from T] [--to T] [--at T] [--limit N] [--offset K]
brain query apps [--from T] [--to T] [--at T]
```

* **`search`**: sessions whose text contains *every* word, anywhere inside
  a word, in any case (`run` finds "rerun", `בית` finds "בבית"), newest
  first (default limit 20). Open sessions are included with what has been
  typed so far (`ended_at: null`).
* **`text`**: what was typed into the app during the time range,
  **reconstructed**: the keys and pastes of the range, split per app window
  (window title), run through the app's tier 0/1 rules (Backspace,
  selections, undo, Enter submitting a shell line, …), each app window
  starting empty at the start of the range. Sessions play no part: a
  document edited in several sessions is one text. `windows` lists each app
  window's text, ordered by its first keystroke; `text` joins them one per
  line. Edits reaching into text typed before the range (Backspace past its
  start, Ctrl+A) only affect what was typed inside it.
* **`sessions`**: the sessions in the window, oldest first, with their full
  text and why they ended, by app and **window-title** substring (default
  limit 50, `total` and `--offset` to page). In a terminal each submitted
  command is a session (`end_reason: "commit"`) and the title usually names
  the host (`juan@prod-db: ~`), so `brain query sessions --app konsole
  --window prod-db --from 2026-11-17 --to 2026-11-18` lists the commands
  run on prod-db that day.
* **`apps`**: the apps with sessions in the window, most recently active
  first, with session counts: the ids `--app` accepts.
* **`--app A`** can be part of the app id, ignoring case (`--app chrome`
  finds `google-chrome`). `search` keeps every app that matches. `text`
  resolves `A` to one app and reports its full id (`"app": "google-chrome"`):
  an exact match wins, otherwise the one id that contains `A`; if several do,
  or none, it is an error that lists them, so text of different apps is
  never mixed.

**Time flags** (all commands): `--at T` is the 10 minutes ending at T;
`--from T` alone runs to now; `--from` and `--to` together are an explicit
window. `--to T` alone means the 10 minutes ending at T for `text`, and
everything up to T for the others. With no flags, `text` covers the last 10
minutes and the others all time. T can be:

| Form | Example |
|---|---|
| relative to now | `-30s`, `-15m`, `-2h`, `-1d`, `-1h30m`, `now` |
| local date and time | `"2026-11-16 14:00"`, `2026-11-16T14:00:30` |
| local date (midnight) | `2026-11-16` |
| local time of day (today) | `14:00`, `14:00:30` |
| RFC 3339 with a zone | `2026-11-16T12:00:00Z`, `2026-11-16T14:00:00+02:00` |

Times without a zone are in the local time zone. Output times are local
too, with their offset (`2026-11-17T00:04:38.044+02:00`, still RFC 3339),
and the resolved window is echoed (`from`, `to`, `at`). The log and the
database store UTC.

`search` output:

```json
{
  "query": "migration blocked",
  "from": null,
  "to": null,
  "results": [
    {
      "session_id": "1",
      "app": "synthetic",
      "window": "corpus.txt",
      "started_at": "2026-11-16T16:51:14.684+02:00",
      "ended_at": null,
      "snippet": "the migration is blocked on the schema change. Dana a…"
    }
  ]
}
```

`text` output:

```json
{
  "app": "org.kde.kate",
  "at": null,
  "from": "2026-11-17T10:00:00.000+02:00",
  "to": "2026-11-17T10:10:00.000+02:00",
  "text": "Shopping list:\n- oat milk\n- bread\n- קפה x2\nTODO: call Dana\n\tsend the \"Q3\" report\npath: C:\\temp",
  "windows": [
    {
      "window": "notes.md — Kate",
      "text": "Shopping list:\n- oat milk\n- bread\n- קפה x2"
    },
    {
      "window": "todo.md — Kate",
      "text": "TODO: call Dana\n\tsend the \"Q3\" report\npath: C:\\temp"
    }
  ]
}
```

That is the output for this typing (`tests/query_cli.rs` replays it):

| When | Window | Keys |
|---|---|---|
| 10:00 | `notes.md — Kate` | `Shopping list:⏎- milk⏎- breadd⌫` |
| 10:01 | `todo.md — Kate` | `TODO: call Dana⏎⇥send the "Q3" report⏎path: C:\temp` |
| 10:03 (a new session) | `notes.md * — Kate` | Ctrl+Home `↓` End Ctrl+⌫ `oat milk` Ctrl+End `⏎- קפה` Ctrl+V (`" ☕ x2"`) Ctrl+Z `" x2"` |

With `--from 10:02` instead, `notes.md` starts empty and the result is
`"oat milk\n- קפה x2"`. In Konsole, `git stauts⌫⌫⌫tus⏎`, `↑ -s⏎`,
`rm -rf build` Ctrl+C, `echo "שלום" > out.txt⏎` give
`"git status\ngit status -s\nrm -rf build\necho \"שלום\" > out.txt"`.

## Ask in plain language: brain-chat

[`chat/`](chat/README.md) wraps the query commands with Claude, so you can
ask "which commands did I run yesterday on prod-db?" or "what did I tell
Dana about the migration?" and get an answer that cites the times, apps and
windows it comes from:

```sh
sudo apt install python3-venv && make chat-setup   # once
export ANTHROPIC_API_KEY=...
chat/brain-chat --data-dir ~/.local/state/brain    # a chat; or pass a question
```

Claude answers by calling read-only tools (`list_apps`, `sessions`,
`search`, `text`) that run `brain query` on your machine, in a loop it ends
when it has the answer (at most 10 rounds of tool calls per question).
**Tool results go to Anthropic's API**: likely secrets (tokens, keys,
passwords in commands, a password typed at a `sudo` prompt) are masked
first, as a best effort, and every tool call is printed.

## Sessions and reconstruction

Ingest (inside `brain run`, about once a second) turns the log into
**writing sessions**, stored in the `sessions` table with an FTS5 index.

* A session is one `(app, window title)`. Markers that change while you
  type are removed from the title first: an editor's unsaved-file dot or
  `*`, and the spinner glyphs terminal programs animate (`◐ ◑ ⠹ ✳`).
  When the title changes *because of the typing* (VS Code names a new file
  after its first line, a mail window after its subject), the session
  continues: a change of the *same* window's title (the KWin script and
  the GNOME extension tell a title change from a window activation) within 1 s of a keystroke in it, with no
  shortcut in between, is treated as a rename.
* A session closes after **30 s without typing in that window**, or right
  away when a single-line field is submitted (Enter at a shell prompt, in a
  chat box, a browser's address bar or a launcher such as KRunner),
  cancelled (Ctrl+C at a prompt), or left for another field of the same
  window (Tab, Ctrl+L in a browser). Switching to another app and back
  within 30 s continues the same session. Stopping or restarting
  `brain run` closes all sessions.
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
* **Tier 2** (`--vscode-replay`): when a VS Code session opens, the
  edited file's initial content is saved. When the session closes:
  1. if the file was saved after your last keystroke (autosave, or Ctrl+S
     at the end), the session text is a **git-style diff** from the initial
     content to the saved file (`--- a/test.cpp`, `+++ b/test.cpp`, `@@`
     hunks with 3 lines of context, `-` removed and `+` added lines): what
     you did to the file in that session, autocomplete and auto-indent
     included; VS Code is not started;
  2. otherwise the keystrokes are replayed into a headless VS Code with
     your settings, in a network-less sandbox with capped CPU and memory, as
     a best effort: the cursor is assumed at the end of the file, your
     extensions are not loaded (so their autocomplete is not reproduced),
     and nothing checks the result.

  The replay stores the lines it changed; tier 0/1 text stays if neither
  works.

  Every key in a VS Code window counts toward its session, including
  shortcuts the tier 0/1 adapters don't understand (Ctrl+D to duplicate a
  line, Alt+Up to move one, Ctrl+/ to comment, Ctrl+S): they can change the
  file. A session made only of such shortcuts gets the saved file's diff
  and is never replayed; when the file was not saved after it, or the diff
  is empty, the session changed nothing and is removed (as is any session
  whose saved file shows no change). The file is also copied when a VS Code
  window is focused, so Ctrl+D then Ctrl+S right after focusing still has
  the content to diff against. Without `--vscode-replay`, VS Code behaves
  like any other app.

  The `tier2_status` column says what happened:

  | `tier2_status` | Meaning |
  |---|---|
  | `resolving` | looking for the edited file |
  | `snapshot` | initial content saved, session open |
  | `pending` | session closed, waiting for the worker |
  | `pending: saved file only` | a session of shortcuts only, waiting for the saved file's diff (removed if there is none) |
  | `done: saved file` | a diff of the file saved after the last edit (exact) |
  | `done: replayed (best effort, unverified)` | text from a replay in VS Code |
  | `failed: …` | the replay failed; tier 0/1 text kept |
  | `skipped: …` | tier 2 impossible (unsaved new file, unsaved changes at the start, file not found, …) |

  The edited file is found from the window title: VS Code's default title
  only has the file and folder names, so `brain` looks the name up in VS
  Code's own state for that window (the editors it has open, also for files
  opened from outside the folder), under the folder, and in the window's
  editor history. VS Code records a newly opened file there a little late,
  so the lookup is retried while the session is open (up to 2 minutes).
  For an instant, exact match, put the full path in the title (VS Code
  setting `"window.title": "${dirty}${activeEditorLong}${separator}${rootName}${separator}${appName}"`).

Inspect the result:

```sh
sqlite3 -readonly ~/brain-data/brain.db \
  "SELECT id, app, started_at, ended_at, status, end_reason, tier_used, tier2_status, reconstructed_text FROM sessions"
```

A GUI such as DB Browser for SQLite keeps a read transaction open and shows
the database as of when it started: sessions that have closed since still
look `open`, and SQLite cannot shrink its WAL file meanwhile. Open the
database read-only and reopen it to see the current state.

## Decisions and edge cases

**Platform and capture**
* **Linux, KDE Plasma 6 and GNOME on Wayland.** Wayland gives applications
  no way to observe other apps' keystrokes, so `brain` reads the kernel's
  input devices (evdev) passively: read-only, never grabbed, so it cannot
  add input lag. Focus (app + window title) comes from inside the
  compositor: a small KWin script on KDE, a GNOME Shell extension on GNOME,
  both reporting over D-Bus, and brain trusts only reports sent from the
  compositor's own bus connection. PLAN.md compares the alternatives.
* **GNOME is verified against a real GNOME Shell, run headless** by
  `tests/gnome_e2e.rs`: focus, renames, layout switches, the clipboard,
  and the extension being disabled and re-enabled, as GNOME does around
  the lock screen. Its lock screen is checked only through a stand-in for
  GNOME's lock service, and a real stock-Ubuntu login has not been tried
  yet.
* **Platform layer.** Capture sits behind interfaces
  (`src/capture/platform.rs`): keys from evdev work on any Linux desktop;
  focus, clipboard, lock and keyboard settings come from a backend per
  desktop, picked from `XDG_CURRENT_DESKTOP`. Another desktop, or another
  OS, is a new module, not a change to the rest (PLAN.md 2.4).
* **Keys are resolved at capture time** with libxkbcommon and your real
  layouts (us/il; Alt+Shift and tray switches on KDE, Super+Space on
  GNOME), so the log holds the characters the app received (`"ש"`, `"H"`),
  as in the assignment's example event. Key repeat is regenerated on the
  desktop's schedule (KDE's kcminputrc, GNOME's keyboard settings).
* **Held keys** are logged with the repeats the desktop's schedule produces
  (KDE: 600 ms delay, then 25 Hz; GNOME: 500 ms, then every 30 ms), exactly. The app may still have received one
  more or one fewer character per hold: on Wayland each app runs its own
  repeat timer, and Qt apps use timers that may fire up to 5% early or late
  (a 600 ms delay fires anywhere in ~570–630 ms). Measured against Kate, the
  same 620 ms hold gave 1, 2 or 3 characters across runs; brain's
  reconstruction stays within one character per hold (two for holds over
  2 s). `tests/repeat_e2e.rs` measures this against a real Kate window.
* **Shortcuts are logged on the Latin layout** (Ctrl+A, not Ctrl+ש), the
  way applications match them; this also makes paste detection work in
  Hebrew.
* **Paste** (Ctrl+V, Ctrl+Shift+V, Shift+Insert): the clipboard text is
  read at that moment (from Klipper on KDE, from the extension on GNOME)
  and logged, up to 64 KiB.
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
  titles renamed by the typing itself followed as the same session. Another
  window being activated is never a rename; but switching *tabs* of the
  same window with the mouse (invisible to brain) within 1 s of the last
  keystroke would wrongly count as one.
* **30 s idle per window; focus loss alone never ends a session.** Enter
  ends a session only in single-line fields (shell prompts, chat boxes,
  browser address/search bars, launchers such as KRunner, by app class);
  elsewhere it is a newline. In browsers and chat apps, Shift+Enter is a
  newline.
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
  typo typed and deleted, whitespace). With VS Code tier 2, the saved file
  decides: a session whose diff is empty is removed once the worker has
  checked it.
* **VS Code tier 2 is opt-in** (`--vscode-replay`). It needs the file's
  content from before the session, so it is skipped for unsaved new files
  (`Untitled-1`), for files with unsaved changes when the session started,
  and for files brain could not identify, or copy before they were saved,
  within 2 minutes of the session start.
* **A saved file gives an exact diff.** Its format matches `git diff -U3`;
  in very repetitive text an ambiguous line may be placed differently than
  git's heuristics would. Search also matches words on the diff's removed
  (`-`) and context lines.
* **A replay is a best effort**: ~25 s for the first one and ~1.2 GB while
  VS Code runs (capped, outside brain's budget), the cursor assumed at the
  end of the file, and no extensions when yours exceed 300 MB, so
  autocomplete from extensions is not reproduced.

**Queries**
* **`query text` reconstructs "stupidly", per app window.** It takes the
  keys typed in the time range and runs them through the same tier 0/1
  adapters the sessions use, per app and window title, from an empty text:
  no session boundaries, no text from before the range. (An earlier version
  returned the raw keys with `<Backspace>` etc.; the owner asked for
  reconstruction.)
* **Search matches substrings.** The index uses SQLite FTS5's trigram
  tokenizer: a query word matches anywhere inside a word, ignoring case and
  accents (`run` finds "running" and "rerun", `בית` finds "בבית", `cafe`
  finds "Café"). This replaces the design's English-only porter stemmer,
  which cannot find Hebrew words with attached prefixes. Query words
  shorter than 3 characters cannot use the trigram index and are matched
  with a slower scan.
* **Times in, times out are local.** `--from`, `--to` and `--at` accept
  relative times (`-2h`), local dates and times, and RFC 3339; output times
  are local with their offset. The log and the database store UTC.
* **`--app` takes part of the id.** `search` keeps every matching app;
  `text` must resolve to exactly one app and reports its full id, so two
  apps' keystrokes are never mixed into one stream.
* **`query text` assumes the log is in time order.** It starts reading at
  the last checkpoint before its window and stops 60 s of log after it. If
  the system clock is stepped backwards while capturing (an NTP correction,
  a manual change), keys typed after the step whose times fall in a queried
  window can be missed. PLAN.md (section 10) describes an exact bound.

**Running**
* **Service as you, not root**: a systemd user service, because desktop
  integration needs your session's D-Bus, and a root keylogger is the
  worst case if it is ever compromised.
* **Several runs on separate data directories are supported**, each
  capturing everything. One rare gap: after a `kill -9`, the next start
  unloads the KWin script the killed run left behind, by its name
  `brain_focus_<pid>`; if another run has meanwhile started with that same,
  reused process id, its script is unloaded instead and it stops seeing
  focus changes until it restarts.

## Tests

```sh
cargo test --release      # unit tests (~130) and the default integration tests:
                          #   crash_safety  25 random kill -9s under load; sessions,
                          #                 documents and checkpoints must equal a
                          #                 clean rebuild from the log
                          #   query_cli     the query commands through the binary,
                          #                 incl. thousands of queries while run writes
                          #   perf_budget   ≤ 1 % CPU and ≤ 32 MB while typing
                          #   tier2_e2e     VS Code saved-file diffs and shortcut-only
                          #                 sessions (no VS Code needed)
                          #   two_instances two runs on separate folders stay apart;
                          #                 a second run on the same folder is refused
```

brain-chat has its own tests (`make chat-test`, offline: redaction, the
tools against a real data directory, the question loop with a scripted
model, the request through the real SDK with the network replaced) and a
live check against the API (`chat/.venv/bin/python chat/live_check.py`,
which costs a little).

GNOME is tested against a real GNOME Shell: a private, headless one that
the test starts in its own D-Bus session and home, inside its own systemd
scope, so the desktop you use is not touched (it needs `gnome-shell`,
`gjs` and `python3-gi`; nothing is typed):

```sh
cargo test --release --test gnome_e2e -- --ignored --nocapture
```

Tests against real software are opt-in (`-- --ignored`):

```sh
# Type through a virtual keyboard into real Kate windows; hands off the
# keyboard and mouse while they run, and the screen must be unlocked:
cargo test --release --test repeat_e2e --test real_keyboard_e2e --test two_instances -- --ignored --nocapture --test-threads=1
# Replay into a real (headless) VS Code, incl. a file opened from outside
# the window's folder, identified late through VS Code's window state:
cargo test --release --test vscode_replay --test tier2_e2e -- --ignored --test-threads=1
# The same, with VS Code shown as a normal window to watch the replays
# (don't type meanwhile: it takes focus):
BRAIN_VSCODE_VISIBLE=1 cargo test --release --test vscode_replay --test tier2_e2e -- --ignored --test-threads=1
```

`sudo sh scripts/setup-test-permissions.sh` (undo with `--undo`) grants two
narrow permissions once: access to `/dev/uinput` for the user at the local
seat, which the Kate tests need, and passwordless
`sudo /usr/sbin/rtcwake -m no -s 30` only, which the manual sleep/wake check
below uses (it sets a wake alarm; nothing else is allowed).

Sleep/wake is checked by hand, with a real suspend through logind (the same
path as the KDE menu or closing the lid); no automated test does it:

```sh
brain run --data-dir /tmp/brain-sleep                     # terminal 1
sudo /usr/sbin/rtcwake -m no -s 30 && systemctl suspend   # terminal 2: wakes after 30 s
# after the resume, brain prints how many keyboards it reopened;
# type something in Kate, wait 30+ s, then:
grep -E '"state":"(sleep|wake)"' /tmp/brain-sleep/events.jsonl
brain query text --data-dir /tmp/brain-sleep              # shows the text typed after the resume
```

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
   index for open sessions). It was kept out of the repo and, once the code
   was done, rewritten against it.
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
   the title "New tab", an animated terminal title (Claude Code's own
   spinner) splitting a session every second, VS Code's new-file title
   (named after the text being typed) splitting a session on every
   character, VS Code's real app id (`com.microsoft.VSCode`) not being
   recognised for tier 2, KRunner's Enter recorded as a newline, VS Code
   files opened from outside the window's folder not being identified, and
   shortcut-only VS Code edits (Ctrl+D then Ctrl+S) leaving no session.
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
10. **Extra characters from held keys.** The input loop emitted the
    repeats due "now" before reading key releases that were already
    queued, so a late wakeup added repeats the app never saw. Found when
    the owner reported held keys reconstructing wrongly; fixed (read all
    queued events first) and covered by a model-based stress test and a
    real-Kate test.
11. **A rename rule that merged two windows.** The rule that keeps VS
    Code's new-file session together (a title change shortly after typing
    is a rename) also fired when another Kate window opened right after
    typing, so the second window's text went into the first window's
    session. The real-keyboard test caught it; KWin now reports whether a
    focus event is a window activation or a title change, and only title
    changes of the typed window can be renames.
12. **A test weaker than it reads.** The kill -9 test never actually
    killed brain in the middle of a log write (0 torn lines across runs);
    torn-line repair is covered by unit tests, not by that test.
13. **A status that overstated the result.** VS Code replays were marked
    `done` whether or not they matched what VS Code really produced; a C++
    session with autocompletion replayed into garbage while the file the
    owner had saved held the exact text. Now a saved file gives the session
    a diff, and a replay is labelled best effort.
14. **A test that left a process running.** A failed test run left a
    synthetic `brain run` typing into a deleted temp directory for 16 hours;
    the tests started `brain run` without a guard that stops it on failure.
    The owner noticed it. Every test now uses such a guard and checks that
    nothing is left running.
15. **Incomplete instructions.** Asked how to trigger a VS Code replay, it
    said to open a file with no unsaved-changes dot, without saying the file
    must exist on disk; the owner tried a new untitled file, which tier 2
    always skips. The owner pointed it out, and it then listed every
    condition the replay needs.
16. **A probe that left a process behind.** Trying a headless GNOME Shell
    in a private D-Bus session, a desktop portal that bus had started
    outlived the run. It found and stopped it; the GNOME test now runs the
    private shell in its own systemd scope, gives its bus no services to
    start, and checks that nothing on that bus survives.
