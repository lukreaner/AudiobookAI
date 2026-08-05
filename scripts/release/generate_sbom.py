#!/usr/bin/env python3
"""Generate a deterministic CycloneDX inventory for the packaged desktop app."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tomllib
import urllib.parse


class SbomError(RuntimeError):
    pass


def command_json(arguments: list[str]) -> object:
    try:
        result = subprocess.run(
            arguments,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        return json.loads(result.stdout)
    except (subprocess.CalledProcessError, json.JSONDecodeError) as error:
        detail = error.stderr.strip() if isinstance(error, subprocess.CalledProcessError) else str(error)
        raise SbomError(f"dependency inventory command failed: {detail}") from error


def cargo_components() -> list[dict]:
    metadata = command_json(["cargo", "metadata", "--locked", "--format-version", "1"])
    if not isinstance(metadata, dict):
        raise SbomError("cargo metadata returned an unexpected document")
    packages = {package["id"]: package for package in metadata["packages"]}
    desktop = next(
        (package for package in packages.values() if package["name"] == "audiobookai-desktop"),
        None,
    )
    if desktop is None:
        raise SbomError("audiobookai-desktop is missing from Cargo metadata")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    included: set[str] = set()
    pending = [desktop["id"]]
    while pending:
        package_id = pending.pop()
        if package_id in included:
            continue
        included.add(package_id)
        pending.extend(dependency["pkg"] for dependency in nodes[package_id].get("deps", []))

    lock = tomllib.loads(Path("Cargo.lock").read_text(encoding="utf-8"))
    checksums = {
        (item["name"], item["version"], item.get("source")): item.get("checksum")
        for item in lock.get("package", [])
    }
    components: list[dict] = []
    for package_id in included:
        package = packages[package_id]
        if package["name"] == "audiobookai-desktop":
            continue
        purl = "pkg:cargo/{}@{}".format(
            urllib.parse.quote(package["name"], safe=""),
            urllib.parse.quote(package["version"], safe=""),
        )
        component: dict = {
            "type": "library",
            "bom-ref": purl,
            "name": package["name"],
            "version": package["version"],
            "purl": purl,
        }
        if package.get("license"):
            component["licenses"] = [{"expression": package["license"]}]
        checksum = checksums.get(
            (package["name"], package["version"], package.get("source"))
        )
        if checksum:
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        references = []
        if package.get("repository"):
            references.append({"type": "vcs", "url": package["repository"]})
        if package.get("homepage"):
            references.append({"type": "website", "url": package["homepage"]})
        if references:
            component["externalReferences"] = references
        components.append(component)
    return components


def frontend_components() -> list[dict]:
    inventory = command_json(["pnpm", "--dir", "web", "licenses", "list", "--prod", "--json"])
    if not isinstance(inventory, dict):
        raise SbomError("pnpm license inventory returned an unexpected document")
    components: list[dict] = []
    for declared_license, packages in inventory.items():
        for package in packages:
            for version in package.get("versions", []):
                name = package["name"]
                purl = "pkg:npm/{}@{}".format(
                    urllib.parse.quote(name, safe="@"), urllib.parse.quote(version, safe="")
                )
                component: dict = {
                    "type": "library",
                    "bom-ref": purl,
                    "name": name,
                    "version": version,
                    "purl": purl,
                    "licenses": [
                        {"expression": package.get("license") or declared_license}
                    ],
                }
                if package.get("homepage"):
                    component["externalReferences"] = [
                        {"type": "website", "url": package["homepage"]}
                    ]
                components.append(component)
    return components


def sidecar_components() -> list[dict]:
    manifest = json.loads(
        Path("packaging/sidecars.lock.json").read_text(encoding="utf-8")
    )
    definitions = [
        ("ffmpeg", "FFmpeg", manifest["ffmpeg"], "url"),
        ("lame", "LAME", manifest["libmp3lame"], "url"),
        ("espeak-ng", "eSpeak NG", manifest["espeakNg"], "gitUrl"),
        ("uv", "uv", manifest["uv"], "url"),
    ]
    components = []
    for package_name, display_name, definition, source_key in definitions:
        purl = f"pkg:generic/{package_name}@{definition['version']}"
        source = definition["source"]
        component: dict = {
            "type": "application",
            "bom-ref": purl,
            "name": display_name,
            "version": definition["version"],
            "purl": purl,
            "licenses": [{"expression": definition["license"]}],
            "externalReferences": [{"type": "distribution", "url": source[source_key]}],
        }
        if source.get("sha256"):
            component["hashes"] = [{"alg": "SHA-256", "content": source["sha256"]}]
        properties = []
        if source.get("commit"):
            properties.append({"name": "audiobookai:source-commit", "value": source["commit"]})
        if package_name == "ffmpeg":
            properties.append(
                {
                    "name": "audiobookai:configure-flags",
                    "value": " ".join(definition["configureFlags"]),
                }
            )
        if properties:
            component["properties"] = properties
        components.append(component)
    return components


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))[
            "workspace"
        ]["package"]["version"]
        root_ref = f"pkg:generic/audiobookai@{version}"
        unique: dict[str, dict] = {}
        for component in cargo_components() + frontend_components() + sidecar_components():
            unique[component["bom-ref"]] = component
        components = [unique[key] for key in sorted(unique)]
        document = {
            "bomFormat": "CycloneDX",
            "specVersion": "1.6",
            "serialNumber": f"urn:uuid:00000000-0000-4000-8000-{version.replace('.', '').ljust(12, '0')[:12]}",
            "version": 1,
            "metadata": {
                "component": {
                    "type": "application",
                    "bom-ref": root_ref,
                    "name": "AudiobookAI",
                    "version": version,
                    "purl": root_ref,
                    "licenses": [{"expression": "GPL-3.0-only"}],
                }
            },
            "components": components,
            "dependencies": [
                {"ref": root_ref, "dependsOn": [component["bom-ref"] for component in components]}
            ],
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except (OSError, KeyError, ValueError, SbomError, json.JSONDecodeError) as error:
        print(f"SBOM generation failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote CycloneDX SBOM with {len(components)} components")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
