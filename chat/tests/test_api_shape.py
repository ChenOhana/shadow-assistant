"""The real SDK path, offline: the request brain-chat builds, and a streamed
reply with text and a tool call, through the SDK's own parsing. The HTTP
transport is replaced, so nothing leaves the machine. Skipped when the
anthropic package is not installed."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

try:
    import anthropic
    import httpx2 as httpx
except ImportError:  # pragma: no cover
    anthropic = None

from brain_chat import MODEL, SYSTEM_PROMPT, Chat, anthropic_turn  # noqa: E402


def sse(*events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)


REPLY = sse(
    {"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": MODEL, "content": [],
        "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": 10, "output_tokens": 1}}},
    {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
    {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Checking prod-db."}},
    {"type": "content_block_stop", "index": 0},
    {"type": "content_block_start", "index": 1,
     "content_block": {"type": "tool_use", "id": "toolu_1", "name": "sessions", "input": {}}},
    {"type": "content_block_delta", "index": 1,
     "delta": {"type": "input_json_delta", "partial_json": '{"window": "prod-db"}'}},
    {"type": "content_block_stop", "index": 1},
    {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None},
     "usage": {"output_tokens": 20}},
    {"type": "message_stop"},
)


@unittest.skipIf(anthropic is None, "anthropic is not installed")
class ApiShape(unittest.TestCase):
    def test_request_and_streamed_tool_call(self):
        sent = []

        def handler(request):
            sent.append(request)
            return httpx.Response(200, text=REPLY, headers={"content-type": "text/event-stream"})

        client = anthropic.Anthropic(api_key="test", http_client=httpx.Client(transport=httpx.MockTransport(handler)))
        streamed = []
        turn = anthropic_turn(MODEL, "low", streamed.append, client=client)
        msg = turn([{"role": "user", "content": "q"}], {"type": "auto"})

        body = json.loads(sent[0].content)
        self.assertEqual(body["model"], "claude-opus-5-5")
        self.assertEqual(body["system"], SYSTEM_PROMPT)
        self.assertEqual(body["output_config"], {"effort": "low"})
        self.assertEqual(body["cache_control"], {"type": "ephemeral"})
        self.assertEqual(body["fallbacks"], "default")
        self.assertTrue(body["stream"])
        self.assertIn("server-side-fallback-2026-07-01", sent[0].headers["anthropic-beta"])
        self.assertEqual([t["name"] for t in body["tools"]], ["list_apps", "sessions", "search", "text"])
        self.assertTrue(all(t["eager_input_streaming"] for t in body["tools"]))

        self.assertEqual(streamed, ["Checking prod-db."])
        self.assertEqual(msg.stop_reason, "tool_use")
        call = [b for b in msg.content if b.type == "tool_use"][0]
        self.assertEqual((call.name, call.input), ("sessions", {"window": "prod-db"}))

    def test_the_final_round_sends_a_system_message_with_tools_off(self):
        sent = []

        def handler(request):
            sent.append(json.loads(request.content))
            return httpx.Response(200, text=REPLY, headers={"content-type": "text/event-stream"})

        client = anthropic.Anthropic(api_key="test", http_client=httpx.Client(transport=httpx.MockTransport(handler)))

        class NoTools:
            def run(self, name, args):
                return "{}", False

        import brain_chat

        saved, brain_chat.MAX_TOOL_ROUNDS = brain_chat.MAX_TOOL_ROUNDS, 1
        try:
            # The canned reply always asks for tools: one round, then the final.
            Chat(anthropic_turn(MODEL, "low", lambda t: None, client=client), NoTools()).ask("q")
        finally:
            brain_chat.MAX_TOOL_ROUNDS = saved
        self.assertEqual(len(sent), 2)
        self.assertEqual(sent[-1]["tool_choice"], {"type": "none"})
        self.assertEqual(sent[-1]["messages"][-1]["role"], "system")
        self.assertEqual(sent[-1]["messages"][-2]["content"][0]["type"], "tool_result")


if __name__ == "__main__":
    unittest.main()
