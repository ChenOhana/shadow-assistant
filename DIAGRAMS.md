# brain — diagrams

The system's structure and its main flows, drawn from the code as built.
The diagrams are [Mermaid](https://mermaid.js.org/): GitHub renders them in
place.

1. [System design](#1-system-design)
2. [Data model](#2-data-model)
3. [Happy flows](#3-happy-flows)
   1. [Start and stop of `brain run`](#31-start-and-stop-of-brain-run)
   2. [A keystroke, from the keyboard to the log](#32-a-keystroke-from-the-keyboard-to-the-log)
   3. [A focus change](#33-a-focus-change)
   4. [A paste](#34-a-paste)
   5. [Ingest: from the log to searchable sessions](#35-ingest-from-the-log-to-searchable-sessions)
   6. [The life of a session](#36-the-life-of-a-session)
   7. [VS Code tier 2](#37-vs-code-tier-2)
   8. [`brain query search` and `brain query text`](#38-brain-query-search-and-brain-query-text)
   9. [A brain-chat question](#39-a-brain-chat-question)

## 1. System design

One process, `brain run`, captures and indexes; readers (`brain query`,
brain-chat) never write. The log is the ground truth; the database is
derived from it. The data directory is private (0700), and `run.lock`
lets only one `brain run` use it at a time.

```mermaid
flowchart TB
    KBD["Keyboards<br/>/dev/input/event*"]
    DESK["Desktop over D-Bus<br/>KDE: KWin script · Klipper<br/>GNOME: brain's Shell extension"]
    LOGIND["logind<br/>sleep / wake"]

    subgraph RUN["brain run · one process"]
        INPUT["Input thread<br/>poll · xkbcommon · repeat"]
        DBUS["D-Bus threads<br/>focus · lock · layouts · sleep"]
        REC["Recorder<br/>stamps app + window<br/>pauses while locked<br/>reads the clipboard at a paste"]
        INGEST["Ingest thread · every ~1 s<br/>sessions · tier 0/1 text"]
        T2["Tier 2 worker<br/>opt-in: --vscode-replay"]
    end

    subgraph DATA["Data directory"]
        LOG[("events.jsonl<br/>append-only log<br/>ground truth")]
        DB[("brain.db · SQLite WAL<br/>sessions + FTS5 trigram<br/>checkpoints · offset")]
    end

    VSC["Headless VS Code<br/>no network · capped"]

    subgraph READ["Readers · never write"]
        Q["brain query<br/>search · sessions · apps · text"]
        CHAT["brain-chat · Python<br/>Claude + read-only tools"]
    end

    API["Claude API"]

    KBD -- "key events" --> INPUT
    DESK -- "focus · lock · layouts" --> DBUS
    LOGIND -- "PrepareForSleep" --> DBUS
    DBUS -- "layout switch · reopen" --> INPUT
    INPUT -- "resolved keys" --> REC
    DBUS -- "focus · lock · sleep" --> REC
    REC -- "one write(2) per event" --> LOG
    LOG -- "tail from the offset" --> INGEST
    INGEST -- "one transaction per step" --> DB
    INGEST <--> T2
    T2 <-- "DevTools over a pipe" --> VSC
    CHAT -- "subprocess" --> Q
    Q -- "read-only snapshot" --> DB
    Q -. "text: from a checkpoint" .-> LOG
    CHAT -- "redacted results" --> API
```

| Part | Code |
|---|---|
| Input thread | `src/capture/linux/input.rs`, `keys.rs`, `xkb.rs` |
| Desktop backends | `src/capture/linux/desktop.rs`, `kde.rs` + `kwin_focus.js`, `gnome.rs` + `packaging/gnome-extension/` |
| Recorder | `src/capture/recorder.rs` |
| Log | `src/eventlog.rs` |
| Ingest and sessions | `src/ingest.rs`, `src/sessions.rs`, `src/reconstruct/` |
| Tier 2 | `src/reconstruct/tier2.rs`, `src/reconstruct/vscode/` |
| Database | `src/store.rs` |
| Queries | `src/query.rs` |
| brain-chat | `chat/` |

## 2. Data model

`events.jsonl` holds one JSON object per event. Everything in `brain.db`
is derived from it and can be rebuilt by deleting the database (tier 2
text aside, which depends on files on disk).

```mermaid
erDiagram
    EVENT {
        text ts "RFC 3339 UTC"
        text app "desktop file id"
        text type "key | focus | paste | system"
        text window "window title"
        text key "key only"
        list mods "key only"
        bool repeat "key only"
        text text "paste only"
        bool renamed "focus only"
        text state "system only"
    }
    SESSIONS {
        int id PK
        text app
        text window "normalized title"
        text started_at
        text ended_at "NULL while open"
        text status "open | closed"
        text reconstructed_text
        int start_offset "log bytes"
        int end_offset "log bytes"
        text end_reason "idle | commit | cancel | field | stop | restart"
        text tier2_status
        text tier2_file
    }
    SESSIONS_FTS {
        text reconstructed_text "trigram index"
    }
    CONTEXT_STATE {
        text app PK
        text window PK
        text text "document between sessions"
        int cursor
        bool cursor_unknown
    }
    INGEST_STATE {
        int log_offset "bytes ingested"
    }
    LOG_CHECKPOINTS {
        text ts PK
        int byte_offset
    }

    SESSIONS |o--o{ EVENT : "covers (log offsets)"
    SESSIONS ||--|| SESSIONS_FTS : "indexed by triggers"
    CONTEXT_STATE |o--o{ SESSIONS : "carries a window's document across"
    LOG_CHECKPOINTS |o--|| EVENT : "points at"
    INGEST_STATE |o--|| EVENT : "points past the last ingested"
```

## 3. Happy flows

### 3.1 Start and stop of `brain run`

Everything that can fail is set up before capture starts. On a signal,
capture stops first, then a final ingest pass takes in every last event.

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant M as main thread
    participant D as Data directory
    participant I as Ingester
    participant P as Platform (Linux)
    participant K as Desktop (KWin / GNOME)

    U->>M: brain run --data-dir DIR
    M->>M: block SIGINT/SIGTERM/SIGHUP, umask 077
    M->>D: create (0700), flock run.lock
    M->>D: open events.jsonl (cut a torn last line)
    M->>D: open brain.db (WAL, check schema)
    M->>I: load open sessions, replay their events from the log
    M->>P: start
    P->>D: log "start"
    P->>P: read layouts + repeat, open keyboards
    P->>K: load KWin script / listen to the extension
    P->>P: spawn input thread, watch logind
    M->>I: spawn ingest thread
    Note over M: waits for a signal

    U->>M: Ctrl+C / SIGTERM
    M->>P: stop input, unload KWin script
    M->>D: log "stop"
    M->>I: stop: final sync + ingest
    I->>D: "stop" closes every open session
    M->>D: release run.lock (process exits)
```

### 3.2 A keystroke, from the keyboard to the log

The input thread never touches SQLite: per key it does one read, one
xkbcommon lookup and one `write(2)`. A stalled brain cannot delay the key,
because the compositor reads the same device independently.

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant Kern as Kernel (evdev)
    participant C as Compositor
    participant In as Input thread
    participant X as xkbcommon
    participant R as Recorder
    participant L as events.jsonl

    U->>Kern: presses H with Shift
    par delivered to both
        Kern->>C: key event (the app gets "H")
    and
        Kern->>In: key event + kernel timestamp
    end
    In->>X: keycode + modifier state
    X-->>In: "H", mods [shift]
    In->>R: key(ts, "H", [shift])
    alt screen locked
        R-->>R: drop it: nothing is logged
    else unlocked
        R->>R: stamp current app + window
        R->>L: write(2) one complete JSON line
    end
    Note over In: a held key: repeats are generated<br/>on the desktop's schedule, not the kernel's
```

### 3.3 A focus change

The compositor is the only process that knows which window has focus, so
brain's code runs inside it and reports out. Reports from anyone else are
ignored.

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant C as Compositor
    participant S as KWin script / GNOME extension
    participant B as brain (D-Bus thread)
    participant R as Recorder
    participant L as events.jsonl

    U->>C: switches to another window
    C->>S: windowActivated / notify::focus-window
    alt KDE
        S->>B: callDBus Activated(desktop file, class, title, renamed=false)
    else GNOME
        S-->>B: signal Focus(desktop file, class, title, renamed=false)
    end
    B->>B: sender is the compositor's bus name?
    B->>R: set_focus(app id, title)
    R->>L: focus event
    Note over S,R: a title change of the focused window<br/>is the same path with renamed=true
```

### 3.4 A paste

Only pastes are recorded, never copies. The clipboard is read outside the
log lock, with a 300 ms limit, because this runs on the input thread.

```mermaid
sequenceDiagram
    autonumber
    participant In as Input thread
    participant R as Recorder
    participant CB as Clipboard (Klipper / extension)
    participant L as events.jsonl

    In->>R: key(ts, "v", [ctrl])
    R->>L: key event
    R->>CB: read clipboard (300 ms timeout)
    CB-->>R: text
    R->>L: paste event (up to 64 KiB of text)
```

### 3.5 Ingest: from the log to searchable sessions

Each step is one SQLite transaction holding everything derived from a batch
of lines together with the new log offset, so a crash at any point neither
loses nor repeats an event.

```mermaid
sequenceDiagram
    autonumber
    participant T as Ingest thread
    participant L as events.jsonl
    participant E as Session engine
    participant A as Tier 0/1 adapters
    participant DB as brain.db

    loop about once a second
        T->>L: fdatasync (if anything was written)
        T->>DB: BEGIN
        T->>L: read complete lines from the stored offset
        T->>DB: log checkpoint (at most every 5 s of log time)
        loop each event
            T->>E: event
            E->>E: route by (app, normalized window)
            alt no open session there
                E->>DB: insert session (open)
            end
            E->>A: key → action for this app class
            A-->>E: edit the session's text buffer
            alt Enter in a single-line field
                E->>DB: close session (commit)
            end
        end
        E->>DB: close sessions idle for 30 s
        E->>DB: write the live text of changed open sessions
        T->>DB: store the new offset, COMMIT
    end
    Note over DB: FTS5 triggers update the search index<br/>in the same transaction
```

### 3.6 The life of a session

```mermaid
stateDiagram-v2
    direction LR
    state ended <<choice>>
    [*] --> Open: first key with an effect<br/>in an (app, window)
    Open --> ended: idle 30 s · Enter (commit)<br/>Ctrl+C (cancel) · Tab (field)<br/>brain run stops · restarts
    ended --> Closed: text changed
    ended --> Removed: nothing changed
    Closed --> [*]
    Removed --> [*]

    note right of Open
        More keys extend it.
        It stays open when focus
        leaves and comes back
        within 30 s, or when the
        typing renames the title.
        Its text is searchable
        while it is open.
    end note
```

### 3.7 VS Code tier 2

Opt-in with `--vscode-replay`. A file saved after the session gives an
exact diff and VS Code is never started; otherwise the keys are replayed
into a sandboxed VS Code as a best effort. The tier 0/1 text stays if
neither works.

```mermaid
sequenceDiagram
    autonumber
    participant E as Ingest / session engine
    participant S as Snapshots
    participant W as Tier 2 worker
    participant F as File on disk
    participant V as Headless VS Code

    E->>F: find the file from the window title
    E->>S: session opens: copy the file (snapshot)
    Note over E: tier2_status = snapshot
    E->>E: session closes with its tier 0/1 text
    Note over E: tier2_status = pending
    E->>W: job (after the ingest commit)
    W->>F: saved after the last keystroke?
    alt saved
        W->>W: git-style diff: snapshot → saved file
        Note over W: done: saved file (exact)
    else not saved
        W->>V: open the snapshot (quick open)
        W->>V: the session's keys, over DevTools
        W->>V: save
        V-->>W: file content
        W->>W: diff: snapshot → replayed file
        Note over W: done: replayed (best effort, unverified)
    end
    W-->>E: result
    E->>E: next ingest step stores the text (index updated)
```

### 3.8 `brain query search` and `brain query text`

Both run while `brain run` writes. `search` reads a consistent snapshot of
the database; `text` reads the log itself and reconstructs it per window.

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant Q as brain query
    participant DB as brain.db
    participant L as events.jsonl
    participant A as Tier 0/1 adapters

    U->>Q: search "migration blocked"
    Q->>DB: open read-only, one read snapshot
    Q->>DB: FTS5 MATCH (every word, trigram)
    DB-->>Q: sessions, newest first, with snippets
    Q-->>U: JSON (times in local time)

    U->>Q: text --app kate --from 10:00
    Q->>DB: last checkpoint before 10:00
    DB-->>Q: byte offset
    Q->>L: read from there, keys + pastes of the app in the range
    Q->>Q: split per window, each from empty text
    Q->>A: each window's keys
    A-->>Q: reconstructed text per window
    Q-->>U: JSON: windows + joined text
```

### 3.9 A brain-chat question

The loop runs locally. Claude decides which lookups to make and when it has
enough; every tool result is capped and redacted before it leaves the
machine.

```mermaid
sequenceDiagram
    autonumber
    actor U as User
    participant C as brain-chat
    participant API as Claude API
    participant T as Tools
    participant Q as brain query
    participant R as Redaction

    U->>C: "which commands did I run yesterday on prod-db?"
    C->>API: time + question, tools (streamed)
    loop up to 10 rounds of tool calls
        API-->>C: tool_use: sessions(app=konsole, window=prod-db, from, to)
        C->>U: prints the tool call
        C->>T: run
        T->>Q: brain query sessions ... (argument list, no shell)
        Q-->>T: JSON
        T->>R: cap + mask secrets
        R-->>C: tool_result
        C->>API: tool results
    end
    API-->>C: answer citing times, apps and windows
    C-->>U: streamed answer (right-to-left aware)
```
