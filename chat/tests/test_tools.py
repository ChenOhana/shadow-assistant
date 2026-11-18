"""The tools against a real brain data directory (no model, no network)."""

import json
import subprocess
import sys
import tempfile
import unittest
from datetime import date, timedelta
from pathlib import Path

sys.path[:0] = [str(Path(__file__).resolve().parents[1]), str(Path(__file__).resolve().parent)]

import fixture  # noqa: E402
from redact import MASK  # noqa: E402
from tools import MAX_RESULT_CHARS, TOOLS, Tools, describe  # noqa: E402

YESTERDAY = (date.today() - timedelta(days=1)).isoformat()
TODAY = date.today().isoformat()


class AgainstBrain(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        data = Path(cls.tmp.name) / "data"
        fixture.build(data)
        cls.tools = Tools(fixture.BRAIN, str(data))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_ok(self, name, args):
        content, is_error = self.tools.run(name, args)
        self.assertFalse(is_error, content)
        return json.loads(content)

    def test_commands_on_a_host_are_listed_and_redacted(self):
        out = self.run_ok("sessions", {"app": "konsole", "window": "prod-db", "from": YESTERDAY, "to": TODAY})
        self.assertEqual(
            [s["text"] for s in out["sessions"]],
            [
                "df -h",
                "sudo systemctl restart nginx",
                MASK,  # the password typed at sudo's prompt
                f"curl -H 'Authorization: Bearer {MASK}' https://api.example.com/health",
                "exit",
            ],
        )
        self.assertTrue(all(s["end_reason"] == "commit" for s in out["sessions"]))
        self.assertNotIn("hunter22", json.dumps(out))

    def test_list_apps(self):
        apps = [a["app"] for a in self.run_ok("list_apps", {"from": YESTERDAY, "to": TODAY})["apps"]]
        self.assertEqual(apps, ["org.kde.kate", "org.kde.konsole"])

    def test_search_and_text(self):
        out = self.run_ok("search", {"words": "nginx", "from": YESTERDAY, "to": TODAY})
        self.assertEqual(
            [r["app"] for r in out["results"]], ["org.kde.kate", "org.kde.konsole"]
        )
        out = self.run_ok("text", {"app": "konsole", "from": f"{YESTERDAY} 09:59", "to": f"{YESTERDAY} 10:10"})
        self.assertIn("ssh prod-db", out["text"])
        self.assertIn(MASK, out["text"])
        self.assertNotIn("hunter22", out["text"])
        self.assertNotIn("abcdefghijklmnop123", out["text"])

    def test_errors_go_back_to_the_model(self):
        for name, args, part in [
            ("sessions", {"from": "the day before"}, "time"),
            ("text", {"app": "nonexistent-app", "from": YESTERDAY}, "nonexistent-app"),
            ("sessions", {"at": "-1h", "from": "-2h"}, "at cannot be combined"),
            ("sessions", {"limit": 0}, "out of range"),
            ("sessions", {"colour": "red"}, "unknown parameter"),
            ("search", {}, "words is required"),
            ("rm", {}, "unknown tool"),
        ]:
            content, is_error = self.tools.run(name, args)
            self.assertTrue(is_error, f"{name} {args}: {content}")
            self.assertIn(part, content.lower() if part.islower() else content, f"{name} {args}")
        missing = Tools("/nonexistent/brain", self.tools.data_dir)
        content, is_error = missing.run("list_apps", {})
        self.assertTrue(is_error)
        self.assertIn("not installed", content)


class Offline(unittest.TestCase):
    def test_big_results_are_cut_with_a_note(self):
        rows = [{"session_id": str(i), "app": "a", "window": "w", "started_at": "t", "ended_at": "t",
                 "end_reason": "idle", "text": "x" * 1500} for i in range(100)]

        def fake(argv, **_):
            body = {"app": None, "window": None, "from": None, "to": None, "total": 100, "offset": 0, "sessions": rows}
            return subprocess.CompletedProcess(argv, 0, json.dumps(body), "")

        content, is_error = Tools("brain", "d", run=fake).run("sessions", {})
        self.assertFalse(is_error)
        self.assertLessEqual(len(content), MAX_RESULT_CHARS)
        out = json.loads(content)
        self.assertIn("cut to fit", out["note"])
        self.assertEqual(out["total"], 100)

    def test_arguments_never_go_through_a_shell(self):
        seen = []

        def fake(argv, **kwargs):
            seen.append((argv, kwargs))
            return subprocess.CompletedProcess(argv, 0, '{"results": []}', "")

        Tools("brain", "d", run=fake).run("search", {"words": "x; rm -rf ~", "app": "$(id)"})
        argv, kwargs = seen[0]
        self.assertEqual(argv, ["brain", "query", "search", "x; rm -rf ~", "--app", "$(id)", "--data-dir", "d"])
        self.assertNotIn("shell", kwargs)

    def test_schemas_and_description(self):
        self.assertEqual({t["name"] for t in TOOLS}, {"list_apps", "sessions", "search", "text"})
        self.assertEqual(describe("sessions", {"app": "konsole", "from": "-1d"}), "sessions app=konsole from=-1d")


if __name__ == "__main__":
    unittest.main()
