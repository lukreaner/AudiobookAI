#!/usr/bin/env python3
"""Enforce the complete, clearly labelled untrusted snapshot artifact set."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

if __package__:
    from .verify_release_upload import digest, release_files
else:
    from verify_release_upload import digest, release_files


INSTALLERS = {
    "AudiobookAI-development-windows-x86_64.exe",
    "AudiobookAI-development-macos-universal.dmg",
    "AudiobookAI-development-linux-x86_64.AppImage",
    "AudiobookAI-development-linux-x86_64.deb",
}
MATERIALS = {"DEVELOPMENT-BUILD.txt", "LICENSE"}
CHECKSUM_LINE = re.compile(r"^([0-9a-f]{64})  ([^/\\]+)$")


class SnapshotArtifactError(RuntimeError):
    pass


def verify_snapshot(
    root: Path, *, version: str, commit: str, with_checksums: bool = False
) -> None:
    try:
        files = release_files(root)
    except RuntimeError as error:
        raise SnapshotArtifactError(str(error)) from error
    expected = INSTALLERS | MATERIALS
    if with_checksums:
        expected.add("SHA256SUMS")
    if files.keys() != expected:
        missing = sorted(expected - files.keys())
        unexpected = sorted(files.keys() - expected)
        raise SnapshotArtifactError(
            f"snapshot artifact set differs; missing={missing}, unexpected={unexpected}"
        )
    for name, path in files.items():
        if path.stat().st_size == 0:
            raise SnapshotArtifactError(f"snapshot artifact is empty: {name}")

    warning = files["DEVELOPMENT-BUILD.txt"].read_text(encoding="utf-8")
    required_lines = {
        "UNTRUSTED DEVELOPMENT BUILD - NOT A STABLE RELEASE",
        f"Version: {version}",
        f"Commit: {commit}",
    }
    if not required_lines.issubset(set(warning.splitlines())):
        raise SnapshotArtifactError("development warning does not match version and commit")

    if with_checksums:
        checksums: dict[str, str] = {}
        for line in files["SHA256SUMS"].read_text(encoding="utf-8").splitlines():
            match = CHECKSUM_LINE.fullmatch(line)
            if match is None or match.group(2) in checksums:
                raise SnapshotArtifactError("snapshot checksum manifest is malformed")
            checksums[match.group(2)] = match.group(1)
        checksum_targets = expected - {"SHA256SUMS"}
        if checksums.keys() != checksum_targets:
            raise SnapshotArtifactError("snapshot checksum manifest does not cover the exact set")
        for name in checksum_targets:
            if digest(files[name]) != checksums[name]:
                raise SnapshotArtifactError(f"snapshot checksum mismatch: {name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--with-checksums", action="store_true")
    args = parser.parse_args()
    try:
        verify_snapshot(
            args.root,
            version=args.version,
            commit=args.commit,
            with_checksums=args.with_checksums,
        )
    except (OSError, UnicodeError, SnapshotArtifactError) as error:
        print(f"development snapshot artifact gate failed: {error}", file=sys.stderr)
        return 2
    print("untrusted development snapshot artifact matrix is complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
