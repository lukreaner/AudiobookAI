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


def write_managed_snapshot(path: Path, target: str, build_id: str, payload: bytes) -> None:
    path.mkdir(parents=True)
    executable = path / "AudiobookAI"
    executable.write_bytes(payload)
    manifest = {
        "artifacts": [{"file": executable.name}],
        "buildId": build_id,
        "checksumFile": "SHA256SUMS",
        "immutableDirectory": f"builds/{target}/{build_id}",
        "productName": "AudiobookAI",
        "target": target,
    }
    manifest_path = path / "manifest.json"
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    (path / "SHA256SUMS").write_text(
        f"{local.sha256_file(executable)}  {executable.name}\n"
        f"{local.sha256_file(manifest_path)}  {manifest_path.name}\n",
        encoding="utf-8",
    )


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

    def test_linux_native_build_preserves_runtime_libraries(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tauri = root / "web" / "node_modules" / ".bin" / "tauri"
            tauri.parent.mkdir(parents=True)
            tauri.write_bytes(b"tauri")
            with (
                mock.patch.object(local, "REPOSITORY", root),
                mock.patch.object(local, "CARGO_TARGET_ROOT", root / "target"),
                mock.patch.object(local, "run_visible") as run,
            ):
                local.build_native("x86_64-unknown-linux-gnu", 1_700_000_000)

        _command, environment = run.call_args.args
        self.assertEqual(environment["NO_STRIP"], "1")

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

    def test_snapshot_pruning_keeps_only_the_active_target_build(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = "aarch64-apple-darwin"
            snapshot_parent = root / "artifacts" / "local-native" / "builds" / target
            active = snapshot_parent / "active-build"
            stale = snapshot_parent / "stale-build"
            write_managed_snapshot(active, target, active.name, b"current")
            write_managed_snapshot(stale, target, stale.name, b"stale")
            other_target = (
                root
                / "artifacts"
                / "local-native"
                / "builds"
                / "x86_64-pc-windows-msvc"
                / "windows-build"
            )
            write_managed_snapshot(
                other_target, "x86_64-pc-windows-msvc", other_target.name, b"windows"
            )

            local.prune_superseded_snapshots(snapshot_parent, target, active.name)

            self.assertTrue(active.is_dir())
            self.assertFalse(stale.exists())
            self.assertTrue(other_target.is_dir())

    def test_snapshot_pruning_rejects_unmanaged_entries_before_deleting(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = "aarch64-apple-darwin"
            snapshot_parent = Path(temporary) / "builds" / target
            active = snapshot_parent / "active-build"
            stale = snapshot_parent / "stale-build"
            write_managed_snapshot(active, target, active.name, b"current")
            write_managed_snapshot(stale, target, stale.name, b"stale")
            unmanaged = snapshot_parent / "unmanaged-notes"
            unmanaged.mkdir()
            (unmanaged / "README.txt").write_text("keep me", encoding="utf-8")

            with self.assertRaises(local.LocalPackageError):
                local.prune_superseded_snapshots(snapshot_parent, target, active.name)

            self.assertTrue(active.is_dir())
            self.assertTrue(stale.is_dir())
            self.assertTrue(unmanaged.is_dir())

    def test_snapshot_pruning_requires_a_valid_active_build(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = "aarch64-apple-darwin"
            snapshot_parent = Path(temporary) / "builds" / target
            stale = snapshot_parent / "stale-build"
            write_managed_snapshot(stale, target, stale.name, b"stale")

            with self.assertRaises(local.LocalPackageError):
                local.prune_superseded_snapshots(
                    snapshot_parent, target, "missing-active-build"
                )

            self.assertTrue(stale.is_dir())

    def test_snapshot_pruning_rejects_incomplete_checksum_coverage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            target = "aarch64-apple-darwin"
            snapshot_parent = Path(temporary) / "builds" / target
            active = snapshot_parent / "active-build"
            stale = snapshot_parent / "stale-build"
            write_managed_snapshot(active, target, active.name, b"current")
            write_managed_snapshot(stale, target, stale.name, b"stale")
            manifest = stale / "manifest.json"
            (stale / "SHA256SUMS").write_text(
                f"{local.sha256_file(manifest)}  {manifest.name}\n",
                encoding="utf-8",
            )

            with self.assertRaises(local.LocalPackageError):
                local.prune_superseded_snapshots(snapshot_parent, target, active.name)

            self.assertTrue(active.is_dir())
            self.assertTrue(stale.is_dir())

    def test_snapshot_pruning_happens_only_after_current_verification(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output_root = root / "artifacts" / "local-native"
            target = "aarch64-apple-darwin"
            stale = output_root / "builds" / target / "stale-build"
            write_managed_snapshot(stale, target, stale.name, b"stale")
            staging = root / "staging"
            write_managed_snapshot(staging, target, "new-build", b"current")

            with (
                mock.patch.object(local, "REPOSITORY", root),
                mock.patch.object(local, "OUTPUT_ROOT", output_root),
                mock.patch.object(
                    local,
                    "verify_checksums",
                    side_effect=[None, local.LocalPackageError("invalid current")],
                ),
                self.assertRaises(local.LocalPackageError),
            ):
                local.publish_snapshot(staging, target, "new-build")

            self.assertTrue(stale.is_dir())

    def test_publication_lock_rejects_a_concurrent_target_publisher(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output_root = root / "artifacts" / "local-native"
            target = "aarch64-apple-darwin"

            with (
                mock.patch.object(local, "REPOSITORY", root),
                mock.patch.object(local, "OUTPUT_ROOT", output_root),
                local.target_publication_lock(target),
            ):
                lock = output_root / ".publish-locks" / target
                self.assertTrue(lock.is_dir())
                with self.assertRaises(local.LocalPackageError):
                    with local.target_publication_lock(target):
                        self.fail("a concurrent publisher acquired the same target lock")

            self.assertFalse(lock.exists())


if __name__ == "__main__":
    unittest.main()
