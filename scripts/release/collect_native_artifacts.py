#!/usr/bin/env python3
"""Collect only the expected Tauri bundles for one native target."""

from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import sys


EXPECTED = {
    "x86_64-pc-windows-msvc": [".exe", ".nsis.zip", ".nsis.zip.sig"],
    "universal-apple-darwin": [".dmg", ".app.tar.gz", ".app.tar.gz.sig"],
    "x86_64-unknown-linux-gnu": [
        ".AppImage",
        ".deb",
        ".AppImage.tar.gz",
        ".AppImage.tar.gz.sig",
    ],
}


class CollectError(RuntimeError):
    pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=sorted(EXPECTED))
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        if not args.source.is_dir():
            raise CollectError(f"Tauri bundle directory is missing: {args.source}")
        selected: list[Path] = []
        for suffix in EXPECTED[args.target]:
            matches = sorted(
                path for path in args.source.rglob(f"*{suffix}") if path.is_file()
            )
            if len(matches) != 1:
                raise CollectError(
                    f"{args.target} expected one *{suffix} artifact, found {len(matches)}"
                )
            selected.append(matches[0])
        args.output.mkdir(parents=True, exist_ok=False)
        names: set[str] = set()
        for source in selected:
            if source.name in names:
                raise CollectError(f"duplicate flattened artifact name: {source.name}")
            names.add(source.name)
            shutil.copy2(source, args.output / source.name)
    except (OSError, CollectError) as error:
        print(f"native artifact collection failed: {error}", file=sys.stderr)
        return 2
    print(f"collected {len(selected)} signed artifacts for {args.target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
