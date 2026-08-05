from __future__ import annotations

import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts.release import sidecars


class FakeResponse(io.BytesIO):
    def __enter__(self) -> FakeResponse:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()


class RecordingOpener:
    def __init__(self) -> None:
        self.request = None

    def open(self, request, timeout: int):  # type: ignore[no-untyped-def]
        self.request = request
        self.timeout = timeout
        return FakeResponse(b"public archive")


class PublicSidecarTests(unittest.TestCase):
    def test_download_sends_no_authorization_header(self) -> None:
        opener = RecordingOpener()
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "archive"
            with mock.patch.object(
                sidecars.urllib.request, "build_opener", return_value=opener
            ):
                sidecars.fetch_archive(
                    "https://downloads.example.test/archive.zip", destination
                )

            self.assertEqual(destination.read_bytes(), b"public archive")
            self.assertIsNotNone(opener.request)
            header_names = {
                name.lower() for name, _value in opener.request.header_items()
            }
            self.assertNotIn("authorization", header_names)

    def test_manifest_url_rejects_credential_like_query(self) -> None:
        with self.assertRaises(sidecars.SidecarError):
            sidecars.validate_https_url(
                "https://downloads.example.test/archive.zip?temporary=value",
                "archive",
                [],
            )

    @unittest.skipIf(os.name == "nt", "creating symlinks is not generally available on Windows CI")
    def test_tree_verification_rejects_required_file_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "sidecars"
            external = Path(temporary) / "external-uv"
            (root / "bin").mkdir(parents=True)
            external.write_bytes(b"not bundled")
            (root / "bin" / "uv").symlink_to(external)
            manifest = {
                "bundles": {
                    "universal-apple-darwin": {
                        "requiredFiles": [
                            {
                                "path": "bin/uv",
                                "sha256": sidecars.sha256_file(external),
                                "executable": True,
                            }
                        ]
                    }
                }
            }

            with self.assertRaisesRegex(sidecars.SidecarError, "required sidecar file"):
                sidecars.verify_tree(manifest, "universal-apple-darwin", root)

    @unittest.skipIf(os.name == "nt", "creating symlinks is not generally available on Windows CI")
    def test_tree_verification_rejects_symlinked_parent_inside_root(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "sidecars"
            actual_bin = root / "actual-bin"
            actual_bin.mkdir(parents=True)
            executable = actual_bin / "ffmpeg"
            executable.write_bytes(b"bundled executable")
            (root / "bin").symlink_to(actual_bin, target_is_directory=True)
            manifest = {
                "bundles": {
                    "x86_64-unknown-linux-gnu": {
                        "requiredFiles": [
                            {
                                "path": "bin/ffmpeg",
                                "sha256": sidecars.sha256_file(executable),
                                "executable": True,
                            }
                        ]
                    }
                }
            }

            with self.assertRaisesRegex(sidecars.SidecarError, "required sidecar file"):
                sidecars.verify_tree(manifest, "x86_64-unknown-linux-gnu", root)

    def test_unresolved_mlx_transitive_installer_lock_is_a_release_gate(self) -> None:
        source = Path("packaging/sidecars.lock.json")
        manifest = json.loads(source.read_text(encoding="utf-8"))
        unresolved: list[str] = []
        sidecars.validate_source(manifest, unresolved)
        self.assertIn("MLX-audio managed Python version", unresolved)
        self.assertIn("MLX-audio artifact lock", unresolved)
        self.assertIn("MLX-audio artifact count", unresolved)
        self.assertIn("MLX-audio complete transitive artifact closure", unresolved)

        manifest["releaseReady"] = True
        with tempfile.TemporaryDirectory() as temporary:
            candidate = Path(temporary) / "sidecars.json"
            candidate.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(
                sidecars.SidecarError, "MLX-audio managed Python version"
            ):
                sidecars.load_manifest(candidate, allow_unresolved=False)

    def test_complete_mlx_artifact_lock_requires_hashed_public_artifacts(self) -> None:
        lock = {
            "schemaVersion": 1,
            "package": "mlx-audio[tts,server]",
            "version": "0.4.6",
            "target": "aarch64-apple-darwin",
            "pythonVersion": "3.12.12",
            "completeTransitiveClosure": True,
            "artifacts": [
                {
                    "package": "mlx-audio",
                    "version": "0.4.6",
                    "filename": "mlx_audio-0.4.6-py3-none-any.whl",
                    "url": "https://files.example.test/mlx_audio-0.4.6-py3-none-any.whl",
                    "sha256": "0" * 64,
                }
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "lock.json"
            path.write_text(json.dumps(lock), encoding="utf-8")
            sidecars.validate_mlx_artifact_lock(
                path, python_version="3.12.12", expected_count=1
            )
            lock["artifacts"][0]["url"] += "?temporary=value"
            path.write_text(json.dumps(lock), encoding="utf-8")
            with self.assertRaises(sidecars.SidecarError):
                sidecars.validate_mlx_artifact_lock(
                    path, python_version="3.12.12", expected_count=1
                )


if __name__ == "__main__":
    unittest.main()
