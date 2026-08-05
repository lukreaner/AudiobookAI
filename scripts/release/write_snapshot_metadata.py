#!/usr/bin/env python3
"""Write the warning and immutable source identity shipped with snapshots."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys


COMMIT = re.compile(r"^[0-9a-f]{40}$")
SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def metadata(version: str, commit: str, run_url: str) -> str:
    if not SEMVER.fullmatch(version):
        raise ValueError("snapshot version must be a stable three-part semantic version")
    if not COMMIT.fullmatch(commit):
        raise ValueError("snapshot commit must be a full lowercase SHA-1 object ID")
    if not run_url.startswith("https://") or any(character.isspace() for character in run_url):
        raise ValueError("snapshot workflow URL must be a whitespace-free HTTPS URL")
    return (
        "UNTRUSTED DEVELOPMENT BUILD - NOT A STABLE RELEASE\n\n"
        "This debug snapshot is automatically built from main on GitHub-hosted runners.\n"
        "Windows and Linux are unsigned; macOS has only an untrusted ad-hoc signature and is "
        "not notarized. No snapshot is updater-enabled or covered by stable-release support.\n"
        "It does not bundle the audited media sidecars; install FFmpeg/ffprobe and, on Linux, "
        "eSpeak NG on PATH for local development use.\n\n"
        "It may share application data with other AudiobookAI builds; use disposable test data "
        "and keep backups.\n\n"
        f"Version: {version}\n"
        f"Commit: {commit}\n"
        f"Source run: {run_url}\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        content = metadata(args.version, args.commit, args.run_url)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(content, encoding="utf-8")
    except (OSError, ValueError) as error:
        print(f"development snapshot metadata failed: {error}", file=sys.stderr)
        return 2
    print("wrote untrusted development snapshot warning")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
