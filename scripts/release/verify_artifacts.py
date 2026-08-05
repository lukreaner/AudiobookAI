#!/usr/bin/env python3
"""Enforce the complete signed desktop release artifact matrix."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


class ArtifactError(RuntimeError):
    pass


def require_count(root: Path, suffix: str, count: int = 1) -> list[Path]:
    matches = sorted(path for path in root.rglob(f"*{suffix}") if path.is_file())
    if len(matches) != count:
        raise ArtifactError(f"expected {count} artifact(s) ending {suffix}, found {len(matches)}")
    return matches


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    try:
        root = args.root.resolve()
        if not root.is_dir():
            raise ArtifactError(f"artifact directory is missing: {root}")
        require_count(root, ".exe")
        require_count(root, ".dmg")
        require_count(root, ".AppImage")
        require_count(root, ".deb")
        updater_suffixes = [".nsis.zip", ".app.tar.gz", ".AppImage.tar.gz"]
        for suffix in updater_suffixes:
            artifact = require_count(root, suffix)[0]
            signature = Path(f"{artifact}.sig")
            if not signature.is_file() or not signature.read_text(encoding="utf-8").strip():
                raise ArtifactError(f"missing non-empty updater signature for {artifact.name}")
        require_count(root, "-corresponding-source.tar.gz")
        sbom = require_count(root, ".cdx.json")[0]
        document = json.loads(sbom.read_text(encoding="utf-8"))
        if document.get("bomFormat") != "CycloneDX" or document.get("specVersion") != "1.6":
            raise ArtifactError("release SBOM must be CycloneDX 1.6")
        if document.get("metadata", {}).get("component", {}).get("version") != args.version:
            raise ArtifactError("release SBOM version does not match the application version")
        for name in [
            "THIRD_PARTY_NOTICES.md",
            "sidecars.lock.json",
            "latest.json",
            "LICENSE",
        ]:
            if not (root / name).is_file():
                raise ArtifactError(f"required release material is missing: {name}")
        for target in [
            "universal-apple-darwin",
            "x86_64-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
        ]:
            for suffix in ["-sidecars.txt", "-provenance.json"]:
                if not (root / f"{target}{suffix}").is_file():
                    raise ArtifactError(
                        f"sidecar release evidence is missing: {target}{suffix}"
                    )
        forbidden = [
            path
            for path in root.rglob("*")
            if path.is_file()
            and (path.suffix == ".whl" or path.name.endswith(".tar.gz") and "source" not in path.name and not any(path.name.endswith(suffix) for suffix in updater_suffixes))
        ]
        if forbidden:
            raise ArtifactError(
                "non-desktop package artifacts are outside the release contract: "
                + ", ".join(path.name for path in forbidden)
            )
    except (OSError, json.JSONDecodeError, ArtifactError) as error:
        print(f"release artifact gate failed: {error}", file=sys.stderr)
        return 2
    print("signed desktop artifact matrix is complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
