#!/usr/bin/env python3
"""Assemble exact application and bundled-component corresponding source."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


MAX_SOURCE_BYTES = 512 * 1024 * 1024


class SourceError(RuntimeError):
    pass


def run(arguments: list[str], *, cwd: Path | None = None) -> None:
    try:
        subprocess.run(arguments, cwd=cwd, check=True)
    except subprocess.CalledProcessError as error:
        raise SourceError(f"source command failed: {' '.join(arguments[:3])}") from error


def sha256(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def download(url: str, expected: str, destination: Path) -> None:
    request = urllib.request.Request(
        url, headers={"User-Agent": "AudiobookAI-source-offer-builder/1"}
    )
    written = 0
    try:
        with urllib.request.urlopen(request, timeout=120) as response, destination.open("wb") as out:
            while chunk := response.read(1024 * 1024):
                written += len(chunk)
                if written > MAX_SOURCE_BYTES:
                    raise SourceError("upstream source archive exceeds 512 MiB")
                out.write(chunk)
    except SourceError:
        raise
    except Exception as error:
        raise SourceError(f"could not download upstream source {url}: {error}") from error
    if sha256(destination) != expected:
        raise SourceError(f"upstream source checksum mismatch for {destination.name}")


def create_outer_archive(source: Path, output: Path, root_name: str, epoch: int) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for path in sorted(source.rglob("*"), key=lambda item: item.relative_to(source).as_posix()):
                    if path.is_symlink():
                        raise SourceError(f"source material contains a symbolic link: {path}")
                    relative = Path(root_name) / path.relative_to(source)
                    info = archive.gettarinfo(str(path), arcname=relative.as_posix())
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mtime = epoch
                    if info.isdir():
                        info.mode = 0o755
                        archive.addfile(info)
                    elif info.isfile():
                        info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                        with path.open("rb") as stream:
                            archive.addfile(info, stream)
                    else:
                        raise SourceError(f"unsupported source material type: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        epoch_text = os.environ.get("SOURCE_DATE_EPOCH")
        if not epoch_text or not epoch_text.isdigit():
            raise SourceError("SOURCE_DATE_EPOCH must be set to the release commit time")
        epoch = int(epoch_text)
        cargo = Path("Cargo.toml").read_text(encoding="utf-8")
        import tomllib

        version = tomllib.loads(cargo)["workspace"]["package"]["version"]
        manifest = json.loads(
            Path("packaging/sidecars.lock.json").read_text(encoding="utf-8")
        )
        with tempfile.TemporaryDirectory(prefix="audiobookai-source-") as temporary_name:
            temporary = Path(temporary_name)
            material = temporary / "material"
            upstream = material / "upstream"
            build_material = material / "build-material"
            upstream.mkdir(parents=True)
            build_material.mkdir(parents=True)

            run(
                [
                    "git",
                    "archive",
                    "--format=tar",
                    "--output",
                    str(material / f"AudiobookAI-{version}-source.tar"),
                    "HEAD",
                ]
            )
            ffmpeg = manifest["ffmpeg"]["source"]
            download(
                ffmpeg["url"],
                ffmpeg["sha256"],
                upstream / f"ffmpeg-{manifest['ffmpeg']['version']}.tar.xz",
            )
            lame = manifest["libmp3lame"]["source"]
            download(
                lame["url"],
                lame["sha256"],
                upstream / f"lame-{manifest['libmp3lame']['version']}.tar.gz",
            )

            espeak_source = manifest["espeakNg"]["source"]
            espeak_repository = temporary / "espeak-ng"
            run(["git", "init", "--quiet", str(espeak_repository)])
            run(
                ["git", "remote", "add", "origin", espeak_source["gitUrl"]],
                cwd=espeak_repository,
            )
            run(
                ["git", "fetch", "--quiet", "--depth", "1", "origin", espeak_source["commit"]],
                cwd=espeak_repository,
            )
            run(
                [
                    "git",
                    "archive",
                    "--format=tar",
                    "--output",
                    str(upstream / f"espeak-ng-{manifest['espeakNg']['version']}.tar"),
                    espeak_source["commit"],
                ],
                cwd=espeak_repository,
            )

            shutil.copy2("packaging/sidecars.lock.json", build_material / "sidecars.lock.json")
            shutil.copy2("docs/sidecars.md", build_material / "SIDECARS.md")
            shutil.copy2("LICENSE", material / "LICENSE")
            (build_material / "FFMPEG_BUILD_FLAGS.txt").write_text(
                "\n".join(manifest["ffmpeg"]["configureFlags"]) + "\n", encoding="utf-8"
            )
            (material / "SOURCE_OFFER_README.txt").write_text(
                "This archive contains the exact AudiobookAI source tree, pinned upstream source "
                "archives for FFmpeg and libmp3lame, the pinned eSpeak NG source tree, and the "
                "sidecar build contract. Any downstream patches are part of the AudiobookAI "
                "source archive under packaging; absence of that patch directory means none were "
                "declared. Build logs, SBOM, notices, and binary checksums are separate signed "
                "release assets.\n",
                encoding="utf-8",
            )
            create_outer_archive(
                material,
                args.output,
                f"AudiobookAI-{version}-corresponding-source",
                epoch,
            )
    except (OSError, KeyError, ValueError, json.JSONDecodeError, SourceError) as error:
        print(f"corresponding-source generation failed: {error}", file=sys.stderr)
        return 2
    print(f"wrote corresponding source archive {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
