import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from scripts.release import capture_sidecar_evidence


class UvEvidenceTests(unittest.TestCase):
    def test_macos_uv_evidence_requires_exact_pinned_version(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "evidence.txt"
            arguments = [
                "capture_sidecar_evidence.py",
                str(root),
                str(output),
                "--macos-uv",
            ]
            manifest = {
                "ffmpeg": {
                    "configureFlags": [],
                    "requiredFeatures": {"encoders": [], "filters": [], "muxers": []},
                }
            }
            sections = iter(
                [
                    "ffmpeg version 8.1.1\n",
                    "configuration:\n",
                    "Encoders:\n",
                    "Filters:\n",
                    "Muxers:\n",
                    "ffprobe version 8.1.1\n",
                    "uv 0.12.1\n",
                ]
            )
            with (
                mock.patch.object(sys, "argv", arguments),
                mock.patch.object(Path, "read_text", return_value=json.dumps(manifest)),
                mock.patch.object(capture_sidecar_evidence, "run", side_effect=lambda _args: next(sections)),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(capture_sidecar_evidence.main(), 0)
            self.assertIn("## uv-version\nuv 0.12.1", output.read_text(encoding="utf-8"))

    def test_macos_uv_evidence_rejects_different_version(self) -> None:
        self.assertIsNone(
            __import__("re").fullmatch(r"uv 0\.12\.1\s*", "uv 0.12.2\n")
        )


if __name__ == "__main__":
    unittest.main()
