#!/usr/bin/env python3
"""Create deterministic Tauri stable-channel metadata from signed updater archives."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import sys
import urllib.parse


class UpdaterError(RuntimeError):
    pass


def exactly_one(root: Path, suffix: str) -> Path:
    matches = sorted(path for path in root.rglob(f"*{suffix}") if path.is_file())
    if len(matches) != 1:
        raise UpdaterError(f"expected exactly one *{suffix} updater artifact, found {len(matches)}")
    return matches[0]


def entry(root: Path, artifact: Path, repository: str, tag: str) -> dict:
    signature_path = Path(f"{artifact}.sig")
    if not signature_path.is_file():
        raise UpdaterError(f"missing Tauri signature for {artifact.name}")
    signature = signature_path.read_text(encoding="utf-8").strip()
    if not signature:
        raise UpdaterError(f"empty Tauri signature for {artifact.name}")
    relative = artifact.relative_to(root).as_posix()
    if "/" in relative:
        # GitHub release assets are flattened before publication.
        file_name = artifact.name
    else:
        file_name = relative
    url = "https://github.com/{}/releases/download/{}/{}".format(
        repository,
        urllib.parse.quote(tag, safe=""),
        urllib.parse.quote(file_name, safe=""),
    )
    return {"signature": signature, "url": url}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        if not args.tag.startswith("v"):
            raise UpdaterError("release tag must begin with v")
        epoch_value = os.environ.get("SOURCE_DATE_EPOCH")
        if not epoch_value or not epoch_value.isdigit():
            raise UpdaterError("SOURCE_DATE_EPOCH must be the release commit timestamp")
        published_at = datetime.fromtimestamp(int(epoch_value), timezone.utc).isoformat().replace(
            "+00:00", "Z"
        )
        root = args.root.resolve()
        windows = exactly_one(root, ".nsis.zip")
        macos = exactly_one(root, ".app.tar.gz")
        linux = exactly_one(root, ".AppImage.tar.gz")
        macos_entry = entry(root, macos, args.repository, args.tag)
        document = {
            "version": args.tag.removeprefix("v"),
            "notes": "See the signed release notes distributed with this stable release.",
            "pub_date": published_at,
            "platforms": {
                "darwin-aarch64": macos_entry,
                "darwin-x86_64": macos_entry,
                "linux-x86_64": entry(root, linux, args.repository, args.tag),
                "windows-x86_64": entry(root, windows, args.repository, args.tag),
            },
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (OSError, ValueError, UpdaterError) as error:
        print(f"updater metadata generation failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote signed updater metadata for {args.tag}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
