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
                "## Optional app-managed Piper runtime",
                "",
                "Piper 1.2.0 is not bundled with AudiobookAI. On Linux x86_64 it is downloaded ",
                "from the official `rhasspy/piper` v1.2.0 release only after an explicit user ",
                "install action, and its archive is SHA-256 verified before installation.",
                "",
                "- Component: Piper 1.2.0",
                "- Declared engine license: MIT",
                "- Source: <https://github.com/rhasspy/piper/tree/v1.2.0>",
                "- Release: <https://github.com/rhasspy/piper/releases/tag/v1.2.0>",
                "- License text: <https://github.com/rhasspy/piper/blob/v1.2.0/LICENSE.md>",
                "",
                "## Optional curated Piper voice",
                "",
                "Voice artifacts are separate from the Piper engine and are not covered merely ",
                "by the engine's MIT license. AudiobookAI's initial catalog offers only ",
                "`de_DE-thorsten-medium`, pinned to `rhasspy/piper-voices` commit ",
                "`f5a6e9094787fd865d65cb024472f977f9c542b5`. Its ONNX model, JSON config, and ",
                "model card are independently checksum-locked. The pinned model card declares ",
                "the source dataset license as CC0; that is a scoped model-card dataset ",
                "declaration, not an unqualified license assertion for unrelated repository ",
                "artifacts. AudiobookAI displays the declaration and voice provenance and ",
                "requires confirmation before download.",
                "",
                "- Voice: `de_DE-thorsten-medium`",
                (
                    "- Pinned model card: <https://huggingface.co/rhasspy/piper-voices/blob/"
                    "f5a6e9094787fd865d65cb024472f977f9c542b5/de/de_DE/thorsten/medium/"
                    "MODEL_CARD>"
                ),
                "- Declared source-dataset license: CC0-1.0",
                "- License information: <https://creativecommons.org/publicdomain/zero/1.0/>",
                (
                    "- Upstream voice source: "
                    "<https://github.com/thorstenMueller/deep-learning-german-tts>"
                ),
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
