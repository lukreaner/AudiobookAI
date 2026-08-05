from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import stat
import tarfile
import tempfile
import unittest
from unittest import mock

from scripts.packaging import build_local_native as local


class LocalNativePackagingTests(unittest.TestCase):
    def test_frontend_install_is_frozen(self) -> None:
        with mock.patch.object(local, "run_visible") as run:
            local.install_frontend_dependencies(1_700_000_000)

        command, environment = run.call_args.args
        self.assertEqual(command, [local.PNPM, "install", "--frozen-lockfile"])
        self.assertEqual(environment["SOURCE_DATE_EPOCH"], "1700000000")

    def test_native_build_is_optimized_debug_with_system_tool_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tauri = root / "web" / "node_modules" / ".bin" / "tauri"
            tauri.parent.mkdir(parents=True)
            tauri.write_bytes(b"tauri")
            target_root = root / "target" / "local-native"
            with (
                mock.patch.object(local, "REPOSITORY", root),
                mock.patch.object(local, "CARGO_TARGET_ROOT", target_root),
                mock.patch.object(local, "run_visible") as run,
            ):
                build_root = local.build_native(
                    "aarch64-apple-darwin", 1_700_000_000
                )

        command, environment = run.call_args.args
        self.assertIn("--debug", command)
        self.assertIn("--no-sign", command)
        self.assertEqual(environment["CARGO_PROFILE_DEV_DEBUG"], "0")
        self.assertEqual(environment["CARGO_PROFILE_DEV_OPT_LEVEL"], "1")
        self.assertEqual(environment["CARGO_PROFILE_DEV_STRIP"], "symbols")
        self.assertEqual(
            build_root,
            target_root / "aarch64-apple-darwin" / "debug",
        )

    def test_macos_archive_is_deterministic_and_keeps_the_executable_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / "AudiobookAI.app"
            executable = app / "Contents" / "MacOS" / "AudiobookAI"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"native executable")
            executable.chmod(0o755)
            (app / "Contents" / "Info.plist").write_text("plist", encoding="utf-8")
            first = root / "first.tar.gz"
            second = root / "second.tar.gz"

            local.archive_macos_app(app, first, 1_700_000_000)
            os.utime(executable, (1_800_000_000, 1_800_000_000))
            local.archive_macos_app(app, second, 1_700_000_000)

            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first, "r:gz") as archive:
                entry = archive.getmember("AudiobookAI.app/Contents/MacOS/AudiobookAI")
                self.assertEqual(entry.mtime, 1_700_000_000)
                self.assertEqual(stat.S_IMODE(entry.mode), 0o755)
                self.assertEqual(entry.uid, 0)
                self.assertEqual(entry.gid, 0)

    def test_metadata_is_sorted_and_every_declared_file_is_checksummed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            staging = Path(temporary)
            (staging / "AudiobookAI").write_bytes(b"executable")
            (staging / "AudiobookAI.app.tar.gz").write_bytes(b"bundle")
            artifacts = local.artifact_records(
                staging,
                [
                    ("AudiobookAI.app.tar.gz", "macos-app-archive"),
                    ("AudiobookAI", "executable"),
                ],
            )
            source = local.SourceState(
                commit="a" * 40,
                dirty=True,
                digest="b" * 64,
                source_date_epoch=1_700_000_000,
            )

            local.write_metadata(
                staging,
                target="aarch64-apple-darwin",
                version="0.1.0",
                build_id="test-build",
                source=source,
                tools={"rustc": "rustc test"},
                artifacts=artifacts,
            )

            manifest = json.loads((staging / "manifest.json").read_text(encoding="utf-8"))
            self.assertFalse(manifest["publishableRelease"])
            self.assertEqual(manifest["profile"], "optimized-debug")
            self.assertTrue(
                manifest["developmentRuntime"]["systemFfmpegOnPathAllowed"]
            )
            self.assertEqual(manifest["signing"]["tauriCodeSigning"], "disabled")
            self.assertFalse(manifest["signing"]["releaseIdentityConfigured"])
            self.assertTrue(manifest["signing"]["platformAdHocSignaturePossible"])
            self.assertEqual(
                [item["file"] for item in manifest["artifacts"]],
                ["AudiobookAI", "AudiobookAI.app.tar.gz"],
            )
            checksum_lines = (staging / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
            self.assertEqual(
                [line.split("  ", 1)[1] for line in checksum_lines],
                ["AudiobookAI", "AudiobookAI.app.tar.gz", "manifest.json"],
            )
            for line in checksum_lines:
                expected, name = line.split("  ", 1)
                self.assertEqual(expected, hashlib.sha256((staging / name).read_bytes()).hexdigest())

    def test_current_publication_replaces_only_managed_names(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            snapshot = root / "snapshot"
            current = root / "artifacts" / "local-native" / "current" / "target"
            snapshot.mkdir()
            (snapshot / "AudiobookAI").write_bytes(b"new executable")
            (snapshot / "manifest.json").write_bytes(b"new manifest")
            (snapshot / "SHA256SUMS").write_bytes(b"new checksums")
            current.mkdir(parents=True)
            (current / "AudiobookAI").write_bytes(b"old executable")
            (current / "keep.txt").write_bytes(b"unmanaged")

            with mock.patch.object(local, "REPOSITORY", root):
                local.publish_current(snapshot, current, "new-build")

            self.assertEqual((current / "AudiobookAI").read_bytes(), b"new executable")
            self.assertEqual((current / "keep.txt").read_bytes(), b"unmanaged")
            self.assertEqual((current / "CURRENT").read_text(encoding="utf-8"), "new-build\n")


if __name__ == "__main__":
    unittest.main()
