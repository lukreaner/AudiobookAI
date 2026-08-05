from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest

from scripts.release.verify_artifacts import ArtifactError, verify


VERSION = "1.2.3"


class ArtifactGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        for name in [
            "AudiobookAI.exe",
            "AudiobookAI.dmg",
            "AudiobookAI.AppImage",
            "AudiobookAI.deb",
            "AudiobookAI-corresponding-source.tar.gz",
        ]:
            (self.root / name).write_bytes(b"release asset")
        updater_names = {
            "darwin-aarch64": "AudiobookAI.app.tar.gz",
            "darwin-x86_64": "AudiobookAI.app.tar.gz",
            "linux-x86_64": "AudiobookAI.AppImage.tar.gz",
            "windows-x86_64": "AudiobookAI.nsis.zip",
        }
        signatures: dict[str, str] = {}
        for name in set(updater_names.values()):
            (self.root / name).write_bytes(b"signed updater")
            signature = f"signature-for-{name}"
            (self.root / f"{name}.sig").write_text(signature + "\n", encoding="utf-8")
            signatures[name] = signature
        (self.root / f"AudiobookAI-{VERSION}.cdx.json").write_text(
            json.dumps(
                {
                    "bomFormat": "CycloneDX",
                    "specVersion": "1.6",
                    "metadata": {"component": {"version": VERSION}},
                }
            ),
            encoding="utf-8",
        )
        for name in ["THIRD_PARTY_NOTICES.md", "sidecars.lock.json", "LICENSE"]:
            (self.root / name).write_text("release material\n", encoding="utf-8")
        for target in [
            "universal-apple-darwin",
            "x86_64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
        ]:
            (self.root / f"{target}-sidecars.txt").write_text("evidence\n", encoding="utf-8")
            (self.root / f"{target}-provenance.json").write_text("{}\n", encoding="utf-8")
        platforms = {}
        for platform, name in updater_names.items():
            platforms[platform] = {
                "signature": signatures[name],
                "url": (
                    "https://github.com/example/AudiobookAI/releases/download/"
                    f"v{VERSION}/{name}"
                ),
            }
        (self.root / "latest.json").write_text(
            json.dumps({"version": VERSION, "platforms": platforms}),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def update_manifest(self, update) -> None:
        path = self.root / "latest.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        update(document)
        path.write_text(json.dumps(document), encoding="utf-8")

    def test_accepts_a_manifest_bound_to_the_exact_signed_artifact_matrix(self) -> None:
        verify(self.root, VERSION)

    def test_rejects_a_manifest_signature_that_does_not_match_the_sidecar(self) -> None:
        self.update_manifest(
            lambda document: document["platforms"]["windows-x86_64"].update(
                {"signature": "substituted-signature"}
            )
        )

        with self.assertRaisesRegex(ArtifactError, "signature does not match"):
            verify(self.root, VERSION)

    def test_rejects_an_updater_url_for_another_release(self) -> None:
        self.update_manifest(
            lambda document: document["platforms"]["linux-x86_64"].update(
                {
                    "url": (
                        "https://github.com/example/AudiobookAI/releases/download/"
                        "v9.9.9/AudiobookAI.AppImage.tar.gz"
                    )
                }
            )
        )

        with self.assertRaisesRegex(ArtifactError, "does not bind"):
            verify(self.root, VERSION)


if __name__ == "__main__":
    unittest.main()
