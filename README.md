# brain — a shadow assistant for Linux

`brain` runs on your computer, follows what you type in each application and
lets you search it later.

```
brain run                      # capture keystrokes + focus changes, store them
brain query search "<words>"   # (stage 2) full-text search over writing sessions
brain query text --app A       # (stage 2) the raw typed stream of an app
```

> **Status: stage 1 of 4 (capture).** `brain run` captures and stores;
> `brain query` is not implemented yet.

## Supported OS

Ubuntu (tested on 26.04) with **KDE Plasma 6 on Wayland** (KWin). Key capture
works on any Linux. Focus tracking needs KWin (Wayland or X11). On other
desktops, keys are recorded under the app `unknown`.

## Build

```sh
# Rust toolchain (https://rustup.rs), then:
cargo build --release
# binary: target/release/brain
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

Data directory (mode 0700):

| File           | What                                                   |
|----------------|--------------------------------------------------------|
| `events.jsonl` | append-only capture log, the ground truth              |
| `brain.db`     | SQLite (WAL) index derived from the log                |
| `run.lock`     | held by the running `brain run` (one per data dir)     |
| `kwin/`        | generated KWin focus-tracking script                   |

Example log lines:

```json
{"ts":"2026-11-15T17:20:01.112Z","app":"org.kde.konsole","type":"focus","window":"~ : bash — Konsole"}
{"ts":"2026-11-15T17:20:02.348Z","app":"org.kde.konsole","type":"key","key":"H","mods":["shift"],"window":"~ : bash — Konsole"}
```

## Tests

```sh
cargo test                                   # unit + integration tests
cargo test --release --test crash_safety     # 25 random kill -9s under load
```
