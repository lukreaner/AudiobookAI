#!/usr/bin/env python3
"""Execute verified sidecars and capture their release capability evidence."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


class EvidenceError(RuntimeError):
    pass


def run(arguments: list[str]) -> str:
    environment = {
        name: value
        for name, value in os.environ.items()
        if name.upper()
        in {"HOME", "LANG", "PATH", "SYSTEMROOT", "TEMP", "TMP", "USERPROFILE", "WINDIR"}
    }
    environment["LC_ALL"] = "C"
    try:
        result = subprocess.run(
            arguments,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
            env=environment,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise EvidenceError(f"sidecar command failed: {Path(arguments[0]).name}") from error
    return result.stdout.replace("\r\n", "\n")


def listed_names(output: str) -> set[str]:
    """Extract exact names from FFmpeg's flag-prefixed capability tables."""
    names: set[str] = set()
    for line in output.splitlines():
        columns = line.split()
        if len(columns) < 2 or not re.fullmatch(r"[A-Z.]+", columns[0]):
            continue
        names.update(name for name in columns[1].split(",") if name != "=")
    return names


def require_capabilities(section: str, output: str, expected: list[str]) -> None:
    available = listed_names(output)
    missing = sorted(set(expected) - available)
    if missing:
        raise EvidenceError(
            f"FFmpeg sidecar is missing required {section}: {', '.join(missing)}"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--linux-espeak", action="store_true")
    parser.add_argument("--macos-uv", action="store_true")
    parser.add_argument("--provenance-output", type=Path)
    args = parser.parse_args()
    try:
        manifest = json.loads(
            Path("packaging/sidecars.lock.json").read_text(encoding="utf-8")
        )
        ffmpeg_contract = manifest["ffmpeg"]
        extension = ".exe" if os.name == "nt" else ""
        ffmpeg = (args.root / "bin" / f"ffmpeg{extension}").resolve()
        ffprobe = (args.root / "bin" / f"ffprobe{extension}").resolve()
        sections = {
            "ffmpeg-version": run([str(ffmpeg), "-hide_banner", "-version"]),
            "ffmpeg-buildconf": run([str(ffmpeg), "-hide_banner", "-buildconf"]),
            "ffmpeg-encoders": run([str(ffmpeg), "-hide_banner", "-encoders"]),
            "ffmpeg-filters": run([str(ffmpeg), "-hide_banner", "-filters"]),
            "ffmpeg-muxers": run([str(ffmpeg), "-hide_banner", "-muxers"]),
            "ffprobe-version": run([str(ffprobe), "-hide_banner", "-version"]),
        }
        if not re.search(r"(?m)^ffmpeg version 8\.1\.1(?:\s|$)", sections["ffmpeg-version"]):
            raise EvidenceError("sidecar does not report FFmpeg 8.1.1")
        if not re.search(r"(?m)^ffprobe version 8\.1\.1(?:\s|$)", sections["ffprobe-version"]):
            raise EvidenceError("sidecar does not report ffprobe 8.1.1")
        for forbidden in ["--enable-gpl", "--enable-nonfree"]:
            if forbidden in sections["ffmpeg-buildconf"]:
                raise EvidenceError(f"FFmpeg sidecar contains forbidden flag {forbidden}")
        missing_flags = [
            flag
            for flag in ffmpeg_contract["configureFlags"]
            if flag not in sections["ffmpeg-buildconf"]
        ]
        if missing_flags:
            raise EvidenceError(
                "FFmpeg sidecar is missing required configure flags: "
                + ", ".join(missing_flags)
            )
        required = ffmpeg_contract["requiredFeatures"]
        require_capabilities("encoders", sections["ffmpeg-encoders"], required["encoders"])
        require_capabilities("filters", sections["ffmpeg-filters"], required["filters"])
        require_capabilities("muxers", sections["ffmpeg-muxers"], required["muxers"])
        if args.linux_espeak:
            espeak = (args.root / "bin" / "espeak-ng").resolve()
            espeak_data = (args.root / "share" / "espeak-ng-data").resolve()
            if not espeak_data.is_dir():
                raise EvidenceError("Linux eSpeak NG voice-data directory is missing")
            data_argument = f"--path={espeak_data.parent}"
            sections["espeak-version"] = run(
                [str(espeak), data_argument, "--version"]
            )
            if not re.search(r"(?<![0-9.])1\.52\.0(?![0-9.])", sections["espeak-version"]):
                raise EvidenceError("Linux sidecar does not report eSpeak NG 1.52.0")
            sections["espeak-voices"] = run(
                [str(espeak), data_argument, "--voices"]
            )
            if "Language" not in sections["espeak-voices"]:
                raise EvidenceError("Linux eSpeak NG could not load its packaged voice data")
        if args.macos_uv:
            uv = (args.root / "bin" / "uv").resolve()
            sections["uv-version"] = run([str(uv), "--version"])
            if not re.fullmatch(r"uv 0\.12\.1\s*", sections["uv-version"]):
                raise EvidenceError("Apple sidecar does not report exactly uv 0.12.1")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            "".join(f"## {name}\n{value}\n" for name, value in sections.items()),
            encoding="utf-8",
        )
        if args.provenance_output:
            provenance = args.root / "sidecar-provenance.json"
            if not provenance.is_file():
                raise EvidenceError("sidecar provenance record is missing")
            args.provenance_output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(provenance, args.provenance_output)
    except (OSError, KeyError, json.JSONDecodeError, EvidenceError) as error:
        print(f"sidecar evidence gate failed: {error}", file=sys.stderr)
        return 2
    print(f"captured sidecar capability evidence in {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
