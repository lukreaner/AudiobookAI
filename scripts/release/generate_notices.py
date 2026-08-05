#!/usr/bin/env python3
"""Create the release dependency/license inventory from the generated SBOM."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


def license_text(component: dict) -> str:
    licenses = component.get("licenses", [])
    values = [entry.get("expression") or entry.get("license", {}).get("id") for entry in licenses]
    return ", ".join(value for value in values if value) or "License metadata unavailable"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sbom", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        document = json.loads(args.sbom.read_text(encoding="utf-8"))
        components = sorted(
            document["components"], key=lambda item: (item["name"].casefold(), item["version"])
        )
        lines = [
            "# AudiobookAI third-party notices",
            "",
            "AudiobookAI is licensed under GPL-3.0-only. This generated inventory accompanies ",
            "the complete license texts and corresponding-source archive distributed with the release.",
            "The CycloneDX SBOM is authoritative for exact resolved versions and checksums.",
            "",
            "| Component | Version | Declared license |",
            "| --- | --- | --- |",
        ]
        for component in components:
            name = component["name"].replace("|", "\\|")
            version = component["version"].replace("|", "\\|")
            license_value = license_text(component).replace("|", "\\|")
            lines.append(f"| {name} | {version} | {license_value} |")
        lines.extend(
            [
                "",
                "## Bundled media components",
                "",
                "The exact FFmpeg, libmp3lame, and eSpeak NG sources, build configuration, ",
                "patches, checksums, and license material are contained in the corresponding-source ",
                "archive published beside this binary release. See `docs/sidecars.md` and ",
                "`packaging/sidecars.lock.json` in that archive.",
                "",
            ]
        )
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text("\n".join(lines), encoding="utf-8")
    except (OSError, KeyError, json.JSONDecodeError) as error:
        print(f"notice generation failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote notices for {len(components)} resolved components")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
