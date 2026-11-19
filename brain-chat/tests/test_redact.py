"""Redaction: secrets are masked, ordinary typing is left alone."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from redact import MASK, redact_after_prompts, redact_text  # noqa: E402


class RedactText(unittest.TestCase):
    def test_token_shapes(self):
        for secret in [
            "sk-ant-api03-abcdefghijklmnop",
            "ghp_" + "a" * 36,
            "xoxb-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.c2lnbmF0dXJlLXNpZw",
        ]:
            out = redact_text(f"export X={secret} # note")
            self.assertNotIn(secret, out)

    def test_private_key(self):
        key = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3Nz\nxyz\n-----END OPENSSH PRIVATE KEY-----"
        self.assertEqual(redact_text(f"before\n{key}\nafter"), f"before\n{MASK}\nafter")

    def test_lead_ins_keep_the_lead(self):
        cases = {
            "password=hunter22 rest": f"password={MASK} rest",
            "DB_PASSWORD: s3cr3t!": f"DB_PASSWORD: {MASK}",
            '{"api_key": "abc123xyz"}': '{"api_key": "' + MASK + '"}',
            "deploy --token=abc.def.ghi now": f"deploy --token={MASK} now",
            "curl -H 'Authorization: Bearer abcdefghijklmnop12'": f"curl -H 'Authorization: Bearer {MASK}'",
            "psql postgres://app:S3cret@db:5432/app": f"psql postgres://app:{MASK}@db:5432/app",
            "mysql -u root -pS3cret mydb": f"mysql -u root -p{MASK} mydb",
            "curl -u admin:pa55word https://x": f"curl -u admin:{MASK} https://x",
        }
        for text, want in cases.items():
            self.assertEqual(redact_text(text), want, text)

    def test_ordinary_typing_is_untouched(self):
        for text in [
            "git commit -m 'fix the password reset page'",
            "bypass=true",
            "sudo systemctl restart nginx",
            "the migration is blocked on the schema change",
            "git checkout 3f2a9c1e5b7d4a6f8e0c2b4d6f8a0c2e4b6d8f0a",
            "mysql -u root -p mydb",
            "ssh juan@prod-db",
            "שלום עולם",
        ]:
            self.assertEqual(redact_text(text), text, text)


class RedactAfterPrompts(unittest.TestCase):
    def test_password_after_a_prompting_command(self):
        lines = ["sudo systemctl restart nginx", "hunter22", "df -h"]
        self.assertEqual(redact_after_prompts(lines), ["sudo systemctl restart nginx", MASK, "df -h"])

    def test_ordinary_next_commands_stay(self):
        for nxt in ["ls", "exit", "htop", "df -h", "./deploy.sh", "/usr/bin/top", "uptime"]:
            self.assertEqual(redact_after_prompts(["ssh prod-db", nxt]), ["ssh prod-db", nxt], nxt)

    def test_only_right_after_the_prompt(self):
        lines = ["ls -la", "hunter22"]
        self.assertEqual(redact_after_prompts(lines), lines)


if __name__ == "__main__":
    unittest.main()
