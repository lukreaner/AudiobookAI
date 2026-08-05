import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from scripts.release.prepare_tauri_config import (
    ConfigError,
    main,
    resource_destination,
    verified_resource_files,
)


class ResourceDestinationTests(unittest.TestCase):
    def test_preserves_verified_binary_directory(self) -> None:
        self.assertEqual(
            resource_destination(Path("bin") / "ffmpeg"),
            "sidecars/bin/ffmpeg",
        )

    def test_preserves_linux_voice_data_directory(self) -> None:
        self.assertEqual(
            resource_destination(
                Path("share") / "espeak-ng-data" / "voices" / "en"
            ),
            "sidecars/share/espeak-ng-data/voices/en",
        )

    @unittest.skipIf(os.name == "nt", "creating symlinks is not generally available on Windows CI")
    def test_resource_enumeration_rejects_symlinked_components(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "verified"
            actual = root / "actual-bin"
            actual.mkdir(parents=True)
            (actual / "ffmpeg").write_bytes(b"binary")
            (root / "bin").symlink_to(actual, target_is_directory=True)

            with self.assertRaisesRegex(ConfigError, "symbolic link"):
                verified_resource_files(root)

    def test_generated_overlay_keeps_binary_and_voice_data_siblings(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sidecars = root / "verified"
            (sidecars / "bin").mkdir(parents=True)
            (sidecars / "share" / "espeak-ng-data" / "voices").mkdir(parents=True)
            (sidecars / "bin" / "ffmpeg").write_bytes(b"ffmpeg")
            (sidecars / "bin" / "ffprobe").write_bytes(b"ffprobe")
            (sidecars / "bin" / "espeak-ng").write_bytes(b"espeak-ng")
            (sidecars / "share" / "espeak-ng-data" / "voices" / "en").write_bytes(
                b"voice"
            )
            output = root / "tauri.release.generated.json"
            arguments = [
                "prepare_tauri_config.py",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--sidecars",
                str(sidecars),
                "--output",
                str(output),
            ]
            environment = {
                "TAURI_SIGNING_PUBLIC_KEY": "test-public-key",
                "AUDIOBOOKAI_UPDATER_ENDPOINT": "https://updates.example.test/latest.json",
            }
            with (
                mock.patch.object(sys, "argv", arguments),
                mock.patch.dict(os.environ, environment, clear=False),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(main(), 0)

            config = json.loads(output.read_text(encoding="utf-8"))
            destinations = set(config["bundle"]["resources"].values())
            self.assertIn("sidecars/bin/ffmpeg", destinations)
            self.assertIn("sidecars/bin/ffprobe", destinations)
            self.assertIn("sidecars/bin/espeak-ng", destinations)
            self.assertIn(
                "sidecars/share/espeak-ng-data/voices/en",
                destinations,
            )
            self.assertNotIn("sidecars/ffmpeg", destinations)


if __name__ == "__main__":
    unittest.main()
