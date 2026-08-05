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
EXPECTED_DESKTOP_NAME = "AudiobookAI"
EXPECTED_DESKTOP_IDENTIFIER = "ai.audiobook.desktop"
EXPECTED_DASHBOARD_NAME = "@audiobookai/dashboard"


class VersionError(RuntimeError):
    pass


def read_toml(root: Path, relative: str) -> dict:
    return tomllib.loads((root / relative).read_text(encoding="utf-8"))


def read_json(root: Path, relative: str) -> dict:
    return json.loads((root / relative).read_text(encoding="utf-8"))


def workspace_packages(root: Path, cargo: dict, version: str) -> dict[str, str]:
    packages: dict[str, str] = {}
    for member in cargo.get("workspace", {}).get("members", []):
        manifest = read_toml(root, f"{member}/Cargo.toml")
        package = manifest.get("package", {})
        name = package.get("name")
        if not isinstance(name, str) or not name:
            raise VersionError(f"workspace member {member} has no package name")
        if package.get("version") != {"workspace": True}:
            raise VersionError(
                f"workspace package {name} must inherit the release version"
            )
        if name in packages:
            raise VersionError(f"duplicate workspace package name: {name}")
        packages[name] = version
    if not packages:
        raise VersionError("Cargo workspace has no package members")
    return packages


def verify_cargo_lock(root: Path, packages: dict[str, str]) -> None:
    lock = read_toml(root, "Cargo.lock")
    entries = lock.get("package", [])
    for name, expected_version in packages.items():
        matches = [
            entry
            for entry in entries
            if entry.get("name") == name and entry.get("source") is None
        ]
        if len(matches) != 1 or matches[0].get("version") != expected_version:
            found = [entry.get("version") for entry in matches]
            raise VersionError(
                f"Cargo.lock workspace package {name} must be exactly {expected_version}; found {found}"
            )


def verify_package_identity(desktop: dict, tauri: dict, frontend: dict) -> None:
    binaries = desktop.get("bin", [])
    if len(binaries) != 1 or binaries[0].get("name") != EXPECTED_DESKTOP_NAME:
        raise VersionError(
            f"desktop Cargo manifest must define exactly one {EXPECTED_DESKTOP_NAME} binary"
        )
    if tauri.get("productName") != EXPECTED_DESKTOP_NAME:
        raise VersionError("Tauri productName does not match the desktop executable")
    if tauri.get("identifier") != EXPECTED_DESKTOP_IDENTIFIER:
        raise VersionError(
            f"Tauri identifier must remain {EXPECTED_DESKTOP_IDENTIFIER} for update continuity"
        )
    if frontend.get("name") != EXPECTED_DASHBOARD_NAME or frontend.get("private") is not True:
        raise VersionError("dashboard package identity must remain private and release-bound")
    updater = tauri.get("plugins", {}).get("updater", {})
    if updater.get("endpoints") != [] or updater.get("pubkey") != "":
        raise VersionError("base Tauri updater configuration must remain disabled")
    if tauri.get("bundle", {}).get("createUpdaterArtifacts") is not False:
        raise VersionError("base Tauri config must not create unsigned updater artifacts")


def verify_repository(
    root: Path, *, tag: str | None = None, release: bool = False
) -> str:
    cargo = read_toml(root, "Cargo.toml")
    toolchain = read_toml(root, "rust-toolchain.toml")
    desktop = read_toml(root, "apps/desktop/src-tauri/Cargo.toml")
    tauri = read_json(root, "apps/desktop/src-tauri/tauri.conf.json")
    frontend = read_json(root, "web/package.json")
    sidecars = read_json(root, "packaging/sidecars.lock.json")
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
    packages = workspace_packages(root, cargo, version)
    verify_cargo_lock(root, packages)
    verify_package_identity(desktop, tauri, frontend)
    if toolchain.get("toolchain", {}).get("channel") != "1.96.0":
        raise VersionError("rust-toolchain.toml must remain pinned to Rust 1.96.0")
    if frontend.get("packageManager") != "pnpm@11.18.0":
        raise VersionError("web/package.json must pin pnpm@11.18.0")
    if sidecars.get("ffmpeg", {}).get("version") != "8.1.1":
        raise VersionError("sidecar manifest must pin FFmpeg 8.1.1")
    if not (root / "Cargo.lock").is_file() or not (root / "pnpm-lock.yaml").is_file():
        raise VersionError("both Rust and frontend lockfiles are required")
    if tag and tag != f"v{version}":
        raise VersionError(f"release tag must be exactly v{version}")
    if release and int(version.split(".", maxsplit=1)[0]) < 1:
        raise VersionError(
            "public release packaging is blocked until the version is 1.0.0 or newer"
        )
    if release:
        subprocess.run(
            [sys.executable, "scripts/release/sidecars.py", "manifest"],
            cwd=root,
            check=True,
        )
    return version


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    try:
        version = verify_repository(Path.cwd(), tag=args.tag, release=args.release)
    except (OSError, KeyError, ValueError, json.JSONDecodeError, VersionError) as error:
        print(f"release version gate failed: {error}", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError:
        return 2
    print(f"repository version pins are consistent at {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
