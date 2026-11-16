# AI logs

Everything the AI did on this project, as recorded.

| File | What |
|---|---|
| `design-doc.md` | The design doc written with Claude on claude.ai before implementation ([live doc](https://claude.ai/artifact/UTxCHmXHeSBaQASSNNiSP9)). The design conversation itself lives on claude.ai. |
| `session-1.md`, `session-2.md` | The Claude Code implementation session (one session, split in two when its context was compacted). |
| `subagent-aa6364f0e0124dab2.md` | Background agent that drafted PLAN.md. |
| `subagent-a8e3c9e477e3774ff.md` | Parallel agent that built the VS Code tier 2 adapter (`src/reconstruct/vscode/`). |
| `raw/*.jsonl` | The complete raw transcripts the Markdown files are rendered from. |

The Markdown files truncate long tool inputs and outputs; the raw JSONL
files are complete. In both, the owner's email address is replaced with
`<redacted>`. Regenerate with:

```sh
python3 -I scripts/export-ai-logs.py --redact <email> --redact <email-local-part>
```
