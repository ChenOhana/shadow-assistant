"""Right-to-left text (Hebrew, Arabic) in terminals that cannot show it.

A terminal draws characters left to right in the order they are written,
and text is stored in logical order (first letter first), so a terminal
without bidirectional support (Ghostty, kitty, Alacritty, xterm) shows
Hebrew backwards. Konsole, GNOME Terminal (VTE) and mlterm reorder it
themselves; reordering it here too would reverse it twice.

Where the terminal does not, brain-chat writes each line already in visual
order (the Unicode Bidirectional Algorithm, via python-bidi), wraps long
lines itself first (a terminal wrapping a reordered line breaks the order
again), and right-aligns lines whose text is right-to-left, like a chat
app. Code blocks (commands) are left as they are.
"""

import os
import shutil
import textwrap
import unicodedata

def decide(mode: str, stream, env=os.environ) -> tuple[bool, str]:
    """Whether brain-chat should reorder, and why: `on`, `off`, or `auto`
    (on for a terminal that does not reorder by itself; off for pipes and
    files)."""
    if mode in ("on", "off"):
        return mode == "on", f"--rtl {mode}"
    if not stream.isatty():
        return False, "output is not a terminal"
    if env.get("KONSOLE_VERSION"):
        return False, "Konsole reorders by itself"
    if env.get("VTE_VERSION"):
        return False, "GNOME Terminal (VTE) reorders by itself"
    if env.get("TERM", "").startswith("mlterm"):
        return False, "mlterm reorders by itself"
    terminal = env.get("TERM_PROGRAM") or env.get("TERM") or "this terminal"
    return True, f"{terminal} does not reorder"


def enabled(mode: str, stream, env=os.environ) -> bool:
    return decide(mode, stream, env)[0]


def is_rtl(text: str) -> bool:
    """Whether the first strongly directional character is right-to-left."""
    for c in text:
        kind = unicodedata.bidirectional(c)
        if kind in ("R", "AL"):
            return True
        if kind == "L":
            return False
    return False


def has_rtl(text: str) -> bool:
    return any(unicodedata.bidirectional(c) in ("R", "AL") for c in text)


def display(line: str, width: int) -> list[str]:
    """One logical line as terminal rows: wrapped to `width`, each row in
    visual order, right-aligned when the line is right-to-left."""
    if not has_rtl(line):
        return [line]
    from bidi import get_display  # only needed when reordering

    rtl = is_rtl(line)
    rows = textwrap.wrap(line, width=max(width, 10), break_long_words=True,
                         replace_whitespace=False, drop_whitespace=True) or [""]
    out = []
    for row in rows:
        visual = get_display(row, base_dir="R" if rtl else "L")
        out.append(visual.rjust(width) if rtl else visual)
    return out


class LineRenderer:
    """Takes streamed text and writes it as whole lines, reordered for the
    terminal; lines inside ``` code blocks are written unchanged."""

    def __init__(self, write, width=None):
        self.write = write
        self.width = width
        self.pending = ""
        self.in_code = False

    def _width(self) -> int:
        return self.width or shutil.get_terminal_size().columns

    def feed(self, text: str) -> None:
        self.pending += text
        while "\n" in self.pending:
            line, self.pending = self.pending.split("\n", 1)
            self._emit(line)

    def finish(self) -> None:
        """Writes a last line that has no newline yet."""
        if self.pending:
            line, self.pending = self.pending, ""
            self._emit(line, newline=False)

    def _emit(self, line: str, newline: bool = True) -> None:
        if line.lstrip().startswith("```"):
            self.in_code = not self.in_code
            rows = [line]
        elif self.in_code:
            rows = [line]
        else:
            rows = display(line, self._width())
        self.write("\n".join(rows) + ("\n" if newline else ""))
