#!/usr/bin/env python3
"""Export the Claude Code transcripts of this project into ai-logs/.

For every session transcript (and each subagent transcript under it):
  ai-logs/raw/<name>.jsonl   the complete raw JSONL, redacted
  ai-logs/<name>.md          a readable rendering: user and assistant
                             messages, tool calls and (truncated) results

Redaction replaces the given strings (e.g. an email address) everywhere,
case-insensitively, in both outputs.

Usage (from the repository root):
  python3 -I scripts/export-ai-logs.py --redact you@example.com \
      [--src ~/.claude/projects/<project>] [--out ai-logs]
"""

import argparse
import json
import re
from pathlib import Path

MAX_TOOL_INPUT = 4000
MAX_TOOL_RESULT = 3000


def redactor(secrets):
    pats = [re.compile(re.escape(s), re.IGNORECASE) for s in secrets if s]

    def apply(text):
        for p in pats:
            text = p.sub("<redacted>", text)
        return text

    return apply


def clip(text, limit):
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n… [{len(text) - limit} more characters in the raw transcript]"


def fence(text):
    # A fence longer than any backtick run inside the text.
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    ticks = "`" * max(3, longest + 1)
    return f"{ticks}\n{text}\n{ticks}"


def block_text(content):
    if isinstance(content, str):
        return content
    parts = []
    for b in content or []:
        if isinstance(b, dict) and b.get("type") == "text":
            parts.append(b.get("text", ""))
        elif isinstance(b, dict) and b.get("type") == "image":
            parts.append("[image]")
    return "\n".join(parts)


def render(path, title):
    out = [f"# {title}", "", f"Rendered from `raw/{path.stem}.jsonl`. Tool inputs and results are "
           f"truncated here; the raw transcript is complete.", ""]
    for line in path.open(encoding="utf-8"):
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = rec.get("type")
        msg = rec.get("message")
        if kind not in ("user", "assistant") or not isinstance(msg, dict):
            continue
        content = msg.get("content")
        ts = rec.get("timestamp", "")
        if kind == "user":
            if isinstance(content, str) or any(
                isinstance(b, dict) and b.get("type") == "text" for b in content or []
            ):
                text = block_text(content).strip()
                if text:
                    label = "Summary of earlier conversation" if rec.get("isCompactSummary") else "User"
                    out += [f"## {label}  <sub>{ts}</sub>", "", text, ""]
            for b in content if isinstance(content, list) else []:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    res = b.get("content")
                    res = block_text(res) if not isinstance(res, str) else res
                    out += ["<details><summary>Tool result</summary>", "",
                            fence(clip(res or "", MAX_TOOL_RESULT)), "", "</details>", ""]
        else:
            for b in content if isinstance(content, list) else []:
                if not isinstance(b, dict):
                    continue
                t = b.get("type")
                if t == "text" and b.get("text", "").strip():
                    out += [f"## Assistant  <sub>{ts}</sub>", "", b["text"].strip(), ""]
                elif t == "thinking" and b.get("thinking", "").strip():
                    out += ["<details><summary>Assistant thinking</summary>", "",
                            b["thinking"].strip(), "", "</details>", ""]
                elif t == "tool_use":
                    inp = json.dumps(b.get("input", {}), ensure_ascii=False, indent=2)
                    out += [f"**Tool call: `{b.get('name')}`**", "", fence(clip(inp, MAX_TOOL_INPUT)), ""]
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser()
    # Claude Code keeps a project's transcripts under a folder named after
    # the project's path, with "/" replaced by "-".
    default_src = Path.home() / ".claude/projects" / str(Path.cwd()).replace("/", "-")
    ap.add_argument("--src", default=str(default_src))
    ap.add_argument("--out", default="ai-logs")
    ap.add_argument("--redact", action="append", default=[])
    args = ap.parse_args()
    redact = redactor(args.redact)
    src, out = Path(args.src), Path(args.out)
    (out / "raw").mkdir(parents=True, exist_ok=True)

    sessions = sorted(src.glob("*.jsonl"), key=lambda p: p.stat().st_mtime)
    jobs = []
    # A subagent resumed in a later session part has a copy under each part;
    # the newest copy is the complete one.
    agents = {}
    for i, s in enumerate(sessions, 1):
        jobs.append((s, f"session-{i}", f"Claude Code session, part {i}"))
        for a in (src / s.stem / "subagents").glob("*.jsonl"):
            if a.stem not in agents or a.stat().st_mtime > agents[a.stem].stat().st_mtime:
                agents[a.stem] = a
    for stem, a in sorted(agents.items(), key=lambda kv: kv[1].stat().st_mtime):
        jobs.append((a, f"subagent-{stem.removeprefix('agent-')}", f"Subagent transcript {stem}"))
    for path, name, title in jobs:
        raw = redact(path.read_text(encoding="utf-8"))
        raw_out = out / "raw" / f"{name}.jsonl"
        raw_out.write_text(raw, encoding="utf-8")
        md = redact(render(raw_out, title))
        (out / f"{name}.md").write_text(md, encoding="utf-8")
        print(f"{path.name} -> {raw_out} ({len(raw) // 1024} KiB), {name}.md ({len(md) // 1024} KiB)")


if __name__ == "__main__":
    main()
