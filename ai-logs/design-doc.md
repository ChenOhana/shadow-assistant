# brain — system design

Nov 15, 2026 · @Chen Ohana

## Overview

`brain` is a personal CLI tool, built for the Endpoint "Shadow Assistant" home exercise, that captures everything typed on this Ubuntu machine and reconstructs it into searchable, queryable text.

One binary, two commands:

- `brain run` — captures keystrokes and focus changes globally, works out what was actually written in each app, and stores it.
- `brain query` — answers `search` and `text` questions about what's been captured, while `run` keeps going.

The pipeline has three stages, matching the assignment's own split:

1. **Capture** (hot path, must stay cheap): reads raw key events, resolves each one to a character using the live keyboard layout and modifier state, and appends it — along with focus-change events — to a durable, append-only log.
2. **Ingest** (cold path): tails that log, groups events into writing sessions, and reconstructs each session's text once it closes, writing the result into a queryable, full-text-indexed store.
3. **Query**: reads that store — `search` for full-text lookups across sessions, `text` for the raw captured stream over a time window — concurrently with `run`, always seeing a consistent state.

Reconstruction uses a tiered strategy depending on the app:

- **Tier 0 — generic buffer replay** (always available): the decoded character stream is replayed through a virtual cursor/selection text buffer (backspace, delete, arrows, Ctrl+A, paste-from-clipboard-log). Covers any app adequately, including the terminal and Chrome in this build.
- **Tier 1 — static per-app rules**: documented formatting conventions layered on top of tier 0, where it's cheap to add.
- **Tier 2 — real-app replay**: for VS Code only in this build, replay the actual captured keystrokes into a sandboxed, invisible instance of the real editor and read back its own output.

Every session gets a usable reconstruction regardless of tier — tier 2 is additive precision for VS Code, never a dependency for anything else to work.

## CLI reference

Both commands take `--data-dir <dir>` (default `./brain-data`) and exit non-zero with a message on stderr on error.

### `brain run`

```bash
brain run [--data-dir <dir>]
```

Runs until stopped (Ctrl+C or killed). Captures continuously; no other flags.

### `brain query search`

```bash
brain query search "<query>" [--app A] [--from T] [--to T] [--limit N] [--data-dir <dir>]
```

One or more words, case-insensitive, ANDed together — every word must appear in the same session's reconstructed text. Prints JSON:

```json
{
  "results": [
    {
      "session_id": "3f9a1c20",
      "app": "com.tinyspeck.slackmacgap",
      "started_at": "2026-11-08T14:03:21.950Z",
      "ended_at": "2026-11-08T14:09:51.004Z",
      "snippet": "…the migration is blocked on the schema change…"
    }
  ]
}
```

A word typed and fully deleted within the same session never appears here; a word present when a session closed stays findable through that session's record even if a later session on the same app deletes it from the live document — see Session definition below.

### `brain query text`

```bash
brain query text --app A [--at T] [--from T] [--to T] [--data-dir <dir>]
```

Returns the **raw decoded keystroke stream** for app `A` — exactly the characters captured in order, with no backspace/cursor editing applied — over a time window:

- No time flag given: the last 10 minutes, ending now.
- `--at T`: a 10-minute window ending at `T` (equivalent to `--to T` with the default window length).
- `--from T` / `--to T`: an explicit window; either bound can be given alone.

```json
{ "app": "com.apple.Notes", "at": null, "text": "Goals\n- ship search\n- …" }
```

Deliberately distinct from `search`'s session-reconstructed, edit-aware text — see Decisions log below for why the two diverge.

## Session definition

A session is keyed by `(app, window/context identity)`. One session accumulates all keystrokes typed into that specific context until it closes.

**A session closes when:**

- 30 seconds pass with no keystrokes in that context (idle timeout), or
- a commit key fires in a single-line context — Enter in a terminal shell prompt, a chat compose box, or a browser address bar — closing it immediately, even before the idle timeout.

**A session does not close on focus loss.** Switching to another app or window pauses that context's session without ending it. If the user returns to the same `(app, window)` context before its 30-second idle timeout has elapsed — counted from that context's own last keystroke, regardless of what happened elsewhere in between — the same session resumes and keeps accumulating. Only once 30 seconds of inactivity in that specific context has passed does it close; a later return to that context starts a new one.

**Single-line vs. multi-line contexts** are distinguished by a small, explicit app-class table (terminal prompts, chat compose boxes, and address/search bars are single-line; code editors, notes apps, and multi-line text areas are not) — Enter only acts as a commit key in the former; in the latter it's literal text passed through tier 0 like any other character.

**Why this shape**: it matches how people actually write — a paragraph composed with a few seconds' pause to think, or a quick alt-tab to check something and back, reads as one session; walking away for a coffee, or genuinely switching tasks, correctly starts a new one. It also keeps session boundaries decoupled from window-manager focus churn, which is noisy and not a reliable signal of writing intent by itself.

## Data model

The capture log is the ground truth; SQLite is a derived, rebuildable query index fed by ingest.

**Capture log** (append-only JSONL, one line per event, matching the assignment's own example format):

```json
{"ts":"2026-11-08T14:03:22.118Z","app":"org.example.app","type":"key","key":"H","mods":["shift"]}
{"ts":"2026-11-08T14:03:25.400Z","app":"org.example.app","type":"focus","window":"Untitled — Notes"}
```

Each line is complete and independently parseable, appended with a trailing newline and fsync'd in small batches. A `kill -9` mid-write can leave at most one torn trailing line, which ingest detects (invalid JSON or missing newline) and discards on restart — never a corrupted or ambiguous history.

**SQLite (WAL mode, derived)**:

```sql
PRAGMA journal_mode = WAL;

CREATE TABLE sessions (
  id                 INTEGER PRIMARY KEY,
  app                TEXT NOT NULL,
  window             TEXT NOT NULL,
  started_at         TEXT NOT NULL,   -- RFC3339, matches the log's ts format
  ended_at           TEXT,
  status             TEXT NOT NULL DEFAULT 'open',   -- open | closed
  tier_used          INTEGER,
  reconstructed_text TEXT
);
CREATE INDEX idx_sessions_app_time ON sessions(app, started_at);
CREATE INDEX idx_sessions_status ON sessions(status);

CREATE VIRTUAL TABLE sessions_fts USING fts5(
  reconstructed_text, content='sessions', content_rowid='id',
  tokenize='porter unicode61'
);
-- + AFTER INSERT/UPDATE/DELETE triggers keep sessions_fts in sync.

CREATE TABLE ingest_state (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  log_offset INTEGER NOT NULL DEFAULT 0   -- byte offset into the capture log already ingested
);

CREATE TABLE log_checkpoints (
  app         TEXT NOT NULL,
  ts          TEXT NOT NULL,
  byte_offset INTEGER NOT NULL,
  PRIMARY KEY (app, ts)
);
```

Raw keystrokes aren't duplicated into a SQLite table at all — `query text` reads the capture log directly for a given app and time window, using `log_checkpoints` (written every few seconds per app) to seek near the right byte offset instead of scanning from the start.

## Capture, ingestion and concurrency

**Hot path** (runs per keystroke, must stay cheap): read the raw key event, resolve it to a character using the live XKB keymap and current modifier state (`key` already reflects shift/caps-lock/layout, per the capture-time resolution decided for this build), and append one JSON line to the capture log — plus one line per focus-change event. Small batched fsyncs (every \~50ms or N events) bound both latency and syscall overhead.

**Ingest** (cold path, same `brain run` process): tails the capture log from the last `ingest_state.log_offset`, assigns each event to a session per the session-definition rules, and advances the offset in the same SQLite transaction as any session data it writes — offset and data can never drift apart, even across a crash. On startup, ingest always resumes from the persisted offset, re-processing only what wasn't yet committed.

**Concurrency**: SQLite in WAL mode — the one writer (ingest) never blocks readers, and readers (`brain query`) never block the writer; each reader sees a consistent snapshot as of when its read began, never a half-written session. `brain query text` reads the capture log directly for the given app and time window; `brain query search` hits `sessions_fts` for everything already closed and reconstructed, plus a just-in-time tier-0 reconstruction of whatever session is currently open for a live-search path.

**Reconstruction**: once a session closes, ingest reconstructs it (tier 0/1 inline; tier 2 for VS Code handed to a small, separately bounded task with a timeout, falling back to tier 0 on failure) and writes `reconstructed_text`, which the FTS5 trigger indexes automatically.

## Reconstruction pipeline

&#91;embedded content: reconstruction dispatch · VS Code is the only tier-2 app\]

A tier 2 failure or timeout falls back to tier 0 for that session rather than losing it — tier 2 is additive precision for VS Code, never a hard dependency. Every adapter implements the same interface so the pool can treat them uniformly:

```rust
trait Tier2Adapter {
    fn reconstruct(&self, session: &Session, keystrokes: &[Keystroke]) -> Result<String, ReconstructError>;
}
```

## Chrome and the terminal: tier 0/1 only

Earlier drafts of this design sandboxed a real, invisible instance of each app (Chrome via the DevTools Protocol, the terminal via a headless pty replaying into the user's real shell) to get an exact replay. For this build, both are scoped back to tier 0/1 — the same generic buffer-replay engine as every other app — to keep implementation effort proportionate to a take-home rather than a production system; VS Code is the one case where the extra precision was judged worth it (below).

**What this costs**: Chrome and the terminal lose exact handling of app-specific behavior — a web page's own autocomplete/autoformat JS, or a shell's readline history search and tab completion — that only the real engine reproduces faithfully. Tier 0 still correctly handles plain typing, backspace, cursor movement and Ctrl+V-from-clipboard-log for both.

**Future work** (see Known limitations): the full sandboxed-replay design for both is kept here as the path to exact reconstruction if this is extended beyond the exercise.

*Chrome, sketched*: sandbox on an isolated, network-isolated profile with `--remote-debugging-port`; dispatch keystrokes via CDP's `Input.dispatchKeyEvent` into the focused element (needs a companion extension logging `url` plus a stable selector per focus change, since a page title alone doesn't disambiguate a field); read back via `Runtime.evaluate`. The browser's own native UI (the omnibox) isn't page DOM, so it would need AT-SPI instead.

*Terminal, sketched*: no `Xvfb` needed — open a pty, spawn the user's real shell with their real `.bashrc`/`inputrc` loaded, feed it the session's keystrokes, and parse the ANSI output with a headless terminal-state emulator (e.g. `alacritty_terminal`'s grid, unrendered) to read the current line. Only applies to bare shell-prompt sessions; a full-screen program (vim, `less`) taking over the terminal needs its own handling.

## Tier 2 adapter: VS Code

VS Code is Electron, so the same CDP mechanism as Chrome applies, with one important difference: there's no need to separately parse `keybindings.json` and reimplement VS Code's keybinding resolution (including the `when`-clause context problem noted earlier) — running the real app with the user's real config resolves all of that correctly, for free.

Launch headlessly on `Xvfb`, pointed at the user's actual config:

```bash
DISPLAY=:99 code \
  --user-data-dir=/var/lib/brain/replay/vscode-userdata \
  --extensions-dir=<copy of the user's real extensions dir> \
  <file path parsed from the session's window_title>
```

VS Code's title bar already includes the open file's path in `window_title`, so — unlike Chrome — no companion capture extension is needed to know what to open. Replay the keystrokes via CDP `Input` events into the focused Monaco editor, exactly as the real app received them, so every keybinding (including multi-key chords and extension-contributed commands) resolves the same way it did originally.

**Reading the result back**: rather than reaching into Monaco's internal model through `Runtime.evaluate` (fragile — it's not a stable public API), let the replay finish with a real `Ctrl+S` against an isolated scratch copy of the file, then just read that file off disk. Simpler and more robust than DOM/JS introspection.

**Known gap**: the file's content at the *start* of the session must match what the user actually had open — snapshot the file (or the relevant git blob) when the session opens, rather than assuming the current on-disk content still matches, since other edits can happen between then and reconstruction time.

This is the only tier 2 adapter built for this exercise — see "Chrome and the terminal" above for why those two stay at tier 0/1, and Known limitations for the trade-off this implies.

## Safety, resource limits and sleep/wake

- **Resource budget**: the assignment requires ≤1% average CPU while typing and ≤32MB memory for `run`. The hot path (evdev read, one XKB lookup, one JSON-line append) should hold this comfortably — verify it directly rather than assuming (see Testing and verification below).
- **Sleep/wake**: evdev file descriptors and the focus-tracking connection can go stale across a suspend/resume cycle. Subscribe to `systemd-logind`'s `PrepareForSleep` D-Bus signal; on the resume edge, re-open the input device handles and the focus-tracking connection before resuming capture.
- **Network isolation** is mandatory for the VS Code tier 2 adapter if its extensions have live network access — a sandboxed-but-connected replay that triggers a sync or telemetry call is a real side effect, not just a reconstruction.
- **One warm instance**, not one per session — Electron startup cost is real; keep the VS Code sandbox running and reset it (fresh scratch file) between sessions.
- **Resource caps**: the VS Code sandbox runs under a `systemd-run --scope` cgroup with `CPUQuota`/`MemoryMax` limits, independent of `brain run`'s own budget.
- **Bounded concurrency**: tier 2 jobs are serialized (one VS Code replay at a time), with a timeout that falls back to tier 0 on expiry rather than blocking the ingest queue.

## Known limitations and future work

- Chrome and the terminal run at tier 0/1 only for this build; the sandboxed-replay design for both is written up above as the path to exact reconstruction, not implemented.
- VS Code tier 2 depends on the edited file's content matching at session start; drift from concurrent external edits isn't detected automatically — worth a content-hash check that flags, rather than silently mis-reconstructs, a mismatch.
- `text --at T` reconstructs by replaying from session start up to `T`; for a very long-running session this could be slow without periodic snapshots — worth a checkpoint every N events if session lengths in practice warrant it.
- The tier 2 adapter interface is intentionally generic — adding Chrome or the terminal back as a full adapter later is a new adapter behind the same interface, not a pipeline redesign.
- Single-user, single-machine design throughout — no multi-user isolation or remote sync, which the assignment doesn't ask for.

## Testing and verification plan

Mapped directly to the assignment's hard requirements:

- **No input lag / ≤1% CPU / ≤32MB memory**: a load-test harness that synthesizes a realistic typing stream (replaying a text corpus at human keystroke timing) while sampling `run`'s CPU and RSS; assert against both thresholds as part of CI, not just checked once by hand.
- **Crash safety (`kill -9` + restart causes no corruption)**: an automated test that starts `run`, feeds it a known stream, sends `SIGKILL` at a randomized point (including mid-write), restarts `run`, and asserts: (a) `PRAGMA integrity_check` passes, (b) the capture log has at most one trailing torn line, (c) re-ingesting from the persisted offset reproduces exactly the events durably appended before the kill — no duplicates, no gaps.
- **Live queries see a consistent state**: a concurrent test running `query` in a tight loop against `run` under continuous synthetic typing, asserting every response is valid JSON representing a state that existed at some real point in time — never a half-written session or a torn reconstruction.
- **Search semantics** (a word typed and deleted within a session doesn't count; a word present at session end stays findable even after a later session deletes it elsewhere): unit tests directly against the reconstruction engine covering both halves of that rule.
- **`text --at T`**: unit tests reconstructing a known keystroke sequence and asserting the output at several points in time matches hand-computed expected state at each `T`.
- **Sleep/wake**: a test that simulates a `PrepareForSleep` signal pair around continued typing and asserts capture resumes without dropping or duplicating events.

## Decisions log

Compiled for direct reuse in README.md's "Decisions and edge cases" section.

- **Target OS**: Ubuntu/Linux. The assignment's example event uses a macOS-style app id; this build targets Linux throughout (evdev, XKB, GNOME/AT-SPI), with the OS choice called out explicitly since the spec leaves it open.
- **Character resolution point**: resolved at capture time, using live XKB keymap + modifier state, matching the assignment's own example format (`key` + `mods` already resolved, not a raw keycode). Analysis/tier 0 only handles buffer logic (cursor, backspace, selection), not character decoding.
- **Capture sink**: an append-only JSONL log is the durable ground truth; a separate ingest stage (same process) tails it into SQLite+FTS5, which is a derived, rebuildable index. Chosen for the simplest possible crash-safety story, and because the spec treats "collect" and "ingest" as distinct design questions.
- **Session boundaries**: keyed by `(app, window/context)`; closes after 30s of no keystrokes in that context, or immediately on a commit key (Enter) in single-line contexts. Focus loss alone never ends a session — returning to the same context within the 30s idle window resumes it.
- **`query text` semantics**: returns the raw decoded keystroke stream (no backspace/cursor editing applied) over a time window — `--at T` (10-minute window ending at T), `--from`/`--to`, or the last 10 minutes by default. Deliberately distinct from `search`'s session-reconstructed, edit-aware text — the two serve different purposes (an audit-style raw trail vs. finding what was actually written).
- **Tier 2 scope**: VS Code only. Chrome and the terminal get tier 0/1; the full sandboxed-replay design for both is documented as future work rather than built, to keep effort proportionate to the exercise.
- **`text --at <time>` (marked "Level 5" in the PDF)**: treated as required, not optional — it's explicit in the CLI spec, and levels elsewhere in the PDF weren't legible in extraction.

## Mapping to assignment deliverables

- **PLAN.md**: Research & Architecture Design maps onto this doc's Data model, Session definition, Capture/ingestion/concurrency, and Reconstruction pipeline sections — including a comparison of capture approaches (evdev raw input vs. X11 XRecord vs. accessibility-bus hooking vs. a kernel-level driver) for PLAN.md's "how you collect" question, weighing latency, permission model and portability for each.
- **README.md**: build/run instructions and a supported-OS statement (Ubuntu/Linux, with the exact capture-permission steps — typically adding the user to the `input` group for `/dev/input/event*` access); the Decisions log above, pasted or linked into "Decisions and edge cases."
- **`ai-logs/`**: this conversation, exported in full — it's the actual design-and-decision transcript the assignment asks for; export it alongside any separate agent transcripts from the implementation phase.
- **`CLAUDE.md`** for the implementing agent: project context (a link to this design doc), the Decisions log, the hard requirements (≤1% CPU, ≤32MB, crash safety, live queries), and a pointer to the Testing and verification plan above so the agent writes verification alongside implementation rather than after it.
- **Code**: structured per this design's component boundaries (capture, ingest, reconstruction engine with pluggable tiers, query) so each is independently testable, matching the "Architecture & Code Quality" evaluation criterion.

## Instructions for the implementing agent

Build this in four stages, in order. After each stage, the binary must build and run, and that stage's pieces must work end to end — not partially, not stubbed. Test it yourself before handing it back; then stop and wait for explicit sign-off before starting the next stage. Do not start stage N+1 until told the current stage is approved.

**Stage 1 — Capture.** `brain run` captures keystrokes with full context (app, window, resolved key, modifiers, timestamp) and focus-change events, writing them durably per the Data model and Capture, ingestion and concurrency sections above, backed by a working SQL store. No query support yet. Test: run it, type in a few different apps, inspect the capture log and the DB directly, confirm every keystroke and focus change landed with correct context and no gaps or corruption after both a normal stop and a `kill -9` + restart.

**Stage 2 — Query.** Implement `brain query search` and `brain query text` per the CLI reference above, against Stage 1's stored data (`search` can run against raw/un-reconstructed text for now if full session reconstruction isn't built yet — say clearly in your test report which parts are real reconstruction vs. a placeholder). Test: run `run` and `query` concurrently, confirm both commands return correct, well-formed JSON matching the spec's shapes while capture keeps going.

**Stage 3 — Reconstruction.** Implement the session-definition rules, the tier 0/1/2 reconstruction pipeline and worker pool from Reconstruction pipeline / Safety above, wiring `search`'s session text and `text`'s point-in-time behavior to real reconstructed output. Test: exercise the session-boundary rules directly (idle timeout, commit keys, focus-loss-and-return), the tier 2 VS Code adapter, and the search-semantics edge case (deleted within a session vs. deleted after that session closed).

**Stage 4 — End-to-end testing.** Run the full Testing and verification plan above — performance budget, crash safety, live-query consistency, search semantics, `text --at T`, sleep/wake — against the complete system, not component-by-component in isolation.

**After every stage:**

- The app must build and actually run — no half-finished stage left in a broken state.
- Test it yourself and report what you tested and the result, before reporting the stage done.
- Then stop. Wait for the user to test it and explicitly approve before starting the next stage.

**Git**: commit as you go, with descriptive messages in the style of [cbea.ms/git-commit](https://cbea.ms/git-commit/) — a short imperative subject line (≤50 characters, no trailing period), a blank line, then a body (wrapped around 72 characters) explaining what changed and why, not how. Commit at meaningful checkpoints within a stage, not only once per stage.
