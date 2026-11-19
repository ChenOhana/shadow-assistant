# brain — diagrams

## Architecture

Layers run top to bottom, in the order data flows through them. Indigo
arrows are two-way; the dotted arrow is `brain query text` reading the
log directly.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"fontSize": "14px", "lineColor": "#94a3b8", "edgeLabelBackground": "#ffffff", "textColor": "#334155"}, "flowchart": {"curve": "basis", "padding": 22, "nodeSpacing": 36, "rankSpacing": 54}}}%%
flowchart TB
    subgraph L1["🖥️  DESKTOP SESSION"]
        KB(["Keyboard"])
        FOC(["Focused window"])
        CLIP(["Clipboard"])
        SCR(["Screen lock"])
    end

    subgraph L2["⚙️  BRAIN RUN · CAPTURE"]
        INP("<b>Input thread</b><br/>evdev · xkbcommon")
        DSK("<b>Desktop listener</b><br/>KWin script · GNOME extension")
    end

    subgraph L3["💾  DATA · GROUND TRUTH"]
        LOG[("<b>events.jsonl</b><br/>append-only event log")]
    end

    subgraph L4["⚙️  BRAIN RUN · INGEST"]
        ING("<b>Ingest</b> · every ~1 s<br/>sessions · text reconstruction")
    end

    subgraph L5["💾  DATA · DERIVED"]
        DB[("<b>brain.db</b><br/>sessions · full-text index")]
    end

    subgraph L6["🔎  READERS"]
        Q("<b>brain query</b><br/>search · sessions · apps · text")
    end

    subgraph LT["🧩  TIER 2 · OPTIONAL"]
        T2("<b>Tier 2 worker</b><br/>saved-file diff or replay")
        VSC("<b>Headless VS Code</b><br/>sandboxed · offline")
    end

    subgraph L7["🤖  ASSISTANT"]
        CHAT("<b>brain-chat</b><br/>questions in plain language")
    end

    API{{"<b>Claude API</b>"}}

    KB -->|"key events"| INP
    FOC -->|"focus changes"| DSK
    CLIP <-->|"read on paste"| DSK
    SCR -->|"pause · resume"| DSK
    INP -->|"append keys"| LOG
    DSK -->|"append focus · paste · lock"| LOG
    LOG -->|"tails new lines"| ING
    ING -->|"creates & updates sessions"| DB
    T2 <-->|"jobs · results"| ING
    T2 <-->|"replays keys"| VSC
    DB -->|"search · list"| Q
    LOG -.->|"query text"| Q
    Q <-->|"JSON"| CHAT
    CHAT <-->|"tool calls"| API

    classDef src fill:#ffffff,stroke:#0ea5e9,stroke-width:1.5px,color:#0f172a
    classDef proc fill:#ffffff,stroke:#6366f1,stroke-width:1.5px,color:#0f172a
    classDef store fill:#fffbeb,stroke:#f59e0b,stroke-width:1.5px,color:#0f172a
    classDef read fill:#ffffff,stroke:#10b981,stroke-width:1.5px,color:#0f172a
    classDef ext fill:#0f172a,stroke:#0f172a,stroke-width:1.5px,color:#ffffff
    class KB,FOC,CLIP,SCR src
    class INP,DSK,ING,T2,VSC proc
    class LOG,DB store
    class Q,CHAT read
    class API ext

    style L1 fill:#f0f9ff,stroke:#bae6fd,color:#0369a1
    style L2 fill:#eef2ff,stroke:#c7d2fe,color:#4338ca
    style L3 fill:#fffbeb,stroke:#fde68a,color:#b45309
    style L4 fill:#eef2ff,stroke:#c7d2fe,color:#4338ca
    style L5 fill:#fffbeb,stroke:#fde68a,color:#b45309
    style L6 fill:#ecfdf5,stroke:#a7f3d0,color:#047857
    style LT fill:#f5f3ff,stroke:#ddd6fe,color:#6d28d9
    style L7 fill:#ecfdf5,stroke:#a7f3d0,color:#047857

    linkStyle 2,8,9,12,13 stroke:#6366f1,stroke-width:2.2px
```

| Layer | Role |
|---|---|
| Desktop session | What brain listens to: the keyboard, the focused window, the clipboard at a paste, the screen lock. |
| brain run · capture | Reads keys passively (never delaying them) and gets focus from inside the compositor. |
| Data · ground truth | `events.jsonl`: every event, appended once, never rewritten. |
| brain run · ingest | Every second, turns new log lines into sessions and reconstructs their text. |
| Data · derived | `brain.db`: sessions and their full-text index, rebuildable from the log. |
| Tier 2 · optional | VS Code sessions from the real editor: a diff of the saved file, or a sandboxed replay. |
| Readers | `brain query` answers while `brain run` keeps writing. |
| Assistant | brain-chat lets Claude call `brain query` to answer plain-language questions. |

## Stages

What happens to what you type, from the keyboard to an answer. Dotted
arrows are loops.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"fontSize": "14px", "lineColor": "#94a3b8", "edgeLabelBackground": "#ffffff", "textColor": "#334155"}, "flowchart": {"curve": "basis", "padding": 22, "nodeSpacing": 36, "rankSpacing": 54}}}%%
flowchart TB
    subgraph RUN["⚙️  BRAIN RUN"]
        direction LR
        S1(["<b>1 · Capture</b><br/>key + focused window"])
        S2[("<b>2 · Log</b><br/>one JSON line per event")]
        S3(["<b>3 · Sessions</b><br/>per app + window"])
        S4(["<b>4 · Reconstruct</b><br/>keys → text"])
        S5[("<b>5 · Index</b><br/>full-text search")]
        S1 -->|"event"| S2
        S2 -->|"new lines"| S3
        S3 -->|"session keys"| S4
        S4 -->|"session text"| S5
        S5 -.->|"↻ every ~1 s"| S3
    end

    subgraph USE["🔎  READERS · ON DEMAND"]
        direction LR
        S6(["<b>6 · Query</b><br/>JSON answers"])
        S7(["<b>7 · Ask</b><br/>brain-chat + Claude"])
        S6 -->|"results"| S7
        S7 -.->|"↻ up to 10 rounds"| S6
    end

    RUN ==>|"search · rebuild a time range"| USE

    classDef proc fill:#ffffff,stroke:#6366f1,stroke-width:1.5px,color:#0f172a
    classDef store fill:#fffbeb,stroke:#f59e0b,stroke-width:1.5px,color:#0f172a
    classDef read fill:#ffffff,stroke:#10b981,stroke-width:1.5px,color:#0f172a
    class S1,S3,S4 proc
    class S2,S5 store
    class S6,S7 read

    style RUN fill:#eef2ff,stroke:#c7d2fe,color:#4338ca
    style USE fill:#ecfdf5,stroke:#a7f3d0,color:#047857

    linkStyle 4,6 stroke:#6366f1,stroke-width:2.2px
    linkStyle 7 stroke:#94a3b8,stroke-width:2.5px
```

| Stage | What it does |
|---|---|
| Capture | Reads the keyboard without delaying it, and asks the desktop which window has focus. |
| Log | Appends every event to `events.jsonl`, the ground truth. |
| Sessions | Groups typing per app and window; a session ends after 30 s idle or on Enter in a single-line field. |
| Reconstruct | Replays the keys into the text the app shows (Backspace, selections, undo); VS Code can use the real editor. |
| Index | Stores each session's text in SQLite with substring search. |
| Query | `brain query` searches sessions or rebuilds a time range, while `brain run` keeps writing. |
| Ask | brain-chat lets Claude call `brain query` until it can answer a plain-language question. |
