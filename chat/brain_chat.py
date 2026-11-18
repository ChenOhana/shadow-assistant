#!/usr/bin/env python3
"""brain-chat: ask questions about what you typed, answered from brain's data.

    brain-chat [--data-dir DIR] [question ...]

With a question, answers it and exits; without one, opens a chat. Claude
answers by calling read-only tools that run `brain query` on this machine
(see tools.py); only those results, with likely secrets masked, go to the
API. The loop: send the conversation and the tools; if Claude asks for
tools, run them, add their results and ask again; stop when it answers.
At most MAX_TOOL_ROUNDS rounds of tool calls per question; then one last
request with tools turned off makes it answer from what it found.
"""

import argparse
import os
import shutil
import sys
from datetime import datetime

import rtl
from tools import TOOLS, Tools, describe

MODEL = "claude-opus-5-5"
MAX_TOOL_ROUNDS = 10
MAX_TOKENS = 32_000
FALLBACK_BETA = "server-side-fallback-2026-07-01"

SYSTEM_PROMPT = """\
You answer the user's questions about what they typed on their own computer, \
using the data of brain, a tool that runs in the background and records it.

What brain records: every key typed, with the focused app and window title, \
grouped into writing sessions per (app, window title) with the text \
reconstructed (edits applied). In a terminal each submitted command is its \
own session (end_reason "commit"), and when connected to another host the \
window title usually names it ("user@host: dir"), so the commands run on a \
host are the terminal sessions whose window contains the host name. Times \
in tool results are the user's local time.

How to work:
- Each user message starts with the user's current local time; resolve \
"yesterday", "this morning" and the like from it, and pass explicit local \
dates and times to the tools (for example from "2026-11-17" to "2026-11-18").
- Start narrow and cheap: list_apps to find app ids, then sessions (by app, \
window and time), search (by words) or text (a time range of one app). \
Narrow a large result rather than reading everything.
- Stop as soon as the results answer the question. If a reasonable search \
finds nothing, say so and say where you looked; never guess.

How to answer:
- Answer in the language the user writes in.
- Be concise. Cite what you report with its time, app and window. List \
commands in the order they were run, in a code block.
- brain sees keys, not program output: it cannot tell whether a command \
succeeded. A line can differ from what ran when it came from shell history \
or tab completion, or was edited with the mouse. Mention this when it matters.
- "[redacted]" marks a likely secret that was masked before you saw it \
(for example a password typed at a sudo prompt). Say that something was \
typed there; never try to guess or reconstruct it.
"""

FINAL_ROUND_NOTE = (
    "You have used all the tool calls available for this question. Answer now "
    "from what you found, and say what you could not check."
)


def now_line() -> str:
    """The user's local time, as the first line of each message."""
    now = datetime.now().astimezone()
    return f"[Local time: {now:%A %Y-%m-%d %H:%M} (UTC{now:%z})]"


class Chat:
    """A conversation; `ask` answers one question, keeping the history."""

    def __init__(self, model_turn, tools, on_tool=lambda line: None):
        self.model_turn = model_turn  # (messages, tool_choice) -> final message
        self.tools = tools
        self.on_tool = on_tool
        self.messages: list = []

    def ask(self, question: str) -> str:
        start = len(self.messages)
        self.messages.append({"role": "user", "content": f"{now_line()}\n{question}"})
        try:
            return self._answer()
        except BaseException:
            # Interrupted or failed: drop the unfinished turn, keep the chat.
            del self.messages[start:]
            raise

    def _answer(self) -> str:
        rounds = 0
        while True:
            final = rounds >= MAX_TOOL_ROUNDS
            if final:
                self.messages.append({"role": "system", "content": FINAL_ROUND_NOTE})
            msg = self.model_turn(self.messages, {"type": "none"} if final else {"type": "auto"})
            self.messages.append({"role": "assistant", "content": msg.content})
            if msg.stop_reason == "refusal":
                return text_of(msg) or "(The model declined to answer this.)"
            tool_uses = [b for b in msg.content if b.type == "tool_use"]
            if final and tool_uses:
                # Tools were off; never run them now, and always end here.
                return text_of(msg) or "(No answer: the tool-call limit was reached.)"
            if msg.stop_reason != "tool_use" or not tool_uses:
                if msg.stop_reason == "max_tokens":
                    return text_of(msg) + "\n(The answer was cut off at the length limit.)"
                return text_of(msg)
            results = []
            for block in tool_uses:
                self.on_tool(describe(block.name, block.input if isinstance(block.input, dict) else {}))
                content, is_error = self.tools.run(block.name, block.input)
                results.append({
                    "type": "tool_result",
                    "tool_use_id": block.id,
                    "content": content,
                    "is_error": is_error,
                })
            # All results of one turn go back in a single message.
            self.messages.append({"role": "user", "content": results})
            rounds += 1


def text_of(msg) -> str:
    return "".join(b.text for b in msg.content if b.type == "text").strip()


def anthropic_turn(model: str, effort: str, on_text, fallbacks: bool = True, client=None):
    """A model_turn that streams from the Claude API (imports the SDK lazily).
    The client resolves credentials from the environment (ANTHROPIC_API_KEY)."""
    if client is None:
        import anthropic

        client = anthropic.Anthropic()
    tools = [{**t, "eager_input_streaming": True} for t in TOOLS]

    def turn(messages, tool_choice):
        params = dict(
            model=model,
            max_tokens=MAX_TOKENS,
            system=SYSTEM_PROMPT,
            tools=tools,
            tool_choice=tool_choice,
            messages=messages,
            output_config={"effort": effort},
            cache_control={"type": "ephemeral"},
        )
        if fallbacks:
            # On a policy decline, the API retries on a fallback model itself.
            params.update(betas=[FALLBACK_BETA], fallbacks="default")
        for attempt in range(3):
            try:
                with client.beta.messages.stream(**params) as stream:
                    for event in stream:
                        if event.type == "text":
                            on_text(event.text)
                    return stream.get_final_message()
            except ValueError:
                # Tool input the SDK could not parse at all: re-issue the turn.
                if attempt == 2:
                    raise
        raise AssertionError("unreachable")

    return turn


def redraw_question(question: str) -> None:
    """Replaces the typed question, drawn backwards by the line editor in a
    terminal without bidirectional support, with its visual-order rows."""
    width = shutil.get_terminal_size().columns
    typed_rows = max(1, -(-(len("you> ") + len(question)) // width))
    sys.stdout.write("\033[1A\033[2K" * typed_rows)
    rows = rtl.display(question, width - len("you> "))
    sys.stdout.write("you> " + "\n     ".join(rows) + "\n")
    sys.stdout.flush()


def main() -> int:
    ap = argparse.ArgumentParser(
        prog="brain-chat",
        description="Ask questions about what you typed, answered from brain's data.",
    )
    ap.add_argument("question", nargs="*", help="a question; without one, opens a chat")
    ap.add_argument(
        "--data-dir",
        default=os.environ.get("BRAIN_DATA_DIR", "./brain-data"),
        help="brain's data directory (default: $BRAIN_DATA_DIR or ./brain-data; "
        "the service's is ~/.local/state/brain)",
    )
    ap.add_argument("--brain", default=os.environ.get("BRAIN_BIN", "brain"), help="the brain binary")
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--effort", default="low", choices=["low", "medium", "high", "xhigh", "max"])
    ap.add_argument("--no-fallback", action="store_true", help="no server-side refusal fallback")
    ap.add_argument(
        "--rtl",
        default=os.environ.get("BRAIN_CHAT_RTL", "auto"),
        choices=["auto", "on", "off"],
        help="show right-to-left text (Hebrew, Arabic) in visual order, right-aligned: "
        "auto = in terminals that cannot do it themselves (default)",
    )
    args = ap.parse_args()

    if not os.path.isdir(args.data_dir):
        print(f"brain-chat: no brain data directory at {args.data_dir} (use --data-dir)", file=sys.stderr)
        return 1
    dim = (lambda s: f"\033[2m{s}\033[0m") if sys.stderr.isatty() else (lambda s: s)
    write = lambda s: (sys.stdout.write(s), sys.stdout.flush())  # noqa: E731
    rtl_on = rtl.enabled(args.rtl, sys.stdout)
    if rtl_on:
        try:
            import bidi  # noqa: F401
        except ImportError:
            print("brain-chat: python-bidi is not installed (make chat-setup); RTL off", file=sys.stderr)
            rtl_on = False
    # With RTL on, answers are written line by line, reordered for display.
    renderer = rtl.LineRenderer(write) if rtl_on else None
    out = renderer.feed if renderer else write
    flush = renderer.finish if renderer else (lambda: None)

    def on_tool(line):
        flush()  # finish any text line before the tool line
        print(dim(f"  → {line}"), file=sys.stderr)

    chat = Chat(
        anthropic_turn(args.model, args.effort, out, fallbacks=not args.no_fallback),
        Tools(args.brain, args.data_dir),
        on_tool=on_tool,
    )

    import anthropic

    def ask(question: str) -> bool:
        try:
            chat.ask(question)
            flush()
            print()
            return True
        except KeyboardInterrupt:
            flush()
            print("\n(stopped)", file=sys.stderr)
        except anthropic.AuthenticationError:
            print("brain-chat: no valid Anthropic API key (set ANTHROPIC_API_KEY)", file=sys.stderr)
            sys.exit(1)
        except anthropic.RateLimitError:
            print("brain-chat: rate limited by the API; try again shortly", file=sys.stderr)
        except anthropic.APIConnectionError:
            print("brain-chat: cannot reach the API (network?)", file=sys.stderr)
        except anthropic.APIStatusError as e:
            print(f"brain-chat: API error {e.status_code}: {e.message}", file=sys.stderr)
        return False

    if args.question:
        return 0 if ask(" ".join(args.question)) else 1
    print(dim("Ask about what you typed. Ctrl+C stops an answer, Ctrl+D quits."), file=sys.stderr)
    while True:
        try:
            question = input("\nyou> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return 0
        if question and rtl_on and rtl.has_rtl(question):
            redraw_question(question)
        if question:
            ask(question)


if __name__ == "__main__":
    sys.exit(main())
