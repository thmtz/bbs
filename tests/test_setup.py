"""Exercise setup against isolated harness directories, without touching user config."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "setup.py"
SKILL = REPO / "integrations" / "skills" / "bbs"


class SetupTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="bbs-setup-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.claude = self.root / "claude"
        self.codex = self.root / "codex"
        self.skills = self.root / "skills"
        self.settings = self.claude / "settings.json"
        self.hooks = self.codex / "hooks.json"

    def run_setup(self, *extra, success=True, use_env=False):
        env = {**os.environ, "HOME": str(self.root),
               "CLAUDE_CONFIG_DIR": str(self.claude), "CODEX_HOME": str(self.codex)}
        args = [sys.executable, str(SCRIPT)]
        if not use_env:
            args += ["--claude-dir", str(self.claude), "--codex-dir", str(self.codex),
                     "--skills-dir", str(self.skills)]
        result = subprocess.run(args + list(extra), cwd=self.root, env=env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def write_json(self, path, data):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data) + "\n")

    def test_fresh_install_and_repeat_leave_files_and_links_untouched(self):
        self.run_setup()
        for path, harness in ((self.settings, "claude-code"), (self.hooks, "codex")):
            expected = json.loads((REPO / "integrations" / harness / "hooks.json").read_text())
            self.assertEqual(json.loads(path.read_text()), expected)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        paths = [self.settings, self.hooks, self.skills / "bbs", self.claude / "skills" / "bbs"]
        before = [(p.read_bytes() if not p.is_symlink() else os.readlink(p),
                   p.lstat().st_mtime_ns) for p in paths]
        self.run_setup()
        after = [(p.read_bytes() if not p.is_symlink() else os.readlink(p),
                  p.lstat().st_mtime_ns) for p in paths]
        self.assertEqual(after, before)
        self.assertFalse(list(self.root.rglob("*.bak-bbs")))

    def test_preserves_settings_custom_handlers_order_permissions_and_backup(self):
        custom = {"type": "command", "command": "bbs hook claude-code", "timeout": 30}
        group = {"matcher": "", "hooks": [{"type": "command", "command": "notify"}, custom]}
        original = {"model": "custom-model", "permissions": {"allow": ["Read(*)"]},
                    "hooks": {"Stop": [group], "PreToolUse": [{"hooks": []}]}}
        self.write_json(self.settings, original)
        self.settings.chmod(0o640)
        original_bytes = self.settings.read_bytes()
        self.run_setup()
        installed = json.loads(self.settings.read_text())
        self.assertEqual(installed["model"], original["model"])
        self.assertEqual(installed["permissions"], original["permissions"])
        self.assertEqual(installed["hooks"]["Stop"], [group])
        self.assertEqual(installed["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"])
        self.assertEqual(self.settings.stat().st_mode & 0o777, 0o640)
        backup = self.settings.with_name("settings.json.bak-bbs")
        self.assertEqual(backup.read_bytes(), original_bytes)
        del installed["hooks"]["SessionStart"]
        self.write_json(self.settings, installed)
        self.run_setup()
        self.assertEqual(backup.read_bytes(), original_bytes)

    def test_removes_unmodified_handlers_from_retired_events(self):
        old = {"type": "command", "command": "bbs hook codex", "timeout": 10}
        custom = {"type": "command", "command": "bbs hook codex", "timeout": 30}
        other = {"type": "command", "command": "notify"}
        self.write_json(self.hooks, {"hooks": {
            "UserPromptSubmit": [{"hooks": [old]}],
            "Stop": [{"hooks": [other, old]}, {"hooks": [custom]}, {"hooks": [old]}],
            "PreToolUse": [{"hooks": []}],
        }})
        self.run_setup()
        hooks = json.loads(self.hooks.read_text())["hooks"]
        self.assertNotIn("UserPromptSubmit", hooks)
        self.assertEqual(hooks["Stop"], [{"hooks": [other]}, {"hooks": [custom]}])
        self.assertEqual(hooks["PreToolUse"], [{"hooks": []}])

    def test_config_symlinks_and_shared_skill_directory(self):
        target = self.root / "dotfiles" / "settings.json"
        self.write_json(target, {"model": "keep"})
        self.claude.mkdir()
        self.settings.symlink_to(target)
        self.skills.mkdir()
        (self.claude / "skills").symlink_to(self.skills, target_is_directory=True)
        (self.skills / "bbs").symlink_to(self.root / "old-checkout", target_is_directory=True)
        self.run_setup()
        self.assertTrue(self.settings.is_symlink())
        self.assertEqual(json.loads(target.read_text())["model"], "keep")
        self.assertTrue(target.with_name("settings.json.bak-bbs").exists())
        self.assertEqual((self.skills / "bbs").resolve(), SKILL)
        self.assertEqual((self.claude / "skills" / "bbs").resolve(), SKILL)

    def test_dry_run_and_environment_defaults(self):
        self.run_setup("--dry-run", use_env=True)
        self.assertEqual(list(self.root.iterdir()), [])
        self.run_setup(use_env=True)
        self.assertTrue(self.settings.exists())
        self.assertTrue(self.hooks.exists())
        self.assertEqual((self.root / ".agents" / "skills" / "bbs").resolve(), SKILL)

    def test_invalid_config_is_rejected_before_any_writes(self):
        for invalid in ("{", "null", '{"hooks": []}', '{"hooks": {"Stop": [{}]}}'):
            with self.subTest(invalid=invalid):
                self.codex.mkdir(exist_ok=True)
                self.hooks.write_text(invalid)
                self.run_setup(success=False)
                self.assertEqual(self.hooks.read_text(), invalid)
                self.assertFalse(self.settings.exists())
                self.assertFalse(self.skills.exists())

    def test_real_skill_directory_is_preserved_before_any_config_writes(self):
        existing = self.skills / "bbs"
        existing.mkdir(parents=True)
        (existing / "SKILL.md").write_text("local skill")
        result = self.run_setup(success=False)
        self.assertIn("move the existing skill aside", result.stderr)
        self.assertEqual((existing / "SKILL.md").read_text(), "local skill")
        self.assertFalse(self.settings.exists())
        self.assertFalse(self.hooks.exists())


if __name__ == "__main__":
    unittest.main()
