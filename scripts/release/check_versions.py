#!/usr/bin/env python3
"""Verify repository version pins and the stable-release tag."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib


SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


class VersionError(RuntimeError):
    pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    try:
        cargo = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))
        toolchain = tomllib.loads(Path("rust-toolchain.toml").read_text(encoding="utf-8"))
        tauri = json.loads(
            Path("apps/desktop/src-tauri/tauri.conf.json").read_text(encoding="utf-8")
        )
        frontend = json.loads(Path("web/package.json").read_text(encoding="utf-8"))
        sidecars = json.loads(
            Path("packaging/sidecars.lock.json").read_text(encoding="utf-8")
        )
        version = cargo["workspace"]["package"]["version"]
        if not SEMVER.fullmatch(version):
            raise VersionError("workspace version must be a stable three-part semantic version")
        mismatches = {
            "Tauri": tauri.get("version"),
            "dashboard": frontend.get("version"),
        }
        wrong = {name: value for name, value in mismatches.items() if value != version}
        if wrong:
            details = ", ".join(f"{name}={value}" for name, value in wrong.items())
            raise VersionError(f"release versions do not match Cargo {version}: {details}")
        if toolchain.get("toolchain", {}).get("channel") != "1.96.0":
            raise VersionError("rust-toolchain.toml must remain pinned to Rust 1.96.0")
        if frontend.get("packageManager") != "pnpm@11.18.0":
            raise VersionError("web/package.json must pin pnpm@11.18.0")
        if sidecars.get("ffmpeg", {}).get("version") != "8.1.1":
            raise VersionError("sidecar manifest must pin FFmpeg 8.1.1")
        if not Path("Cargo.lock").is_file() or not Path("pnpm-lock.yaml").is_file():
            raise VersionError("both Rust and frontend lockfiles are required")
        if args.tag and args.tag != f"v{version}":
            raise VersionError(f"release tag must be exactly v{version}")
        if args.release and int(version.split(".", maxsplit=1)[0]) < 1:
            raise VersionError(
                "public release packaging is blocked until the version is 1.0.0 or newer"
            )
        if args.release:
            subprocess.run(
                [sys.executable, "scripts/release/sidecars.py", "manifest"], check=True
            )
    except (OSError, KeyError, ValueError, json.JSONDecodeError, VersionError) as error:
        print(f"release version gate failed: {error}", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError:
        return 2
    print(f"repository version pins are consistent at {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
