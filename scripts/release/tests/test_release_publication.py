import json
import os
from pathlib import Path
import tempfile
import unittest

from scripts.release.collect_snapshot_artifacts import (
    SnapshotCollectionError,
    collect,
)
from scripts.release.generate_checksums import digest as checksum_digest
from scripts.release.prepare_snapshot_config import snapshot_config
from scripts.release.verify_release_upload import (
    UploadVerificationError,
    verify_upload,
)
from scripts.release.verify_snapshot_artifacts import (
    INSTALLERS,
    SnapshotArtifactError,
    verify_snapshot,
)
from scripts.release.write_snapshot_metadata import metadata


VERSION = "0.1.0"
COMMIT = "a" * 40
RUN_URL = "https://github.com/example/AudiobookAI/actions/runs/123"


class ReleaseUploadVerificationTests(unittest.TestCase):
    def test_accepts_exact_asset_copy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            downloaded = root / "downloaded"
            candidate.mkdir()
            downloaded.mkdir()
            (candidate / "installer.exe").write_bytes(b"installer")
            (downloaded / "installer.exe").write_bytes(b"installer")
            verify_upload(candidate, downloaded)

    def test_rejects_missing_or_changed_asset(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            downloaded = root / "downloaded"
            candidate.mkdir()
            downloaded.mkdir()
            (candidate / "installer.exe").write_bytes(b"candidate")
            (downloaded / "installer.exe").write_bytes(b"changed")
            with self.assertRaises(UploadVerificationError):
                verify_upload(candidate, downloaded)
            (downloaded / "installer.exe").unlink()
            (downloaded / "unexpected.exe").write_bytes(b"candidate")
            with self.assertRaises(UploadVerificationError):
                verify_upload(candidate, downloaded)

    @unittest.skipIf(os.name == "nt", "symlink creation is not generally available on Windows CI")
    def test_rejects_symlinked_release_asset(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / "candidate"
            downloaded = root / "downloaded"
            candidate.mkdir()
            downloaded.mkdir()
            target = root / "target"
            target.write_bytes(b"artifact")
            (candidate / "installer.exe").symlink_to(target)
            (downloaded / "installer.exe").write_bytes(b"artifact")
            with self.assertRaises(UploadVerificationError):
                verify_upload(candidate, downloaded)


class SnapshotPackagingTests(unittest.TestCase):
    def test_snapshot_overlay_is_isolated_and_has_no_release_inputs(self) -> None:
        config = snapshot_config("universal-apple-darwin")
        self.assertEqual(config["productName"], "AudiobookAI Development")
        self.assertEqual(config["identifier"], "ai.audiobook.desktop.development")
        self.assertFalse(config["bundle"]["createUpdaterArtifacts"])
        self.assertEqual(config["bundle"]["resources"], {})
        self.assertEqual(config["plugins"]["updater"]["endpoints"], [])
        self.assertEqual(config["bundle"]["macOS"]["signingIdentity"], "-")
        self.assertNotIn("Developer ID", json.dumps(config))

        windows = snapshot_config("x86_64-pc-windows-msvc")
        self.assertNotIn("signingIdentity", json.dumps(windows))

    def test_collects_only_expected_development_installers(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "bundle"
            (source / "appimage").mkdir(parents=True)
            (source / "deb").mkdir(parents=True)
            (source / "appimage" / "source.AppImage").write_bytes(b"appimage")
            (source / "deb" / "source.deb").write_bytes(b"deb")
            output = root / "output"
            result = collect("x86_64-unknown-linux-gnu", source, output)
            self.assertEqual({path.name for path in result}, {
                "AudiobookAI-development-linux-x86_64.AppImage",
                "AudiobookAI-development-linux-x86_64.deb",
            })

    def test_collection_rejects_ambiguous_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "bundle"
            source.mkdir()
            (source / "one.exe").write_bytes(b"one")
            (source / "two.exe").write_bytes(b"two")
            with self.assertRaises(SnapshotCollectionError):
                collect("x86_64-pc-windows-msvc", source, root / "output")

    def test_complete_snapshot_and_checksum_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in INSTALLERS:
                (root / name).write_bytes(name.encode("utf-8"))
            (root / "LICENSE").write_text("GPL-3.0-only\n", encoding="utf-8")
            (root / "DEVELOPMENT-BUILD.txt").write_text(
                metadata(VERSION, COMMIT, RUN_URL), encoding="utf-8"
            )
            verify_snapshot(root, version=VERSION, commit=COMMIT)
            names = sorted(path.name for path in root.iterdir())
            (root / "SHA256SUMS").write_text(
                "".join(
                    f"{checksum_digest(root / name)}  {name}\n" for name in names
                ),
                encoding="utf-8",
            )
            verify_snapshot(root, version=VERSION, commit=COMMIT, with_checksums=True)

    def test_snapshot_gate_rejects_wrong_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in INSTALLERS:
                (root / name).write_bytes(b"artifact")
            (root / "LICENSE").write_text("license", encoding="utf-8")
            (root / "DEVELOPMENT-BUILD.txt").write_text(
                metadata(VERSION, COMMIT, RUN_URL), encoding="utf-8"
            )
            with self.assertRaises(SnapshotArtifactError):
                verify_snapshot(root, version=VERSION, commit="b" * 40)


if __name__ == "__main__":
    unittest.main()
