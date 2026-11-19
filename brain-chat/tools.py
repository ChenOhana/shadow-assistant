"""The model's tools: read-only wrappers around `brain query`.

Each tool runs `brain query <command> ... --data-dir D` as a subprocess (an
argument list, never a shell), checks the model's input first, caps the
size of the result, and redacts likely secrets (see ``redact``) before the
result goes back to the model. Errors (a bad time, no such app) are
returned as error results, so the model can correct itself.
"""

import json
import subprocess

from redact import redact_after_prompts, redact_text

# Per session or window text, and per whole result, in characters.
MAX_TEXT_CHARS = 2_000
MAX_RESULT_CHARS = 24_000
MAX_LIMIT = 200
TIMEOUT_S = 30

_TIME_HELP = (
    "Times: relative (-30m, -2h, -1d, -1h30m, now), local date/time "
    "('2026-11-17 14:00', '2026-11-17' = midnight, '14:00' = today), or RFC 3339."
)
_TIME = {"type": "string", "description": _TIME_HELP}

TOOLS = [
    {
        "name": "list_apps",
        "description": (
            "The apps that have writing sessions in the time window, most recently "
            "active first, with session counts. Use it to find the exact app id "
            "(for example org.kde.konsole for the terminal) before filtering by app. "
            "Without from/to: all time."
        ),
        "input_schema": {
            "type": "object",
            "properties": {"from": _TIME, "to": _TIME},
            "additionalProperties": False,
        },
    },
    {
        "name": "sessions",
        "description": (
            "Writing sessions, oldest first, each with its app, window title, start "
            "and end time, end reason and full text. Filter by app id substring, "
            "window-title substring and time. In a terminal every submitted command "
            "is its own session (end_reason 'commit'), and the window title usually "
            "names the host (user@host: dir), so window='<host>' lists the commands "
            "run on that host. Returns `total` matches; page with offset. Without "
            "from/to/at: all time; at=T is the 10 minutes ending at T."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "app": {"type": "string", "description": "Part of the app id, any case."},
                "window": {"type": "string", "description": "Part of the window title, any case."},
                "from": _TIME,
                "to": _TIME,
                "at": _TIME,
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                "offset": {"type": "integer", "minimum": 0},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "search",
        "description": (
            "Sessions whose text contains every word (substrings, any case, any "
            "language), newest first, with a snippet around the match. Use it to find "
            "where or when something was written. Without from/to/at: all time."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "words": {"type": "string", "description": "One or more words."},
                "app": {"type": "string", "description": "Part of the app id, any case."},
                "from": _TIME,
                "to": _TIME,
                "at": _TIME,
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
            },
            "required": ["words"],
            "additionalProperties": False,
        },
    },
    {
        "name": "text",
        "description": (
            "What was typed into one app during a time range, reconstructed (edits "
            "applied), one text per window title. The app must match exactly one app "
            "id. Without from/to/at: the last 10 minutes; at=T is the 10 minutes "
            "ending at T."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "app": {"type": "string", "description": "Part of the app id, any case."},
                "from": _TIME,
                "to": _TIME,
                "at": _TIME,
            },
            "required": ["app"],
            "additionalProperties": False,
        },
    },
]

_SCHEMAS = {t["name"]: t["input_schema"] for t in TOOLS}


class ToolError(Exception):
    """A problem to report to the model as an error result."""


def check_input(name: str, args) -> dict:
    """Validates the model's input against the tool's schema (the shapes used here)."""
    schema = _SCHEMAS.get(name)
    if schema is None:
        raise ToolError(f"unknown tool {name!r}")
    if not isinstance(args, dict):
        raise ToolError("the input must be an object")
    props = schema["properties"]
    for key, value in args.items():
        if key not in props:
            raise ToolError(f"unknown parameter {key!r}")
        kind = props[key]["type"]
        if kind == "string" and not (isinstance(value, str) and value.strip()):
            raise ToolError(f"{key} must be a non-empty string")
        if kind == "integer":
            if not isinstance(value, int) or isinstance(value, bool):
                raise ToolError(f"{key} must be an integer")
            if value < props[key].get("minimum", value) or value > props[key].get("maximum", value):
                raise ToolError(f"{key} is out of range")
    for key in schema.get("required", []):
        if key not in args:
            raise ToolError(f"{key} is required")
    if "at" in args and ("from" in args or "to" in args):
        raise ToolError("at cannot be combined with from or to")
    return args


def describe(name: str, args: dict) -> str:
    """One line for the terminal: what is about to be read."""
    parts = " ".join(f"{k}={v!r}" if " " in str(v) else f"{k}={v}" for k, v in args.items())
    return f"{name} {parts}".strip()


def _truncate(text: str) -> str:
    if len(text) <= MAX_TEXT_CHARS:
        return text
    return text[:MAX_TEXT_CHARS] + f"… [{len(text) - MAX_TEXT_CHARS} more characters cut]"


class Tools:
    """Runs the tools against one brain data directory."""

    def __init__(self, brain: str, data_dir: str, run=subprocess.run):
        self.brain = brain
        self.data_dir = data_dir
        self._run = run

    def query(self, command: str, *args: str) -> dict:
        argv = [self.brain, "query", command, *args, "--data-dir", self.data_dir]
        try:
            out = self._run(argv, capture_output=True, text=True, timeout=TIMEOUT_S)
        except FileNotFoundError:
            raise ToolError(f"brain is not installed or not at {self.brain!r}") from None
        except subprocess.TimeoutExpired:
            raise ToolError(f"brain query {command} took longer than {TIMEOUT_S} s") from None
        if out.returncode != 0:
            raise ToolError(out.stderr.strip().removeprefix("brain: error: ") or "brain query failed")
        return json.loads(out.stdout)

    def run(self, name: str, args) -> tuple[str, bool]:
        """Runs a tool; returns (content, is_error)."""
        try:
            args = check_input(name, args)
            result = getattr(self, f"_{name}")(args)
        except ToolError as e:
            return str(e), True
        except json.JSONDecodeError:
            return "brain returned output that is not JSON", True
        return result, False

    @staticmethod
    def _flags(args: dict, *names: str) -> list[str]:
        flags = []
        for n in names:
            if n in args:
                flags += [f"--{n}", str(args[n])]
        return flags

    def _list_apps(self, args: dict) -> str:
        return _fit(self.query("apps", *self._flags(args, "from", "to")))

    def _sessions(self, args: dict) -> str:
        out = self.query(
            "sessions", *self._flags(args, "app", "window", "from", "to", "at", "limit", "offset")
        )
        sessions = out.get("sessions", [])
        for s in sessions:
            s["text"] = _truncate(redact_text(s["text"]))
        # A password typed after `sudo` is its own session in the same window.
        by_window: dict[tuple, list] = {}
        for s in sessions:
            by_window.setdefault((s["app"], s["window"]), []).append(s)
        for group in by_window.values():
            for s, text in zip(group, redact_after_prompts([s["text"] for s in group])):
                s["text"] = text
        return _fit(out, "sessions")

    def _search(self, args: dict) -> str:
        out = self.query(
            "search", args["words"], *self._flags(args, "app", "from", "to", "at", "limit")
        )
        for r in out.get("results", []):
            r["snippet"] = redact_text(r["snippet"])
        return _fit(out, "results")

    def _text(self, args: dict) -> str:
        out = self.query("text", *self._flags(args, "app", "from", "to", "at"))
        for w in out.get("windows", []):
            lines = redact_after_prompts(redact_text(w["text"]).split("\n"))
            w["text"] = _truncate("\n".join(lines))
        out["text"] = "\n".join(w["text"] for w in out.get("windows", []))
        return _fit(out, "windows")


def _fit(out: dict, items: str | None = None) -> str:
    """Compact JSON, dropping list items from the end past MAX_RESULT_CHARS."""
    text = json.dumps(out, ensure_ascii=False, separators=(",", ":"))
    if items is None or len(text) <= MAX_RESULT_CHARS:
        return text
    rows = out[items]
    dropped = 0
    while rows and len(text) > MAX_RESULT_CHARS:
        rows.pop()
        dropped += 1
        if items == "windows":
            out["text"] = "\n".join(w["text"] for w in rows)
        out["note"] = f"{dropped} more {items} cut to fit; narrow the query or page with offset"
        text = json.dumps(out, ensure_ascii=False, separators=(",", ":"))
    return text
