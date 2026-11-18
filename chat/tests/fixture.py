"""A brain data directory with a known day of typing, for the tests.

The capture log is written by hand, in brain's own format, with yesterday's
dates; then `brain run --synthetic` is started on it for a moment, because a
run first ingests the existing log with brain's real engine. (The synthetic
source adds a few keys under the app `synthetic`, which the tests ignore.)
"""

import json
import os
import signal
import subprocess
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BRAIN = os.environ.get("BRAIN_BIN", str(REPO / "target/release/brain"))
KONSOLE = "org.kde.konsole"
LOCAL = "~ : bash — Konsole"
PROD = "juan@prod-db: ~ — Konsole"
STAGING = "juan@staging: /srv — Konsole"
KATE = "org.kde.kate"


def yesterday_at(hour: int, minute: int = 0) -> datetime:
    """Yesterday at the given local time, as an aware datetime."""
    day = datetime.now().astimezone() - timedelta(days=1)
    return day.replace(hour=hour, minute=minute, second=0, microsecond=0)


def _ts(t: datetime) -> str:
    return t.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.") + f"{t.microsecond // 1000:03d}Z"


class Log:
    def __init__(self):
        self.lines = []

    def type(self, t: datetime, app: str, window: str, text: str) -> datetime:
        for c in text:
            key = {"\n": "Enter", "\b": "Backspace"}.get(c, c)
            self.lines.append(json.dumps(
                {"ts": _ts(t), "app": app, "type": "key", "key": key, "mods": [], "window": window},
                ensure_ascii=False,
            ))
            t += timedelta(milliseconds=120)
        return t


def build(data: Path) -> None:
    """Writes yesterday's typing into `data` and builds its index."""
    log = Log()
    t = log.type(yesterday_at(10), KONSOLE, LOCAL, "ssh prod-db\n")
    t = log.type(t + timedelta(seconds=5), KONSOLE, PROD, "df -h\n")
    t = log.type(t + timedelta(seconds=5), KONSOLE, PROD, "sudo systemctl restart nginx\n")
    t = log.type(t + timedelta(seconds=2), KONSOLE, PROD, "hunter22\n")
    t = log.type(t + timedelta(seconds=5), KONSOLE, PROD, "curl -H 'Authorization: Bearer abcdefghijklmnop123' https://api.example.com/health\n")
    t = log.type(t + timedelta(seconds=5), KONSOLE, PROD, "exit\n")
    t = log.type(yesterday_at(15), KONSOLE, STAGING, "ls\n")
    log.type(yesterday_at(16), KATE, "notes.md — Kate", "restarted nginx on prod after the 502s")
    data.mkdir(parents=True, exist_ok=True)
    (data / "events.jsonl").write_text("\n".join(log.lines) + "\n", encoding="utf-8")

    run = subprocess.Popen(
        [BRAIN, "run", "--synthetic", "0.1", "--data-dir", str(data)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.time() + 10
        while not (data / "brain.db").exists() and time.time() < deadline:
            time.sleep(0.1)
        time.sleep(1.5)  # the first ingest pass
    finally:
        run.send_signal(signal.SIGTERM)
        if run.wait(timeout=20) != 0:
            raise RuntimeError("brain run did not stop cleanly")
