"""The question loop, with a scripted fake model (no API, no network)."""

import sys
import unittest
from pathlib import Path
from types import SimpleNamespace as NS

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from brain_chat import FINAL_ROUND_NOTE, MAX_TOOL_ROUNDS, Chat  # noqa: E402


def text(t):
    return NS(type="text", text=t)


def tool(i, name="sessions", **args):
    return NS(type="tool_use", id=f"tu{i}", name=name, input=args)


def message(stop, *blocks):
    return NS(stop_reason=stop, content=list(blocks))


class FakeTools:
    def __init__(self):
        self.calls = []

    def run(self, name, args):
        self.calls.append((name, args))
        if args.get("from") == "bad":
            return "unrecognised time 'bad'", True
        return f'{{"tool": "{name}"}}', False


class Script:
    """A model that answers with the given messages in order, recording requests."""

    def __init__(self, *replies):
        self.replies = list(replies)
        self.requests = []

    def __call__(self, messages, tool_choice):
        self.requests.append((list(messages), tool_choice))
        reply = self.replies.pop(0)
        if isinstance(reply, BaseException):
            raise reply
        return reply(tool_choice) if callable(reply) else reply


class Loop(unittest.TestCase):
    def test_tools_then_answer(self):
        model = Script(
            message("tool_use", text("Looking."), tool(1, app="konsole", window="prod-db")),
            message("end_turn", text("You ran `df -h`.")),
        )
        tools, lines = FakeTools(), []
        chat = Chat(model, tools, on_tool=lines.append)
        # The final answer (text from earlier turns was already streamed).
        self.assertEqual(chat.ask("which commands on prod-db?"), "You ran `df -h`.")
        self.assertEqual(tools.calls, [("sessions", {"app": "konsole", "window": "prod-db"})])
        self.assertEqual(lines, ["sessions app=konsole window=prod-db"])
        roles = [m["role"] for m in chat.messages]
        self.assertEqual(roles, ["user", "assistant", "user", "assistant"])
        self.assertTrue(chat.messages[0]["content"].startswith("[Local time: "))
        result = chat.messages[2]["content"][0]
        self.assertEqual((result["type"], result["tool_use_id"], result["is_error"]), ("tool_result", "tu1", False))

    def test_parallel_calls_answer_in_one_message(self):
        model = Script(
            message("tool_use", tool(1, "list_apps"), tool(2, "search", words="nginx")),
            message("end_turn", text("done")),
        )
        chat = Chat(model, FakeTools())
        chat.ask("q")
        results = chat.messages[2]["content"]
        self.assertEqual([r["tool_use_id"] for r in results], ["tu1", "tu2"])

    def test_tool_errors_go_back_as_errors(self):
        model = Script(message("tool_use", tool(1, **{"from": "bad"})), message("end_turn", text("fixed")))
        chat = Chat(model, FakeTools())
        chat.ask("q")
        self.assertTrue(chat.messages[2]["content"][0]["is_error"])

    def test_rounds_are_capped_then_it_must_answer(self):
        always_tools = lambda choice: (  # noqa: E731
            message("end_turn", text("From what I found: ..."))
            if choice == {"type": "none"}
            else message("tool_use", tool(len(model.requests)))
        )
        model = Script(*[always_tools] * (MAX_TOOL_ROUNDS + 1))
        tools = FakeTools()
        chat = Chat(model, tools)
        self.assertEqual(chat.ask("q"), "From what I found: ...")
        self.assertEqual(len(tools.calls), MAX_TOOL_ROUNDS)
        last_messages, last_choice = model.requests[-1]
        self.assertEqual(last_choice, {"type": "none"})
        self.assertEqual(last_messages[-1], {"role": "system", "content": FINAL_ROUND_NOTE})
        self.assertTrue(all(c == {"type": "auto"} for _, c in model.requests[:-1]))

    def test_the_final_round_always_ends(self):
        # Even if the model asks for tools when they are off, nothing runs
        # and the question ends.
        model = Script(*[message("tool_use", tool(i)) for i in range(MAX_TOOL_ROUNDS + 1)])
        tools = FakeTools()
        self.assertIn("limit", Chat(model, tools).ask("q"))
        self.assertEqual(len(tools.calls), MAX_TOOL_ROUNDS)
        self.assertEqual(len(model.requests), MAX_TOOL_ROUNDS + 1)

    def test_refusal_and_cut_off_answers(self):
        chat = Chat(Script(message("refusal")), FakeTools())
        self.assertIn("declined", chat.ask("q"))
        chat = Chat(Script(message("max_tokens", text("partial"))), FakeTools())
        self.assertIn("cut off", chat.ask("q"))

    def test_an_interrupted_question_leaves_the_history_as_it_was(self):
        model = Script(
            message("end_turn", text("first answer")),
            message("tool_use", tool(1)),
            KeyboardInterrupt(),
        )
        chat = Chat(model, FakeTools())
        chat.ask("first")
        before = list(chat.messages)
        with self.assertRaises(KeyboardInterrupt):
            chat.ask("second")
        self.assertEqual(chat.messages, before)


if __name__ == "__main__":
    unittest.main()
