import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


LAUNCHER = Path(__file__).resolve().parents[1] / "start_audiobookai.py"
SPEC = importlib.util.spec_from_file_location("start_audiobookai", LAUNCHER)
launcher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(launcher)

TARGET = "x86_64-unknown-linux-gnu"
EXECUTABLE = "AudiobookAI.exe" if sys.platform == "win32" else "AudiobookAI"


class CurrentBuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.current = self.root / "artifacts" / "local-native" / "current" / TARGET
        self.current.mkdir(parents=True)
        patcher = mock.patch.object(launcher, "REPOSITORY", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(self.temporary.cleanup)

    def publish(self, digest):
        (self.current / "manifest.json").write_text(json.dumps({"source": {"digest": digest}}))
        (self.current / EXECUTABLE).write_text("binary")

    def test_matching_source_reuses_the_published_build(self):
        self.publish("abc")
        self.assertEqual(launcher.current_build(TARGET, "abc"), self.current / EXECUTABLE)

    def test_changed_source_requires_a_rebuild(self):
        self.publish("abc")
        self.assertIsNone(launcher.current_build(TARGET, "def"))

    def test_missing_or_corrupt_build_requires_a_rebuild(self):
        self.assertIsNone(launcher.current_build(TARGET, "abc"))
        (self.current / "manifest.json").write_text("{not json")
        self.assertIsNone(launcher.current_build(TARGET, "abc"))


class BuildModuleTests(unittest.TestCase):
    def test_the_real_build_script_loads(self):
        build = launcher.load_build_module()
        self.assertTrue(callable(build.git_source_state))
        self.assertTrue(callable(build.main))


class PrerequisiteTests(unittest.TestCase):
    def test_missing_tools_are_reported_with_install_hints(self):
        with mock.patch.object(launcher.shutil, "which", return_value=None):
            with self.assertRaises(launcher.LaunchError) as raised:
                launcher.check_prerequisites()
        self.assertIn("pnpm", str(raised.exception))
        self.assertIn("https://rustup.rs", str(raised.exception))

    def test_failures_return_an_error_code_without_waiting_when_not_interactive(self):
        with mock.patch.object(launcher, "run", side_effect=launcher.LaunchError("broken")), \
                mock.patch.object(launcher.sys, "stdin", None):
            self.assertEqual(launcher.main(), 1)


if __name__ == "__main__":
    unittest.main()
