# brain — diagrams

## Components

Four layers, top to bottom: what brain listens to, `brain run` in the
background, the data it keeps, and the readers that only read it. Pink
arrows are two-way.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"fontSize": "15px", "lineColor": "#64748b", "edgeLabelBackground": "#ffffff"}, "flowchart": {"curve": "basis", "padding": 20, "nodeSpacing": 40, "rankSpacing": 55}}}%%
flowchart TB
    subgraph L1["🖥️ Your desktop"]
        direction LR
        KB(["⌨️ Keyboard"])
        FOC(["🪟 Focused window"])
        CLIP(["📋 Clipboard"])
        SCR(["🔒 Screen lock"])
        KB ~~~ FOC ~~~ CLIP ~~~ SCR
    end

    subgraph L2["⚙️ brain run"]
        direction LR
        CAP(["🎙️ Capture"])
        ING(["🔄 Ingest · every ~1 s"])
        T2(["🧩 VS Code tier 2 · optional"])
        CAP ~~~ ING
        ING <--> T2
    end

    subgraph L3["💾 Data directory"]
        direction LR
        LOG[("📜 Event log")]
        DB[("🗂️ Search index")]
        LOG ~~~ DB
    end

    subgraph L4["🔎 Readers · never write"]
        direction LR
        Q(["🔍 brain query"])
        CHAT(["💬 brain-chat"])
        API{{"🤖 Claude API"}}
        Q <--> CHAT <--> API
    end

    L1 ==>|"keys · focus · clipboard · lock"| L2
    L2 <==>|"append · tail · write"| L3
    L3 ==>|"read-only"| L4

    classDef src fill:#e0f2fe,stroke:#0284c7,stroke-width:1.5px,color:#0c4a6e
    classDef proc fill:#ede9fe,stroke:#7c3aed,stroke-width:1.5px,color:#3b0764
    classDef store fill:#fef3c7,stroke:#d97706,stroke-width:1.5px,color:#78350f
    classDef read fill:#dcfce7,stroke:#16a34a,stroke-width:1.5px,color:#14532d
    classDef ext fill:#f1f5f9,stroke:#475569,stroke-width:1.5px,color:#0f172a
    class KB,FOC,CLIP,SCR src
    class CAP,ING,T2 proc
    class LOG,DB store
    class Q,CHAT read
    class API ext

    style L1 fill:#f0f9ff,stroke:#7dd3fc,stroke-width:1.5px,color:#0369a1
    style L2 fill:#f5f3ff,stroke:#c4b5fd,stroke-width:1.5px,color:#6d28d9
    style L3 fill:#fffbeb,stroke:#fcd34d,stroke-width:1.5px,color:#b45309
    style L4 fill:#f0fdf4,stroke:#86efac,stroke-width:1.5px,color:#15803d

    linkStyle 4,6,7,9 stroke:#db2777,stroke-width:3px
    linkStyle 8,10 stroke:#64748b,stroke-width:3px
```

## Stages

What happens to what you type, from the keyboard to an answer. Dashed
pink arrows are loops.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"fontSize": "15px", "lineColor": "#64748b", "edgeLabelBackground": "#ffffff"}, "flowchart": {"curve": "basis", "padding": 20, "nodeSpacing": 40, "rankSpacing": 55}}}%%
flowchart TB
    subgraph RUN["⚙️ brain run"]
        direction LR
        S1(["⌨️ 1 · Capture<br/>keys + focused window"])
        S2[("📜 2 · Log<br/>one line per event")]
        S3(["🧵 3 · Sessions<br/>by app + window"])
        S4(["✍️ 4 · Reconstruct<br/>keys → text"])
        S5[("🗂️ 5 · Index<br/>full-text search")]
        S1 --> S2 --> S3 --> S4 --> S5
        S5 -.->|"↻ every ~1 s"| S3
    end

    subgraph USE["🔎 Readers"]
        direction LR
        S6(["🔍 6 · Query<br/>JSON answers"])
        S7(["💬 7 · Ask<br/>brain-chat + Claude"])
        S6 --> S7
        S7 -.->|"↻ up to 10 rounds"| S6
    end

    RUN ==>|"search · rebuild a time range"| USE

    classDef proc fill:#ede9fe,stroke:#7c3aed,stroke-width:1.5px,color:#3b0764
    classDef store fill:#fef3c7,stroke:#d97706,stroke-width:1.5px,color:#78350f
    classDef read fill:#dcfce7,stroke:#16a34a,stroke-width:1.5px,color:#14532d
    class S1,S3,S4 proc
    class S2,S5 store
    class S6,S7 read

    style RUN fill:#f5f3ff,stroke:#c4b5fd,stroke-width:1.5px,color:#6d28d9
    style USE fill:#f0fdf4,stroke:#86efac,stroke-width:1.5px,color:#15803d

    linkStyle 4,6 stroke:#db2777,stroke-width:2.5px
    linkStyle 7 stroke:#64748b,stroke-width:3px
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
