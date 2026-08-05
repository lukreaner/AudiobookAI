#!/usr/bin/env python3
"""Generate deterministic SHA256SUMS without following links."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import sys


EXCLUDED = {"SHA256SUMS", "SHA256SUMS.asc", "RELEASE_KEY.asc"}


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    output = (args.output or root / "SHA256SUMS").resolve()
    try:
        if not root.is_dir():
            raise RuntimeError(f"artifact root is missing: {root}")
        files = []
        for path in root.rglob("*"):
            if path.is_symlink():
                raise RuntimeError(f"release artifact tree contains a symbolic link: {path}")
            if path.is_file() and path.name not in EXCLUDED and path.resolve() != output:
                files.append(path)
        if not files:
            raise RuntimeError("release artifact tree is empty")
        lines = [
            f"{digest(path)}  {path.relative_to(root).as_posix()}"
            for path in sorted(files, key=lambda item: item.relative_to(root).as_posix())
        ]
        output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    except (OSError, RuntimeError) as error:
        print(f"checksum generation failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote {len(lines)} artifact checksums")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
