# Shadow Assistant — `brain`

A shadow assistant for Linux: `brain run` follows what you type in each
application and `brain query` lets you search it later.

The full README (build, run, supported OS, permissions, decisions and edge
cases, tests, where the agent failed) is
**[docs/README.md](docs/README.md)**.

## Repository structure

| Folder | What |
|---|---|
| [`brain/`](brain/) | The `brain` program (Rust): `src/`, `tests/`, `Cargo.toml`, the `Makefile`, `packaging/` (GNOME Shell extension, systemd unit) and `scripts/`. It builds into `brain/target/`. |
| [`brain-chat/`](brain-chat/README.md) | brain-chat (Python): ask Claude about what you typed, in plain language, on top of `brain query`. |
| [`docs/`](docs/) | Everything to read: the full [README](docs/README.md), [PLAN.md](docs/PLAN.md) (research and architecture), [diagrams](docs/diagrams/DIAGRAMS.md), the AI transcripts in [ai-logs/](docs/ai-logs/README.md), the agent instructions in [CLAUDE.md](docs/CLAUDE.md), and the assignment brief. |

## Quick start

```sh
cd brain
cargo build --release                       # or: make build
target/release/brain run --data-dir ~/brain-data
target/release/brain query search "words" --data-dir ~/brain-data
```

Your user must be in the `input` group to capture keys, and on GNOME brain
needs its Shell extension (`make install` in `brain/`). See
[docs/README.md](docs/README.md) for the details.
