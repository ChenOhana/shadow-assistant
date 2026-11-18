#!/usr/bin/env python3
"""A live check of brain-chat against the real Claude API, on the test
fixture (a made-up day: an ssh session to prod-db with a sudo password, a
token in a command, a note). Not part of the unit tests: it needs an API key
and costs a little (a few requests to claude-opus-5-5 at effort low).

    chat/.venv/bin/python chat/live_check.py

Prints each question, the tools Claude called and its answer, and checks
the facts each answer must contain or must not contain.
"""

import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE / "tests")]

import fixture  # noqa: E402
from brain_chat import MODEL, Chat, anthropic_turn  # noqa: E402
from tools import Tools  # noqa: E402

CHECKS = [
    ("Which commands did I run yesterday on prod-db?",
     ["df -h", "systemctl restart nginx", "curl"], ["hunter22", "abcdefghijklmnop123"]),
    ("What did I write in my notes yesterday?", ["502"], []),
    ("Did I run anything on a server called db-backup yesterday?", [], ["df -h"]),
]


def main() -> int:
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        data = Path(tmp) / "data"
        fixture.build(data)
        for question, must, must_not in CHECKS:
            print(f"\n=== {question}")
            chat = Chat(
                anthropic_turn(MODEL, "low", lambda t: None),
                Tools(fixture.BRAIN, str(data)),
                on_tool=lambda line: print(f"  → {line}"),
            )
            answer = chat.ask(question)
            print(answer)
            missing = [m for m in must if m not in answer]
            leaked = [m for m in must_not if m in answer]
            if missing or leaked:
                failures += 1
                print(f"!!! missing {missing}, should not contain {leaked}")
            else:
                print("--- ok")
    print(f"\n{len(CHECKS) - failures}/{len(CHECKS)} checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
