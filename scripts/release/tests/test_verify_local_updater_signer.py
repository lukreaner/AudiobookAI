import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts.release.verify_local_updater_signer import SignerError, verify


class LocalUpdaterSignerTests(unittest.TestCase):
    def test_accepts_owner_only_absolute_key_file_outside_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "workspace"
            workspace.mkdir()
            key = root / "local-updater-signer"
            key.write_text("dummy-private-material", encoding="utf-8")
            key.chmod(0o600)
            environment = {
                "TAURI_SIGNING_PRIVATE_KEY": str(key),
                "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "dummy-password",
            }
            with mock.patch.dict(os.environ, environment, clear=False):
                verify(workspace)

    def test_rejects_raw_key_material_in_environment(self) -> None:
        environment = {
            "TAURI_SIGNING_PRIVATE_KEY": "dummy-line-one\ndummy-line-two",
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "dummy-password",
        }
        with mock.patch.dict(os.environ, environment, clear=False):
            with self.assertRaises(SignerError):
                verify(Path.cwd())

    def test_rejects_key_file_inside_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            key = workspace / "local-updater-signer"
            key.write_text("dummy-private-material", encoding="utf-8")
            key.chmod(0o600)
            environment = {
                "TAURI_SIGNING_PRIVATE_KEY": str(key),
                "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "dummy-password",
            }
            with mock.patch.dict(os.environ, environment, clear=False):
                with self.assertRaises(SignerError):
                    verify(workspace)


if __name__ == "__main__":
    unittest.main()
