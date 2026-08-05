from __future__ import annotations

import json
from pathlib import Path
import shutil
import tempfile
import unittest

from scripts.release.check_versions import VersionError, verify_repository


REPOSITORY = Path(__file__).resolve().parents[3]
FILES = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "pnpm-lock.yaml",
    "web/package.json",
    "apps/desktop/src-tauri/Cargo.toml",
    "apps/desktop/src-tauri/tauri.conf.json",
    "crates/core/Cargo.toml",
    "crates/epub/Cargo.toml",
    "crates/media/Cargo.toml",
    "crates/providers/Cargo.toml",
    "crates/service/Cargo.toml",
    "crates/storage/Cargo.toml",
    "packaging/sidecars.lock.json",
]


class VersionGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        for relative in FILES:
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPOSITORY / relative, destination)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_current_repository_metadata_is_consistent(self) -> None:
        self.assertEqual(verify_repository(self.root), "0.1.0")

    def test_rejects_stale_workspace_version_in_cargo_lock(self) -> None:
        lock = (self.root / "Cargo.lock").read_text(encoding="utf-8")
        lock = lock.replace(
            'name = "audiobookai-desktop"\nversion = "0.1.0"',
            'name = "audiobookai-desktop"\nversion = "0.0.9"',
            1,
        )
        (self.root / "Cargo.lock").write_text(lock, encoding="utf-8")

        with self.assertRaisesRegex(VersionError, "Cargo.lock workspace package"):
            verify_repository(self.root)

    def test_rejects_workspace_member_with_an_independent_version(self) -> None:
        manifest = self.root / "crates/core/Cargo.toml"
        text = manifest.read_text(encoding="utf-8").replace(
            "version.workspace = true", 'version = "0.1.0"', 1
        )
        manifest.write_text(text, encoding="utf-8")

        with self.assertRaisesRegex(VersionError, "must inherit the release version"):
            verify_repository(self.root)

    def test_rejects_desktop_identity_drift_that_would_break_update_continuity(self) -> None:
        config_path = self.root / "apps/desktop/src-tauri/tauri.conf.json"
        config = json.loads(config_path.read_text(encoding="utf-8"))
        config["identifier"] = "example.changed.application"
        config_path.write_text(json.dumps(config), encoding="utf-8")

        with self.assertRaisesRegex(VersionError, "update continuity"):
            verify_repository(self.root)

    def test_rejects_a_base_config_that_enables_unverified_updates(self) -> None:
        config_path = self.root / "apps/desktop/src-tauri/tauri.conf.json"
        config = json.loads(config_path.read_text(encoding="utf-8"))
        config["plugins"]["updater"]["endpoints"] = [
            "https://updates.example.test/latest.json"
        ]
        config_path.write_text(json.dumps(config), encoding="utf-8")

        with self.assertRaisesRegex(VersionError, "must remain disabled"):
            verify_repository(self.root)

    def test_release_mode_rejects_the_current_pre_one_point_zero_version(self) -> None:
        with self.assertRaisesRegex(VersionError, "1.0.0 or newer"):
            verify_repository(self.root, tag="v0.1.0", release=True)


if __name__ == "__main__":
    unittest.main()
