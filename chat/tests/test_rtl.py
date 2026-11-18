"""Right-to-left text: visual order, wrapping, alignment, code blocks,
streaming in pieces, and when it is turned on."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import rtl  # noqa: E402

try:
    import bidi  # noqa: F401
except ImportError:  # pragma: no cover
    bidi = None


class Tty:
    def __init__(self, tty):
        self.tty = tty

    def isatty(self):
        return self.tty


class Enabled(unittest.TestCase):
    def test_auto_detection(self):
        self.assertTrue(rtl.enabled("auto", Tty(True), {"TERM": "xterm-ghostty"}))
        self.assertTrue(rtl.enabled("auto", Tty(True), {"TERM": "xterm-kitty"}))
        self.assertFalse(rtl.enabled("auto", Tty(True), {"KONSOLE_VERSION": "250801"}))
        self.assertFalse(rtl.enabled("auto", Tty(True), {"VTE_VERSION": "8000"}))
        self.assertFalse(rtl.enabled("auto", Tty(True), {"TERM": "mlterm"}))
        self.assertFalse(rtl.enabled("auto", Tty(False), {"TERM": "xterm-ghostty"}))  # a pipe

    def test_forced(self):
        self.assertTrue(rtl.enabled("on", Tty(False), {}))
        self.assertFalse(rtl.enabled("off", Tty(True), {"TERM": "xterm-ghostty"}))

    def test_direction(self):
        self.assertTrue(rtl.is_rtl("- שלום world"))
        self.assertFalse(rtl.is_rtl("hello שלום"))
        self.assertFalse(rtl.has_rtl("df -h"))


@unittest.skipIf(bidi is None, "python-bidi is not installed")
class Display(unittest.TestCase):
    def test_hebrew_line_is_reversed_and_right_aligned(self):
        self.assertEqual(rtl.display("שלום עולם", 20), ["           םלוע םולש"])

    def test_mixed_line_keeps_english_numbers_and_commands(self):
        (row,) = rtl.display("הרצת df -h ב-10:00 על prod-db", 40)
        self.assertEqual(row.strip(), "prod-db לע 10:00-ב df -h תצרה")
        self.assertEqual(len(row), 40)

    def test_english_lines_are_untouched(self):
        self.assertEqual(rtl.display("You ran df -h at 10:00.", 40), ["You ran df -h at 10:00."])
        (row,) = rtl.display("You wrote שלום here", 40)
        self.assertEqual(row, "You wrote םולש here")  # left to right, not aligned

    def test_long_lines_wrap_before_reordering(self):
        line = "אתמול בעשר בבוקר הרצת שלוש פקודות על השרת prod-db"
        rows = rtl.display(line, 20)
        self.assertGreater(len(rows), 1)
        self.assertTrue(all(len(r) == 20 for r in rows))
        # The first logical words are on the first row, at its right end.
        self.assertTrue(rows[0].rstrip().endswith("לומתא"))


@unittest.skipIf(bidi is None, "python-bidi is not installed")
class Streaming(unittest.TestCase):
    def test_pieces_are_written_as_whole_reordered_lines(self):
        out = []
        r = rtl.LineRenderer(out.append, width=20)
        for piece in ["של", "ום עו", "לם\nhel", "lo"]:
            r.feed(piece)
        self.assertEqual(out, ["           םלוע םולש\n"])
        r.finish()
        self.assertEqual(out[-1], "hello")

    def test_code_blocks_are_written_unchanged(self):
        out = []
        r = rtl.LineRenderer(out.append, width=20)
        r.feed("```\necho שלום\n```\nשלום\n")
        self.assertEqual(out, ["```\n", "echo שלום\n", "```\n", "                םולש\n"])


if __name__ == "__main__":
    unittest.main()
