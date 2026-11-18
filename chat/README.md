# brain-chat

Ask questions about what you typed, in plain language, answered by Claude
from brain's data while `brain run` keeps capturing:

```
$ chat/brain-chat --data-dir ~/.local/state/brain "which commands did I run yesterday on prod-db?"
  → list_apps from=2026-11-17 to=2026-11-18
  → sessions app=org.kde.konsole window=prod-db from=2026-11-17 to=2026-11-18
Yesterday on prod-db (Konsole, window "juan@prod-db: ~"):
    10:00  df -h
    10:00  sudo systemctl restart nginx
    10:00  [a password was typed at sudo's prompt]
    ...
```

Without a question it opens a chat; follow-up questions keep the context.
Ctrl+C stops an answer, Ctrl+D quits.

## Setup

```sh
sudo apt install python3-venv     # Ubuntu, once
make chat-setup                   # chat/.venv with the Anthropic SDK
export ANTHROPIC_API_KEY=...      # your key
chat/brain-chat --data-dir ~/brain-data
```

`--data-dir` is brain's data directory (`$BRAIN_DATA_DIR`, default
`./brain-data`; the service's is `~/.local/state/brain`). `--brain` picks the
binary (`$BRAIN_BIN`, default `brain` on the PATH), `--model` the model
(default `claude-opus-5-5`), `--effort` how hard it thinks (default `low`).

## How it answers

```
you ──► brain-chat ──► Claude API (streaming, tool use)
           │  ▲             │ "call sessions(app=konsole, window=prod-db, ...)"
           ▼  │             ▼
      runs the tool ──► `brain query ...` (read-only) ──► brain's data
```

For each question brain-chat sends the conversation and four tools; when
Claude asks for tools, brain-chat runs them on this machine, prints each
call (`→ ...`), sends back the results and asks again, until Claude
answers. Claude decides what to look up and when it has enough: it starts
narrow, stops when the results answer the question, and says so when
nothing matches, citing the time, app and window of what it reports.

| Tool | Runs | For |
|---|---|---|
| `list_apps` | `brain query apps` | the exact app ids (which one is the terminal) |
| `sessions` | `brain query sessions` | sessions by app, window title and time, with their text: in a terminal each command is a session and the title names the host, so this answers "what did I run on X" |
| `search` | `brain query search` | where or when something was written |
| `text` | `brain query text` | everything typed into one app in a time range |

Guard rails: at most 10 rounds of tool calls per question, then one last
request with tools turned off makes Claude answer from what it found; each
result is capped (long texts cut, lists cut with a note to page); a failing
tool (a bad time, an unknown app) goes back to Claude as an error to
correct; arguments are passed to `brain` as an argument list, never
through a shell. Each user message starts with the local time, so
"yesterday" means yesterday; the stable system prompt and tool list are
marked for prompt caching. On a policy decline the API retries on a
fallback model by itself (`--no-fallback` turns that off).

## Privacy

Tool results go to Anthropic's API, and brain's data is your raw typing.
Before anything is sent, `redact.py` masks what looks like a secret:
tokens and keys by their shape (GitHub, Slack, AWS, Anthropic, JWTs, private
keys), `password=`/`token:` values, `--password X`, `Authorization: Bearer
...`, passwords in URLs, `mysql -pSECRET`, `curl -u user:pass`, and a single
password-like word typed right after `sudo`, `su`, `ssh`, `passwd` and
similar commands (their prompt). It is best effort: a secret it does not
recognise is sent as typed. Every tool call is printed, so you see what is
read. The loop runs here (the Messages API), so no conversation is kept on
Anthropic's side by brain-chat; nothing is written to disk.

## Tests

```sh
make chat-test                 # offline: 23 tests, no API calls
chat/.venv/bin/python chat/live_check.py   # live: 3 questions, costs a little
```

The offline tests cover the redaction, the tools against a real brain data
directory (a made-up day with an ssh session, a sudo password and a token,
ingested by brain itself), the question loop with a scripted model
(rounds, the cap, parallel calls, errors, refusals, interruption), and the
request and streamed reply through the real SDK with the network replaced.
The SDK tests run once `make chat-setup` has installed it.
