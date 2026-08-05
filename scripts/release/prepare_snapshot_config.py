#!/usr/bin/env python3
"""Generate a clearly isolated, untrusted Tauri development-build overlay."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


TARGETS = {
    "x86_64-pc-windows-msvc",
    "universal-apple-darwin",
    "x86_64-unknown-linux-gnu",
}


def snapshot_config(target: str) -> dict:
    if target not in TARGETS:
        raise ValueError(f"unsupported development snapshot target {target}")
    config = {
        "productName": "AudiobookAI Development",
        "identifier": "ai.audiobook.desktop.development",
        "bundle": {
            "createUpdaterArtifacts": False,
            "fileAssociations": [],
            "resources": {},
        },
        "plugins": {"updater": {"endpoints": [], "pubkey": ""}},
    }
    if target == "universal-apple-darwin":
        config["bundle"]["macOS"] = {"signingIdentity": "-"}
    return config


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        overlay = snapshot_config(args.target)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(overlay, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (OSError, ValueError) as error:
        print(f"development snapshot configuration failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote untrusted development overlay for {args.target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
