# brain — diagrams

## Components

`brain run` captures and indexes in the background; `brain query` and
brain-chat only read.

```mermaid
flowchart LR
    subgraph SRC["Sources"]
        KB["Keyboard"]
        DE["Desktop<br/>focus · clipboard · lock"]
    end

    subgraph RUN["brain run"]
        CAP["Capture"]
        ING["Ingest"]
        T2["VS Code tier 2<br/>optional"]
    end

    subgraph DATA["Data directory"]
        LOG[("Event log")]
        DB[("Search index")]
    end

    subgraph USE["Readers"]
        Q["brain query"]
        CHAT["brain-chat"]
    end

    API["Claude API"]

    KB --> CAP
    DE --> CAP
    CAP --> LOG
    LOG --> ING
    ING --> DB
    ING <--> T2
    DB --> Q
    LOG --> Q
    Q --> CHAT
    CHAT <--> API
```

## Stages

What happens to what you type, from the keyboard to an answer.

```mermaid
flowchart LR
    subgraph RUN["brain run"]
        direction LR
        S1["1 · Capture<br/>keys + focused window"]
        S2["2 · Log<br/>one line per event"]
        S3["3 · Sessions<br/>by app + window"]
        S4["4 · Reconstruct<br/>keys → text"]
        S5["5 · Index<br/>full-text search"]
    end

    S6["6 · Query<br/>JSON answers"]

    S1 --> S2 --> S3 --> S4 --> S5 --> S6
```

| Stage | What it does |
|---|---|
| Capture | Reads the keyboard without delaying it, and asks the desktop which window has focus. |
| Log | Appends every event to `events.jsonl`, the ground truth. |
| Sessions | Groups typing per app and window; a session ends after 30 s idle or on Enter in a single-line field. |
| Reconstruct | Replays the keys into the text the app shows (Backspace, selections, undo); VS Code can use the real editor. |
| Index | Stores each session's text in SQLite with substring search. |
| Query | `brain query` searches sessions or rebuilds a time range; brain-chat asks Claude on top of it. |
