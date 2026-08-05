#!/usr/bin/env python3
"""Generate a credential-free Tauri release overlay for one verified target."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import stat
import sys


class ConfigError(RuntimeError):
    pass


def resource_destination(relative: Path) -> str:
    """Preserve the verified bundle tree below the installed sidecar root."""
    return Path("sidecars", *relative.parts).as_posix()


def verified_resource_files(root: Path) -> list[Path]:
    try:
        root_metadata = root.lstat()
    except OSError as error:
        raise ConfigError(f"verified sidecar directory is missing: {root}") from error
    if not stat.S_ISDIR(root_metadata.st_mode) or stat.S_ISLNK(root_metadata.st_mode):
        raise ConfigError("verified sidecar root must be a non-symlink directory")
    canonical_root = root.resolve(strict=True)
    files: list[Path] = []
    for candidate in sorted(root.rglob("*")):
        relative = candidate.relative_to(root)
        current = root
        for component in relative.parts:
            current = current / component
            metadata = current.lstat()
            if stat.S_ISLNK(metadata.st_mode):
                raise ConfigError("verified sidecar tree contains a symbolic link")
        metadata = candidate.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise ConfigError("verified sidecar tree contains a non-regular resource")
        if not candidate.resolve(strict=True).is_relative_to(canonical_root):
            raise ConfigError("verified sidecar resource escapes its root")
        files.append(candidate)
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--sidecars", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        public_key = os.environ.get("TAURI_SIGNING_PUBLIC_KEY")
        endpoint = os.environ.get("AUDIOBOOKAI_UPDATER_ENDPOINT")
        if not public_key or not endpoint:
            raise ConfigError(
                "TAURI_SIGNING_PUBLIC_KEY and AUDIOBOOKAI_UPDATER_ENDPOINT are required"
            )
        if not endpoint.startswith("https://"):
            raise ConfigError("updater endpoint must use HTTPS")
        sidecars = args.sidecars.absolute()
        resource_files = verified_resource_files(sidecars)
        desktop = Path("apps/desktop/src-tauri").resolve()
        resources: dict[str, str] = {}
        for source in resource_files:
            relative = source.relative_to(sidecars)
            resources[os.path.relpath(source, desktop)] = resource_destination(relative)
        if not resources:
            raise ConfigError("sidecar directory is empty")
        overlay: dict = {
            "bundle": {
                "createUpdaterArtifacts": True,
                "resources": resources,
            },
            "plugins": {"updater": {"pubkey": public_key, "endpoints": [endpoint]}},
        }
        if args.target == "x86_64-pc-windows-msvc":
            thumbprint = os.environ.get("WINDOWS_CERTIFICATE_THUMBPRINT")
            if not thumbprint:
                raise ConfigError("WINDOWS_CERTIFICATE_THUMBPRINT is required")
            overlay["bundle"]["windows"] = {
                "certificateThumbprint": thumbprint,
                "digestAlgorithm": "sha256",
                "timestampUrl": "http://timestamp.digicert.com",
            }
        elif args.target == "universal-apple-darwin":
            identity = os.environ.get("APPLE_SIGNING_IDENTITY")
            if not identity:
                raise ConfigError("APPLE_SIGNING_IDENTITY is required")
            overlay["bundle"]["macOS"] = {"signingIdentity": identity}
        elif args.target != "x86_64-unknown-linux-gnu":
            raise ConfigError(f"unsupported release target {args.target}")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(overlay, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (OSError, ConfigError) as error:
        print(f"release configuration failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote Tauri release overlay for {args.target} without credential material")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
