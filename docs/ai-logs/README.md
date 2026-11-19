# AI logs

Everything the AI did on this project, as recorded. The design was written
with Claude on claude.ai. The implementation was done with Claude Code
(Opus 5.5) in one conversation from Nov 15 to Nov 18, 2026, with the owner
approving each stage. The mistakes it made are listed in the main README,
under [Where the agent failed](../README.md#where-the-agent-failed).

| File | What |
|---|---|
| `design-doc.md` | The design doc written with Claude on claude.ai before implementation ([live doc](https://claude.ai/artifact/UTxCHmXHeSBaQASSNNiSP9)). The design conversation itself lives on claude.ai. |
| `session-1.md` | The Claude Code conversation's first transcript (Nov 15): reading the assignment and the design, the crash-safe log, SQLite index and ingest, capture of keys and focus on KDE, the crash test, sessions and text reconstruction. It ends at the first context compaction, after the VS Code tier 2 agent was started. |
| `session-2.md` | The same conversation, continued after that compaction (Nov 15 to 18). It covers the rest: tier 2, substring search, `brain query`, the systemd service, the test suites, PLAN.md and the README, the final reviews, the platform interfaces and GNOME support, `query sessions`/`apps`, brain-chat (with redaction and right-to-left text), the clean-code pass and the Makefile. The context was compacted three more times in it. Each compaction appears as a summary message, and the conversation continues after it. |
| `subagent-aa6364f0e0124dab2.md` | A background agent that drafted PLAN.md's research and design sections. |
| `subagent-a8e3c9e477e3774ff.md` | A background agent, forked into its own git worktree, that built the first VS Code tier 2 adapter (`src/reconstruct/vscode/`). The main conversation merged and reworked it, and its worktree has since been removed. |
| `subagent-a14863ef2dc743064.md` | A web-fetch agent that summarized a Rust clean-code article ([dev.to](https://dev.to/mbayoun95/rust-clean-code-crafting-elegant-efficient-and-maintainable-software-27ce)), for the review that led to the clean-code pass. |
| `raw/*.jsonl` | The complete raw transcripts the Markdown files are rendered from, under the same names. |

The Markdown files truncate long tool inputs and outputs; the raw JSONL
files are complete. Both are redacted the same way. The owner's email
address becomes `<redacted>`. So does anything shaped like an Anthropic API
key (`sk-ant-…`), and so does every 10-character piece of such a key found
anywhere in the transcripts (a session may quote part of a key). Regenerate
with:

```sh
python3 -I docs/ai-logs/export-ai-logs.py --redact <email> --redact <email-local-part>
```

It reads Claude Code's transcripts of this project
(`~/.claude/projects/<project path with / as ->/`). It numbers the session
files from oldest to newest, takes each subagent's newest copy (a subagent
resumed later has a copy under each session), and rewrites everything in
`ai-logs/` except this README, `design-doc.md` and the exporter itself.
