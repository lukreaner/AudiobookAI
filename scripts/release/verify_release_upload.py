#!/usr/bin/env python3
"""Verify that a downloaded GitHub Release draft exactly matches its candidate."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import stat
import sys


class UploadVerificationError(RuntimeError):
    pass


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def release_files(root: Path) -> dict[str, Path]:
    try:
        metadata = root.lstat()
    except OSError as error:
        raise UploadVerificationError(f"release artifact directory is missing: {root}") from error
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise UploadVerificationError("release artifact root must be a non-symlink directory")

    files: dict[str, Path] = {}
    for candidate in sorted(root.iterdir()):
        metadata = candidate.lstat()
        if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
            raise UploadVerificationError(
                f"release artifact root contains a non-regular entry: {candidate.name}"
            )
        files[candidate.name] = candidate
    if not files:
        raise UploadVerificationError("release artifact directory is empty")
    return files


def verify_upload(candidate_root: Path, downloaded_root: Path) -> None:
    candidate = release_files(candidate_root)
    downloaded = release_files(downloaded_root)
    if candidate.keys() != downloaded.keys():
        missing = sorted(candidate.keys() - downloaded.keys())
        unexpected = sorted(downloaded.keys() - candidate.keys())
        raise UploadVerificationError(
            f"uploaded asset set differs; missing={missing}, unexpected={unexpected}"
        )
    for name, source in candidate.items():
        remote = downloaded[name]
        if source.stat().st_size != remote.stat().st_size or digest(source) != digest(remote):
            raise UploadVerificationError(f"uploaded asset differs from candidate: {name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("downloaded", type=Path)
    args = parser.parse_args()
    try:
        verify_upload(args.candidate, args.downloaded)
    except (OSError, UploadVerificationError) as error:
        print(f"release upload verification failed: {error}", file=sys.stderr)
        return 2
    print("downloaded release assets exactly match the staged candidate")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
