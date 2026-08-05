#!/usr/bin/env python3
"""Collect and clearly rename untrusted installers from one snapshot target."""

from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import stat
import sys


EXPECTED = {
    "x86_64-pc-windows-msvc": [
        (".exe", "AudiobookAI-development-windows-x86_64.exe"),
    ],
    "universal-apple-darwin": [
        (".dmg", "AudiobookAI-development-macos-universal.dmg"),
    ],
    "x86_64-unknown-linux-gnu": [
        (".AppImage", "AudiobookAI-development-linux-x86_64.AppImage"),
        (".deb", "AudiobookAI-development-linux-x86_64.deb"),
    ],
}


class SnapshotCollectionError(RuntimeError):
    pass


def collect(target: str, source: Path, output: Path) -> list[Path]:
    if target not in EXPECTED:
        raise SnapshotCollectionError(f"unsupported snapshot target {target}")
    try:
        source_metadata = source.lstat()
    except OSError as error:
        raise SnapshotCollectionError(f"Tauri bundle directory is missing: {source}") from error
    if not stat.S_ISDIR(source_metadata.st_mode) or stat.S_ISLNK(source_metadata.st_mode):
        raise SnapshotCollectionError("Tauri bundle root must be a non-symlink directory")

    selected: list[tuple[Path, str]] = []
    for suffix, destination_name in EXPECTED[target]:
        matches = []
        for path in sorted(source.rglob(f"*{suffix}")):
            metadata = path.lstat()
            if stat.S_ISREG(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode):
                matches.append(path)
        if len(matches) != 1:
            raise SnapshotCollectionError(
                f"{target} expected one *{suffix} artifact, found {len(matches)}"
            )
        selected.append((matches[0], destination_name))

    output.mkdir(parents=True, exist_ok=False)
    destinations: list[Path] = []
    for artifact, destination_name in selected:
        destination = output / destination_name
        shutil.copy2(artifact, destination)
        destinations.append(destination)
    return destinations


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=sorted(EXPECTED))
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        selected = collect(args.target, args.source, args.output)
    except (OSError, SnapshotCollectionError) as error:
        print(f"development snapshot collection failed: {error}", file=sys.stderr)
        return 2
    print(f"collected {len(selected)} untrusted development artifact(s) for {args.target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
