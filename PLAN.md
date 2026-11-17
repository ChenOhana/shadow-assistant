# PLAN — `brain`, a shadow assistant for Linux (KDE Plasma 6 / Wayland)

This is the research and architecture write-up for `brain`: how keystrokes
and application context are collected, how they are ingested, how writing
sessions are defined and their text reconstructed, how everything is stored
and indexed, and how queries read it. It describes the code as built; the
original design doc is in `ai-logs/design-doc.md`, and the places where the
build deliberately differs from it are called out ("Differs from the design").

## 1. Summary

```
 keyboards ──► input thread ──► Recorder ──► events.jsonl  (append-only, ground truth)
 (evdev,         │ xkbcommon      ▲ app + window                │ ~1 s
  passive)       │ key repeat     │ from KWin                   ▼
                 ▼                │                       ingest thread ──► brain.db (SQLite WAL)
 KWin script ──D-Bus──────────────┘                         │ sessions        sessions + FTS5 (trigram)
 Klipper (paste text), screen lock, sleep, layouts ─────────┘ reconstruction  context_state
                                                              checkpoints     log_checkpoints
                                    tier 2 worker (opt-in): VS Code replay ──┘
 brain query search ──► brain.db (read-only snapshot)
 brain query text   ──► log_checkpoints → events.jsonl
```

* **Capture** reads keyboards directly from the kernel (evdev), passively, and
  resolves each key to the character the app receives with libxkbcommon and
  the user's real layouts. Focus (app id and window title) comes from a KWin
  script calling back over D-Bus. Every event is one JSON line appended to
  `events.jsonl`.
* **Ingest** (a thread in the same process) tails the log about once a
  second. In one SQLite transaction per batch it writes log checkpoints,
  opens, updates and closes writing sessions, reconstructs their text, and
  advances the log offset. The log is the ground truth; the database is
  derived from it and can be rebuilt from it.
* **Query** reads the database (`search`) or the log (`text`) while `run`
  keeps writing, and prints JSON.
* **Threads in `brain run`**: main (signals, orderly shutdown), input (the
  hot path), ingest, one small thread per watched D-Bus signal plus zbus's
  own connection threads, and with `--vscode-replay` a tier 2 worker. A real
  run has 13 threads.
* **Measured cost**: 0.1 % CPU and 8 MB peak RSS while typing at human speed
  with queries running (`tests/perf_budget.rs`); the budget is 1 % and 32 MB.

## 2. Research: collecting keystrokes and application data

### 2.1 The constraint that shapes everything: Wayland

On X11, any client can read global input (XRecord, XInput2 raw events) and the
active window (`_NET_ACTIVE_WINDOW`). Wayland removed both on purpose. Clients
only receive input while they have focus, and there is no standard protocol
for asking "which window is active". Global capture therefore needs one of:
kernel-level input access, cooperation from the compositor, or cooperation from
each application. Every approach below falls into one of those three groups.

### 2.2 Keystrokes: approaches compared

| # | Approach | Works on KDE Wayland? | What it sees | Permissions | Latency / CPU | Text fidelity | Robustness | Invasiveness / risk |
|---|---|---|---|---|---|---|---|---|
| A | **evdev read** (`/dev/input/event*`, non-grabbing) | Yes, independent of the display server | Raw keycodes + press/release/repeat, kernel timestamps | `input` group (or a udev ACL) | No added latency (passive parallel reader); a few µs per event | Keys, not text. Characters need an xkb keymap that mirrors the compositor's | Must handle hotplug, `ENODEV` after resume, `SYN_DROPPED` | Read-only; but `input` group can read **every** keyboard, a real security cost |
| B | **libinput as a library** (path/udev backend over the same nodes) | Yes | Same keycodes, normalized; adds touchpad/gesture processing | Same as A (udev backend also wants seat access) | Slightly higher than A (more processing, larger dependency) | Same as A | Device handling done for us, but its udev backend assumes it owns the seat | Same as A; also pulls in libinput, libudev, libevdev |
| C | **X11 XRecord / XInput2 raw events** | XWayland clients only | Keycodes/keysyms via the X server | None on X11 | Low | Keysyms; native Wayland apps invisible | Fine on X11 | Not viable on Wayland: Konsole, Firefox (Wayland), most KDE apps are native |
| D | **Grab + forward proxy** (`EVIOCGRAB` + re-emit via `/dev/uinput`; keyd/kmonad style) | Yes | Same as A, exclusive | `input` + write access to `/dev/uinput` (root-only here) | **Adds a hop to every keystroke** (read → uinput → kernel → compositor) | Same as A | If the proxy hangs, the keyboard is dead; crashes release the grab but drop held keys | High: brain sits *in* the input path, and that hop is exactly what "no input lag" forbids |
| E | **Kernel module / eBPF** (input handler, kprobe on the input core, HID-BPF) | Yes | Keycodes (HID-BPF: HID reports) | root / `CAP_BPF`+`CAP_PERFMON`; module signing under Secure Boot | Very low | Same as A | Breaks across kernel versions (kprobes are not ABI); module crash = kernel crash | Very high. Needs root for an end-user tool, and looks like a rootkit |
| F | **AT-SPI accessibility bus** (`text-changed`, `text-caret-moved`, focus events) | Yes, where apps cooperate | **Actual widget text**, insertions and deletions, caret, role (can skip `password text` fields) | None (session bus) | Event volume is per text change; enabling a11y in Chromium/Electron has a noticeable cost | Best for GTK/Qt widgets; Chrome/Electron need `--force-renderer-accessibility`; terminals expose little or nothing; many apps expose nothing | Apps must be started with a11y on; Qt needs a11y enabled | Low privilege, but changes app behaviour (a11y trees) and coverage is uneven |
| G | **Input-method framework** (IBus engine / Fcitx5 addon; on KWin via `input-method-v1`) | Only in IM-aware clients | Key events routed to the IM, **committed text** (handles compose/IME correctly) | None, but it must *be* the user's IM | Low | Correct committed text, but no view of edits inside the widget (backspace into existing text, mouse selection) | Clients without text-input support (some terminals, XWayland, games) are invisible | Replaces the user's IM (conflicts with CJK users' setup); every keystroke now depends on brain |
| H | **Compositor hook** (KWin C++ effect/plugin, or patched KWin) | Yes | Keys after the compositor's own xkb processing + the exact focused surface | Install into KWin's plugin dir | Very low | Best key-level fidelity: same keymap, same repeat, same focus as apps actually get | KWin's C++ plugin API is not stable; must rebuild for every KWin release | A bug crashes the whole desktop session. KWin *scripts* (JS) have no key-event API |
| I | **LD_PRELOAD / toolkit hooks** (wrap `wl_keyboard` listeners, Qt/GTK input) | Partly | Whatever the hooked process receives | None | Low | Good per process | Flatpak/Snap sandboxes, static binaries, setuid and already-running apps are missed; breaks on toolkit updates | Injects code into every app |
| J | **Per-app extensions** (browser extension, VS Code extension, shell `PROMPT_COMMAND`/zsh hooks) | Yes | **The real document state** (`onDidChangeTextDocument`, DOM `input` events, the executed command line) | Per-app install | Low | Highest possible, but only inside that app | Each app's extension API | One integration per app; doesn't meet "follows what they write in each application" by itself |
| K | **xdg-desktop-portal** (GlobalShortcuts, InputCapture, RemoteDesktop/EIS) | n/a | GlobalShortcuts: only registered shortcuts. InputCapture: input only after the pointer crosses a barrier (made for Synergy-style sharing). RemoteDesktop: *sends* input | User consent dialog | — | None of them give a passive global key stream | — | Not usable, by design |

**Notes on the close calls:**

- **evdev (A) vs libinput (B).** Both read the same device nodes. libinput's
  value is pointer acceleration, touchpad gestures and tablet handling, none
  of which a key logger needs. Reading evdev directly keeps the dependency
  set and the memory footprint small, and gives full control over hotplug and
  resync.
- **Why not grab (D).** A passive reader is not on the input path: the kernel
  delivers each event to every open evdev client independently, so if brain
  stalls the user notices nothing. A grab-and-forward proxy makes every
  keystroke wait on brain and fails with a frozen keyboard.
- **AT-SPI (F)** is the only system-wide source that sees real text rather
  than keys. Coverage decides against it: Chrome/Electron, VS Code and
  terminals, the main targets, are its weak spots. It remains the best
  candidate for a second source that corrects key-based reconstruction for
  GTK/Qt apps (section 10).
- **Compositor hook (H)** has the best fidelity, but a C++ plugin loaded into
  KWin breaks with every KWin release and a bug takes the desktop down. brain
  takes the one thing it needs from the compositor (focus) through KWin's
  supported scripting interface, and mirrors its keymap and repeat settings
  instead of hooking it.

**Decision: evdev (A), passive and never grabbed, with characters resolved by
libxkbcommon.** (`src/capture/input.rs`, `keys.rs`, `xkb.rs`)

- **Devices.** Every `/dev/input/event*` whose key bitmap has A, Z, Space and
  Enter is a keyboard. Each gets an event-type mask (`EVIOCSMASK`: only
  `EV_SYN` and `EV_KEY`), so the kernel never wakes brain for LEDs, scan codes
  or anything else. Mice and touchpads are not opened at all (section 10:
  a click says the cursor moved, not where to).
- **Hotplug.** inotify on `/dev/input` (`IN_CREATE`, and `IN_ATTRIB` because
  udev applies the `input` group permissions after creating the node). A
  device that disappears releases its held keys, so a modifier can never get
  stuck.
- **One poll loop.** The input thread sleeps in `poll(2)` on the keyboards,
  the inotify fd and an eventfd for control messages, with a timeout only
  while a key is held (for repeat). Per keystroke: one `read`, an xkbcommon
  update, JSON into a reused buffer, one `write`.
- **Characters.** One keymap from the compositor's own layout settings, read
  in KWin's order: `kxkbrc` (when its layout list is in use), the
  `XKB_DEFAULT_*` environment of the session, then systemd-localed. One
  modifier state shared by all keyboards, as KWin keeps it (Shift on one
  keyboard plus a letter on another gives a capital). KDE's layout switches
  are followed two ways: Alt+Shift is applied by xkbcommon itself (the same
  `grp:` option the compositor uses), and `org.kde.KeyboardLayouts`
  `layoutChanged`/`layoutListChanged` signals set the layout when it is
  switched from the tray or the list changes.
- **Shortcuts on the Latin layout.** With Ctrl, Alt or Super held, a key that
  is not Latin in the active layout is resolved in the first layout where it
  is (Ctrl+ש is logged as Ctrl+A), because that is how Qt, GTK and Chromium
  match shortcuts. Plain typing stays in the active layout.
- **Key repeat.** On Wayland the kernel's repeat events are not what apps
  see: each app repeats keys itself on the schedule the compositor
  advertises. brain ignores kernel repeats (`value == 2`) and synthesizes
  repeats on KDE's schedule (`kcminputrc`, default 600 ms delay, 25 Hz), for
  keys that repeat in the keymap, cancelled by release or by a new press.
  Repeats are emitted in time order with the transitions around them: on
  every wakeup the loop first reads every queued key event, then emits the
  repeats due by the time it sampled before reading.
- **Timestamps.** The kernel's timestamp of each event is logged, not the
  time brain read it.
- **Resync.** On `SYN_DROPPED` the real key state is re-read
  (`EVIOCGKEY`) and held keys reconciled.

### 2.3 Application and focus data: approaches compared

| Approach | Works on KDE Wayland? | What it gives | Permissions | Cost | Notes |
|---|---|---|---|---|---|
| **KWin script + D-Bus callback** (chosen) | Yes (and KWin X11) | Activation events, caption changes, `desktopFileName`, `resourceClass`, pid, for native Wayland *and* XWayland windows | None; loaded via KWin's own `org.kde.KWin /Scripting` interface | Event-driven, zero polling | Supported, stable JS API. The script runs inside the compositor but is sandboxed JS; a bug cannot crash KWin the way a C++ plugin can |
| `kdotool` | Yes | One-shot queries (active window, title) | None | Each call loads a temporary KWin script; polling would be expensive | Same mechanism, wrapped as a CLI. Good proof the approach is sound; unsuitable as an event source |
| `wlr-foreign-toplevel-management` | No (wlroots compositors: Sway, Hyprland, …) | Toplevel list + activated state | None | Event-driven | Not implemented by KWin. The newer `ext-foreign-toplevel-list-v1` deliberately carries **no** activation state, so it cannot answer "which window is focused" anyway |
| KDE `org_kde_plasma_window_management` | Only for privileged clients | Full window list + state (what the Plasma task manager uses) | KWin restricts it to trusted clients (desktop-file allowlisting) | Event-driven | Not meant for third-party tools; relies on KDE-internal trust configuration |
| X11 `_NET_ACTIVE_WINDOW` / `WM_CLASS` | XWayland only | Active *X* window | None | Event-driven (PropertyNotify) | Native Wayland windows are invisible to XWayland clients |
| AT-SPI focus events | Partly | Focused accessible object + its app | None | Event-driven | Same coverage problem as in 2.2; also gives no stable app id |
| GNOME Shell extension | GNOME only | Equivalent of the KWin script (`global.display` `notify::focus-window`) | Extension install | Event-driven | The GNOME port of our design; out of scope |
| `/proc` + compositor-agnostic heuristics | No | — | — | — | No way to know focus from process state |

**Decision: a KWin script that pushes focus changes into brain over D-Bus.**
(`src/capture/desktop.rs`, `kwin_focus.js`)

- `brain run` claims `org.shadowassistant.Brain.p<pid>` on the session bus
  and exports one method, `Activated(desktopFile, resourceClass, caption,
  renamed)`.
  It writes a small script and loads it through KWin's own
  `org.kde.KWin /Scripting` interface. The script reports
  `workspace.windowActivated` (`renamed = false`) and the active window's
  `captionChanged` (`renamed = true`), and the current window right away.
  Focus events in the log carry `"renamed": true` for title changes. Calls
  from anyone but KWin's bus name are ignored.
- The app id is the window's desktop file name (`org.kde.kate`,
  `google-chrome`, `com.microsoft.VSCode`), falling back to the X11
  resource class. Every key is stamped with the current app and window
  title, so each log line is self-describing.
- If KWin restarts (`NameOwnerChanged`), or is not up yet when brain starts
  as a service, the script is loaded as soon as KWin's name appears. A
  script left loaded by a `brain run` killed with -9 is unloaded by the next
  start.

**Screen lock.** `org.freedesktop.ScreenSaver` `ActiveChanged` (and
`GetActive` at start): while locked, keys are still read (to keep modifier
state right) but nothing is logged, so the unlock password never is. `lock`
and `unlock` are logged, and the focus is re-stated after unlock.

**Paste.** On Ctrl+V, Ctrl+Shift+V or Shift+Insert the clipboard is read from
Klipper (`getClipboardContents`, 300 ms timeout, up to 64 KiB) and logged as a
`paste` event right after the key. Copying alone is never recorded.

**Sleep/wake.** logind's `PrepareForSleep` on the system bus: `sleep` and
`wake` are logged, and on wake every keyboard is closed and reopened (held
keys released). Tested with a real suspend through `systemctl suspend`.

## 3. Event format and ingestion

### 3.1 The capture log

`events.jsonl`, one JSON object per line, in the assignment's format plus the
window title:

```json
{"ts":"2026-11-16T11:37:56.937Z","app":"org.kde.kate","type":"key","key":"H","mods":["shift"],"window":"notes.md — Kate"}
{"ts":"2026-11-16T11:37:57.132Z","app":"org.kde.kate","type":"key","key":"Backspace","mods":[],"repeat":true,"window":"notes.md — Kate"}
{"ts":"2026-11-16T11:38:00.003Z","app":"google-chrome","type":"focus","window":"New tab - Google Chrome"}
{"ts":"2026-11-16T11:38:01.120Z","app":"google-chrome","type":"focus","window":"brain - Google Search - Google Chrome","renamed":true}
{"ts":"2026-11-16T11:38:02.410Z","app":"google-chrome","type":"paste","text":"pasted words","window":"New tab - Google Chrome"}
{"ts":"2026-11-16T11:36:48.298Z","app":"org.kde.kate","type":"system","state":"sleep"}
```

Types: `key` (with `repeat: true` for synthesized repeats), `focus` (a
window activated, or with `renamed: true` the active window's title
changed), `paste`, and `system` (`start`, `stop`,
`sleep`, `wake`, `lock`, `unlock`). Timestamps are RFC 3339 UTC with
milliseconds: fixed width, so they sort as strings.

### 3.2 Write path

- One `write(2)` per event, of a complete line, from the input thread (or a
  D-Bus thread for focus, lock and sleep events) under one mutex. The event
  is in the kernel's page cache when the call returns, so a crash of brain
  loses nothing; a power loss can lose the last second.
- `fdatasync` at most once per second, from the ingest thread, only if
  something was written.
- On open, a torn last line (a `kill -9` or power loss mid-write) is cut off;
  readers only ever return complete lines.

### 3.3 Ingest

Once a second, the ingest thread syncs the log and runs steps of at most 5,000
lines until it has caught up. **Each step is one SQLite transaction** that
holds everything derived from those lines and the new log offset:

1. read complete lines from `ingest_state.log_offset`;
2. write a log checkpoint at most every 5 s of log time;
3. feed each event to the session engine (section 4);
4. once caught up, close sessions idle on the wall clock, write finished tier
   2 results, refresh the live text of open sessions;
5. store the new offset and commit.

If the commit fails nothing is visible and the same lines are read again; the
in-memory session state is rebuilt from the database first, so it cannot run
ahead. After the commit, tier 2 jobs are handed to the worker.

**Why a thread in the same process**: one binary and one lock on the data
directory, no IPC, and ingest can never run without capture or vice versa.
The capture path never touches SQLite, so a slow transaction cannot delay
reading the keyboard.

**Why not write keystrokes to SQLite directly**: a SQLite write can stall
(WAL checkpoints, FTS updates, lock contention), and a stalled reader of the
keyboard overflows the kernel's small buffer and loses keys. The append-only
log also makes the database disposable: a schema change, or a bug in session
logic, is fixed by deleting `brain.db`, which is rebuilt from the log.

### 3.4 `kill -9` at each point

| Killed while... | After restart |
|---|---|
| writing a log line | the torn line is cut off; everything before it is intact |
| between a write and the next ingest | ingest resumes from its offset and reads the new lines |
| inside an ingest transaction | SQLite rolls it back; the offset still points before those lines, so they are read again: no gap, no duplicate |
| with sessions open | open sessions are rebuilt from the database and their events replayed from the log (section 4.5); the restart's `start` event then closes them |

`tests/crash_safety.rs` kills brain 25 times under load and checks database
integrity, offsets on line boundaries, a complete log, and that every session,
document and checkpoint equals a clean rebuild from the log.

## 4. Sessions and reconstruction

(`src/sessions.rs`, `src/reconstruct/`)

### 4.1 Definition

- A session is one **`(app, window title)`**. The title is normalized first:
  leading status glyphs are removed (an editor's unsaved-file dot, the
  spinners terminal programs animate in titles) and so are `*` / `[modified]`
  markers, because they change while typing.
- **Titles renamed by the typing itself** keep the session: VS Code names a
  new file after its first line, a mail window after its subject. A title
  change *of the same window* (a `renamed` focus event) within 1 s of a
  keystroke in it, with no shortcut in between (Ctrl+Tab, Alt+Tab may switch
  tabs or windows), makes the new title an alias of the open session. A
  window activation, by keyboard or mouse, ends any possible rename.
- A session **closes** after **30 s without activity in that window**, or at
  once when a single-line field commits (Enter at a shell prompt, in a chat
  box, an address bar or a launcher), is cancelled (Ctrl+C at a prompt), or
  is left for another field of the same window (Tab, Ctrl+L/K/T/W in a
  browser), and when `brain run` stops or restarts.
- **Focus loss alone never closes a session**: switching away and back
  within 30 s of that window's last keystroke resumes it.
- Shortcuts with no modelled effect (Ctrl+C copy, Alt+Tab, Ctrl+S, F-keys)
  are not activity: they neither open nor extend a session.
- A session that changed nothing (cursor movement only, a typo typed and
  deleted, whitespace) leaves no row; a committed non-empty line always
  does (a shell command run again from history).
- **Idle closing on the clock** happens only once ingest has caught up with
  the log, and 2 s past the 30 s timeout, so it always agrees with the same
  rule applied to event times on a rebuild.

### 4.2 Text across sessions

Editors and unknown (multi-line) apps keep a **document per window** between
sessions (`context_state`, forgotten after 24 h), so the next session
continues where the previous one left it, cursor included. A session's text
is that document when the session closes: a word present at close stays
findable through that session even if a later session deletes it.
Single-line fields (terminals, browsers, chat, launchers) start every session
empty: their titles are reused by unrelated fields ("New tab", "~"), so
carrying text over would mix them.

### 4.3 The engine: incremental, in memory, from the log

Ingest keeps one entry per open session in memory: its row id, a text buffer
(the reconstruction so far, with cursor, selection and undo history), and its
last activity. Each logged event is routed by its own `(app, window)` to its
session and applied once, the moment ingest reads it; switching between apps
is just a different map entry. The log interleaves apps, but filtering it per
key preserves each window's order. Every ingest step writes the current text
of changed open sessions, so `search` sees what is being typed now. Closing a
session writes the text it already has.

*Differs from the design*, which reconstructed each session at close and
rebuilt open sessions just in time for live search. The result is the same;
incremental work costs a constant amount per key instead of growing with the
session.

### 4.4 Reconstruction tiers

- **Tier 0, a generic text field** (`reconstruct/buffer.rs`): insertion
  replacing the selection, Backspace/Delete (word-wise with Ctrl), arrows,
  Home/End, Up/Down keeping the column, PageUp/PageDown, Shift-selection,
  select-all, cut, and undo/redo grouped about a word at a time. A 200,000
  character cap per buffer keeps memory bounded. An unknown cursor position
  is assumed to be at the end.
- **Tier 1, per-app rules** (`reconstruct/apps.rs`, `editor.rs`): a small
  table classifies app ids into terminal, browser, chat, launcher, editor or
  generic. It decides whether Enter commits or inserts a newline (Shift+Enter
  is a newline in browsers and chat), readline editing in terminals
  (Ctrl+A/E/U/K/W, Alt+B/F) and history recall with Up/Down from the commands
  brain saw committed in that terminal app, browser field changes, and that
  plain Ctrl+V does not paste in a terminal.
- **Tier 2, VS Code replayed in VS Code** (`reconstruct/vscode/`,
  `tier2.rs`, opt-in with `--vscode-replay`): when a VS Code session opens,
  the file is identified from the window title (an absolute path, or the
  default title resolved through VS Code's recently opened folders) and
  copied, unless it had unsaved changes or changed on disk after the session
  started. When the session closes with its tier 0/1 text, a worker replays
  the session's own keystrokes from the log into one warm, headless VS Code
  (Electron's headless mode, no display server) over the DevTools protocol on
  a pipe, in its own network namespace and a CPU/memory-capped systemd scope,
  with the user's settings and keybindings. It saves and the session's text
  becomes the changed lines. Failures and timeouts keep the tier 0/1 text;
  pending jobs survive restarts.

*Differs from the design*: no Xvfb (headless Electron needs no display), and
the CDP connection is a pipe, because the network namespace makes a TCP port
unreachable.

### 4.5 Restart with open sessions

On start, ingest loads the sessions still open in the database, restores
their starting documents from `context_state`, and replays their events from
each session's `start_offset` up to the ingested offset, through the same
key-to-action rules. The `start` event that follows closes them (input while
brain was not running is unknown). Everything the engine decides is a
function of the log, so a database rebuilt from the log alone is identical;
`sessions::tests::crash_recovery_reproduces_an_uninterrupted_run` restarts
ingest after every line to prove it.

## 5. Storage and indexing

### 5.1 Layout

| Path | What |
|---|---|
| `events.jsonl` | the capture log (0600), ground truth |
| `brain.db` (+ `-wal`, `-shm`) | SQLite index, derived |
| `run.lock` | flock held by the running `brain run` |
| `kwin/` | the generated KWin script |
| `vscode/` | tier 2 sandbox and file snapshots |

The directory is created 0700 and brain runs with umask 077.

### 5.2 Schema (version 4)

- **`sessions`**: `id`, `app`, `window` (normalized), `started_at`,
  `ended_at` (NULL while open), `status`, `tier_used`, `reconstructed_text`,
  `start_offset` / `end_offset` (the session's events in the log),
  `end_reason` (`idle`, `commit`, `cancel`, `field`, `stop`, `restart`),
  `tier2_status`, `tier2_file`. Indexes on `(app, started_at)`, `status`, and
  `(app, end_reason, end_offset)` for shell history.
- **`sessions_fts`**: FTS5 over `reconstructed_text`, external content (the
  text is stored once, in `sessions`), kept in sync by insert/update/delete
  triggers in the same transaction.
- **`context_state`**: each multi-line window's document between sessions.
- **`ingest_state`**: the single log offset.
- **`log_checkpoints`**: `(ts, byte_offset)`, at most one per 5 s of log time.

There are no migrations (first version): a database from another schema
version is refused with a message to delete it.

### 5.3 Why SQLite, WAL and FTS5

- **WAL**: one writer (ingest) never blocks readers, and each `brain query`
  sees a snapshot as of the start of its read: never a half-written session.
- **`synchronous = NORMAL`**: no fsync per commit. A power loss can drop the
  last commits but never corrupts the database, and the dropped work is
  re-derived from the log because the offset rolls back with it.
- **FTS5** is in SQLite itself (bundled, no server), and external content
  avoids storing session text twice.
- A 1 MiB page cache keeps the writer inside the memory budget.

### 5.4 Tokenizer: trigram

The index uses FTS5's **trigram** tokenizer, case- and accent-insensitive: a
query word matches anywhere inside a word (`run` finds "rerun", `cafe` finds
"Café"). *Differs from the design* (porter stemming), on the owner's call:
search should behave as "contains", and porter is English-only, while
trigram finds Hebrew words with attached prefixes (`בית` in "בבית"). Words
shorter than 3 characters cannot use the trigram index and are matched with
`LIKE`.

### 5.5 Log checkpoints

`query text` must read the keys of a time window from the log without
scanning it from the start. Each checkpoint says "the event at this byte has
this timestamp". Because the log is written in time order, starting at the
latest checkpoint at or before the window misses nothing, and reads at most
5 s of log before the window; reading stops 60 s of log after it. Per-app
checkpoints would not save work, since the log interleaves apps and the
window has to be read anyway. Clock steps backwards are discussed in section
10.

## 6. Query semantics

(`src/query.rs`)

- **`brain query search "<words>" [--app A] [--from T] [--to T] [--at T]
  [--limit N]`**: sessions whose text contains every word (substrings, any
  case), newest first, default limit 20, with a snippet around the first
  match. Open sessions are included with their live text and
  `ended_at: null`. A session is in a time window if it overlaps it.
- **`brain query text --app A [--from T] [--to T] [--at T]`**: what was typed
  into the app during the time range, reconstructed. The range's key and
  paste events (read from the log, section 5.5) are split per app window
  (window title) and each app window's events run, from an empty text,
  through the app's tier 0/1 adapter: the same `editor::action` and
  `FieldEditor` the session engine uses, without sessions. A submit in a
  single-line field (Enter at a shell prompt) keeps the line and continues
  on a fresh one; Up recalls lines submitted earlier in the same app window.
  Output: `windows` (each app window's text, by first keystroke) and `text`
  (them joined, one per line). *Differs from the design*, where `text` was
  the raw keystroke stream; the owner changed it.
- **`--app A`** can be part of an app id, ignoring case. `search` keeps
  every matching app; `text` resolves it to one full id (exact match first,
  else the single id containing it; ambiguous or unknown is an error) and
  reports that id.
- **Windows**: `--at T` is the 10 minutes ending at T; `--from` alone runs to
  now; `--from` and `--to` together are explicit; `--to` alone is the 10
  minutes ending at T for `text` and everything up to T for `search`; no
  flags means the last 10 minutes for `text` and all time for `search`.
- **Times**: relative (`-30s`, `-15m`, `-2h`, `-1d`, `-1h30m`, `now`), local
  (`2026-11-16 14:00`, a date, or a time of day meaning today; DST folds
  take the earlier instant, DST gaps are an error), or RFC 3339 with a zone.
  Output times are local with their offset (RFC 3339), and the resolved
  window is echoed; the log and the database store UTC.
- Errors (empty query, bad time, empty window, `--at` with `--from`/`--to`,
  missing data directory) go to stderr with a non-zero exit code.

## 7. Resource budget

- **No input lag by construction**: brain is a passive reader, not on the
  delivery path. Even a completely stalled brain cannot delay a keystroke.
- **CPU**: no async runtime and no polling. The input thread sleeps in
  `poll` and wakes per key event (and per repeat while a key is held); one
  `write` per event; one small transaction per second; SQLite work per
  session proportional to its text.
- **Memory**: few threads with small stacks, a 1 MiB SQLite cache, a capped
  buffer per open session (usually one to three are open), no copy of closed
  sessions in memory.
- **Measured** (`tests/perf_budget.rs`, part of `cargo test`): 8 keys/s for
  30 s through the whole pipeline with a query every 3 s: **0.1 % CPU, 8.1 MB
  peak RSS**. A real run idles at about 8 MB.
- The opt-in VS Code instance runs outside brain's budget, in its own
  systemd scope capped at 50 % CPU and 1.5 GB, shut down after 10 minutes
  idle.

## 8. Security and privacy

- **Local only.** No network code in brain; the VS Code replay instance runs
  in a network namespace with only loopback.
- **Data at rest**: directory 0700, files 0600.
- **Privilege**: brain runs as the user, as a systemd *user* service if
  wanted (`make install-service`), never as root. Reading keyboards needs
  the `input` group, which lets any process of the user read all keyboards;
  the README states this trade-off.
- **Passwords**: capture pauses while the screen is locked. Other password
  prompts (sudo in a terminal, web logins) cannot be told apart from typing
  by key events alone (section 10).
- **Clipboard**: read only on a paste shortcut.
- **KWin integration** only reads window metadata and calls one D-Bus
  method; calls not coming from KWin are ignored.
- **Queries**: search words are quoted for FTS5 and all SQL uses bound
  parameters.
- **Tests that need privileges** get exactly two narrow ones
  (`scripts/setup-test-permissions.sh`): passwordless `rtcwake -m no -s 30`
  only, and `/dev/uinput` for the user at the local seat.

## 9. Testing strategy

- **Unit tests** (about 120, `cargo test`): key resolution and layouts,
  repeat synthesis (including a 300-scenario stress test against an
  independent model, with random early and late wakeups), the recorder, the
  log's torn-tail repair, schema and search, ingest, the tier 0 buffer and
  tier 1 rules, title normalization and renames, every session rule, crash
  recovery after every line, time parsing, query windows, and `query text`'s
  per-window reconstruction (the README's example, event for event).
- **Integration tests run by default**: `crash_safety` (25 `kill -9`s under
  load, compared with a clean rebuild), `query_cli` (the binary's JSON
  output, search semantics through the CLI, time windows, reconstructed
  text including the README's example, errors, and thousands of queries
  while `run` writes), `perf_budget` (CPU and
  memory budget).
- **Opt-in tests against real software**: `repeat_e2e` and
  `real_keyboard_e2e` type through a uinput keyboard into real Kate windows
  and compare Kate's saved files with brain (held-key repeats, Hebrew via
  Alt+Shift, focus moving between windows, keyboard unplug/replug);
  `vscode_replay` and `tier2_e2e` drive real VS Code.
- **Manual, once**: sleep/wake through a real suspend, screen lock and
  unlock, the systemd service (start, restart after `kill -9`, clean stop).

## 10. Limitations and future work

- **Keys, not text.** What an app does with a key is invisible: autocomplete,
  auto-closing brackets (except in VS Code with tier 2), IME conversion, tab
  completion and zsh autosuggestions in shells, Ctrl+R history search.
  Shell history recall only knows commands brain saw being run.
- **The mouse is not captured.** A click tells brain the cursor moved but not
  where in the text. Text edited after moving the cursor with the mouse
  (clicking into a line, mouse selection, drag and drop) is not supported:
  typing continues from where the keyboard last left the cursor. Switching
  *tabs* of one window with the mouse within 1 s of a keystroke would be
  taken as a title rename (another window being activated never is).
- **Held keys.** brain logs KDE's repeat schedule exactly, but each app runs
  its own repeat timer; Qt's may fire up to 5 % early or late, so a held key
  can produce one character more or less in the app than in brain.
- **Compose, dead keys, IME** are not interpreted; dead keys are logged by
  name.
- **Clock steps backwards** (NTP corrections, manual changes) break the
  "log in time order" assumption `query text` relies on: events in its window
  written after such a step can be missed. Both bounds can be made exact using
  the checkpoints' 5 s spacing (start at the earliest checkpoint within 5 s
  before the window, stop after the last one within 5 s after it); not done.
- **Other password prompts** are captured. AT-SPI's `password text` role
  would allow skipping them in apps that expose it.
- **VS Code tier 2** assumes the cursor starts at the end of the file,
  replays faster than a human (so accepting a suggestion may not replay the
  same), runs without extensions when they exceed 300 MB, and is the one part
  of `brain.db` a rebuild from the log does not reproduce (its file snapshot
  must be taken within 10 s of the session starting).
- **Desktop scope**: focus tracking needs KWin. On GNOME the equivalent would
  be a Shell extension; on wlroots compositors, `wlr-foreign-toplevel`. Keys
  are captured on any Linux, attributed to `unknown`.
- **Future work**: AT-SPI as a second source for GTK/Qt apps; the
  sandboxed-replay approach of tier 2 for terminals (a pty with the user's
  shell) and Chrome (CDP with a companion extension); retention and
  compaction of old log data.
