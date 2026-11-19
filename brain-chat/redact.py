"""Masks likely secrets in brain's data before it is sent to the model.

Best effort: brain records raw typing, so a password typed at a prompt, a
token pasted into a terminal or a connection string can be in it. Two
passes:

* ``redact_text``: patterns that look like secrets wherever they appear
  (API tokens, private keys, credentials in URLs, ``password=...``,
  ``--token X``, ``Authorization: Bearer ...``, ``mysql -pSECRET``).
* ``redact_after_prompts``: in a sequence of lines typed in one terminal
  window, a single word typed right after a command that asks for a
  password (``sudo``, ``su``, ``ssh``, ``passwd``, ...) is most likely that
  password, when it looks like one (6+ characters, not all letters).

A masked value is replaced with ``[redacted]``. What is not recognised is
sent as typed; the README says so.
"""

import re

MASK = "[redacted]"

# Whole tokens that are secrets by their shape.
_TOKEN_PATTERNS = [
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
    r"\bsk-ant-[A-Za-z0-9_\-]{10,}",  # Anthropic
    r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}",  # OpenAI-style
    r"\bgh[pousr]_[A-Za-z0-9]{30,}",  # GitHub
    r"\bgithub_pat_[A-Za-z0-9_]{30,}",
    r"\bglpat-[A-Za-z0-9_\-]{20,}",  # GitLab
    r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",  # Slack
    r"\bAKIA[0-9A-Z]{16}\b",  # AWS access key id
    r"\bAIza[0-9A-Za-z_\-]{35}\b",  # Google API key
    r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",  # JWT
]

# Secrets that follow a recognisable lead-in: the lead-in is kept.
_LEAD_PATTERNS = [
    # password=..., DB_PASSWORD: ..., "api_key": "..." (no letter before the
    # name, so `bypass=` is not a password)
    r"""(?i)((?<![a-z])(?:pass(?:word|wd|phrase)?|pwd|secret|token|api[_-]?key|access[_-]?key|auth|credentials?)["']?\s*[:=]\s*["']?)[^\s"',;]+""",
    # --password X, --token=X, --api-key X
    r"""(?i)(--(?:pass(?:word)?|token|secret|api[_-]?key|auth(?:-token)?)[=\s]+["']?)[^\s"']+""",
    # Authorization: Bearer xyz / Basic xyz
    r"""(?i)(\b(?:bearer|basic)\s+)[A-Za-z0-9._~+/=\-]{12,}""",
    # scheme://user:password@host
    r"""(\b[a-z][a-z0-9+.\-]*://[^\s:/@]+:)[^\s@/]+(?=@)""",
    # mysql -pSECRET, mariadb -pSECRET (the password glued to -p)
    r"""(\b(?:mysql|mariadb|mysqldump)\b[^\n]*?\s-p)(?!\s)[^\s]+""",
    # curl -u user:password
    r"""(\bcurl\b[^\n]*?\s(?:-u|--user)\s+[^\s:]+:)[^\s]+""",
]

_TOKENS = [re.compile(p, re.DOTALL) for p in _TOKEN_PATTERNS]
_LEADS = [re.compile(p) for p in _LEAD_PATTERNS]

# Commands that make the next line a password prompt.
_PROMPTING = re.compile(
    r"^\s*(?:sudo|su|doas|ssh|scp|sftp|passwd|chpasswd|kinit|gpg|pkexec|"
    r"mysql|psql|mariadb|ansible-vault|openssl|git\s+(?:push|pull|clone|fetch))\b"
)


def redact_text(text: str) -> str:
    """Masks secret-shaped values anywhere in ``text``."""
    for p in _TOKENS:
        text = p.sub(MASK, text)
    for p in _LEADS:
        text = p.sub(lambda m: m.group(1) + MASK, text)
    return text


def looks_like_password(line: str) -> bool:
    """A single word of 6+ characters that is not all letters."""
    word = line.strip()
    return (
        len(word) >= 6
        and not any(c.isspace() for c in word)
        and not word.isalpha()
        and not word.startswith(("/", "./", "~", "-"))
    )


def redact_after_prompts(lines: list[str]) -> list[str]:
    """Masks a likely password typed right after a command that prompts for one."""
    out = list(lines)
    for i in range(1, len(out)):
        if _PROMPTING.match(lines[i - 1]) and looks_like_password(lines[i]):
            out[i] = MASK
    return out
