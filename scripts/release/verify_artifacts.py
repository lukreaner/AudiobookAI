#!/usr/bin/env python3
"""Enforce the complete signed desktop release artifact matrix."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import urllib.parse


class ArtifactError(RuntimeError):
    pass


def require_count(root: Path, suffix: str, count: int = 1) -> list[Path]:
    matches = sorted(path for path in root.rglob(f"*{suffix}") if path.is_file())
    if len(matches) != count:
        raise ArtifactError(f"expected {count} artifact(s) ending {suffix}, found {len(matches)}")
    return matches


def require_flat_release_asset(root: Path, suffix: str) -> Path:
    artifact = require_count(root, suffix)[0]
    if artifact.parent != root:
        raise ArtifactError(f"release asset must be at the candidate root: {artifact.name}")
    return artifact


def verified_github_asset_url(url: object, version: str, artifact: Path) -> tuple[str, str]:
    if not isinstance(url, str):
        raise ArtifactError(f"updater URL is missing for {artifact.name}")
    parsed = urllib.parse.urlsplit(url)
    if (
        parsed.scheme != "https"
        or parsed.netloc != "github.com"
        or parsed.query
        or parsed.fragment
    ):
        raise ArtifactError(f"updater URL is not a canonical GitHub HTTPS asset URL: {url}")
    raw_segments = parsed.path.split("/")
    if raw_segments[0] or len(raw_segments) != 7 or any(not part for part in raw_segments[1:]):
        raise ArtifactError(f"updater URL has an invalid release asset path: {url}")
    segments = [urllib.parse.unquote(part) for part in raw_segments[1:]]
    if any("/" in part or "\\" in part for part in segments):
        raise ArtifactError(f"updater URL contains an encoded path separator: {url}")
    if (
        segments[2:4] != ["releases", "download"]
        or segments[4] != f"v{version}"
        or segments[5] != artifact.name
    ):
        raise ArtifactError(
            f"updater URL does not bind {artifact.name} to release v{version}"
        )
    return segments[0], segments[1]


def verify_updater_manifest(
    root: Path, version: str, artifacts: dict[str, Path]
) -> None:
    manifest_path = root / "latest.json"
    document = json.loads(manifest_path.read_text(encoding="utf-8"))
    if document.get("version") != version:
        raise ArtifactError("updater manifest version does not match the application version")
    platforms = document.get("platforms")
    if not isinstance(platforms, dict) or set(platforms) != set(artifacts):
        raise ArtifactError("updater manifest platform matrix is incomplete or unexpected")

    repository: tuple[str, str] | None = None
    for platform, artifact in artifacts.items():
        entry = platforms.get(platform)
        if not isinstance(entry, dict) or set(entry) != {"signature", "url"}:
            raise ArtifactError(f"updater entry is invalid for {platform}")
        expected_signature = Path(f"{artifact}.sig").read_text(encoding="utf-8").strip()
        if entry.get("signature") != expected_signature:
            raise ArtifactError(
                f"updater signature does not match {artifact.name} for {platform}"
            )
        entry_repository = verified_github_asset_url(entry.get("url"), version, artifact)
        if repository is None:
            repository = entry_repository
        elif entry_repository != repository:
            raise ArtifactError("updater URLs do not use one GitHub repository")


def verify(root: Path, version: str) -> None:
    root = root.resolve()
    if not root.is_dir():
        raise ArtifactError(f"artifact directory is missing: {root}")
    require_flat_release_asset(root, ".exe")
    require_flat_release_asset(root, ".dmg")
    require_flat_release_asset(root, ".AppImage")
    require_flat_release_asset(root, ".deb")
    updater_suffixes = [".nsis.zip", ".app.tar.gz", ".AppImage.tar.gz"]
    updater_artifacts: dict[str, Path] = {}
    for suffix in updater_suffixes:
        artifact = require_flat_release_asset(root, suffix)
        signature = Path(f"{artifact}.sig")
        if signature.parent != root or not signature.is_file():
            raise ArtifactError(f"missing updater signature for {artifact.name}")
        if not signature.read_text(encoding="utf-8").strip():
            raise ArtifactError(f"empty updater signature for {artifact.name}")
        updater_artifacts[suffix] = artifact
    require_flat_release_asset(root, "-corresponding-source.tar.gz")
    sbom = require_flat_release_asset(root, ".cdx.json")
    document = json.loads(sbom.read_text(encoding="utf-8"))
    if document.get("bomFormat") != "CycloneDX" or document.get("specVersion") != "1.6":
        raise ArtifactError("release SBOM must be CycloneDX 1.6")
    if document.get("metadata", {}).get("component", {}).get("version") != version:
        raise ArtifactError("release SBOM version does not match the application version")
    for name in [
        "THIRD_PARTY_NOTICES.md",
        "sidecars.lock.json",
        "latest.json",
        "LICENSE",
    ]:
        if not (root / name).is_file():
            raise ArtifactError(f"required release material is missing: {name}")
    verify_updater_manifest(
        root,
        version,
        {
            "darwin-aarch64": updater_artifacts[".app.tar.gz"],
            "darwin-x86_64": updater_artifacts[".app.tar.gz"],
            "linux-x86_64": updater_artifacts[".AppImage.tar.gz"],
            "windows-x86_64": updater_artifacts[".nsis.zip"],
        },
    )
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
        and (
            path.suffix == ".whl"
            or path.name.endswith(".tar.gz")
            and "source" not in path.name
            and not any(path.name.endswith(suffix) for suffix in updater_suffixes)
        )
    ]
    if forbidden:
        raise ArtifactError(
            "non-desktop package artifacts are outside the release contract: "
            + ", ".join(path.name for path in forbidden)
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    try:
        verify(args.root, args.version)
    except (OSError, json.JSONDecodeError, ArtifactError) as error:
        print(f"release artifact gate failed: {error}", file=sys.stderr)
        return 2
    print("signed desktop artifact matrix is complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
