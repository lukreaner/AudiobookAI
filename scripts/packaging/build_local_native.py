#!/usr/bin/env python3
"""Build and publish a current non-release-signed host-native AudiobookAI package."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from typing import BinaryIO, Iterable
import uuid


REPOSITORY = Path(__file__).resolve().parents[2]
OUTPUT_ROOT = REPOSITORY / "artifacts" / "local-native"
CARGO_TARGET_ROOT = REPOSITORY / "target" / "local-native"
PNPM = "pnpm.cmd" if sys.platform == "win32" else "pnpm"
SOURCE_INPUTS = (
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "apps",
    "crates",
    "web",
    "scripts/packaging",
)


class LocalPackageError(RuntimeError):
    """A safe, actionable local packaging failure."""


@dataclass(frozen=True)
class SourceState:
    commit: str
    dirty: bool
    digest: str
    source_date_epoch: int


@dataclass(frozen=True)
class Artifact:
    name: str
    kind: str
    sha256: str
    size: int


def run_capture(command: list[str], *, binary: bool = False) -> str | bytes:
    try:
        result = subprocess.run(
            command,
            cwd=REPOSITORY,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=not binary,
        )
    except FileNotFoundError as error:
        raise LocalPackageError(f"required command is unavailable: {command[0]}") from error
    except subprocess.CalledProcessError as error:
        detail = error.stderr if isinstance(error.stderr, str) else error.stderr.decode(errors="replace")
        raise LocalPackageError(
            f"command failed ({shlex.join(command)}): {detail.strip()}"
        ) from error
    return result.stdout


def run_visible(command: list[str], environment: dict[str, str]) -> None:
    print(f"$ {shlex.join(command)}", flush=True)
    try:
        subprocess.run(command, cwd=REPOSITORY, env=environment, check=True)
    except FileNotFoundError as error:
        raise LocalPackageError(f"required command is unavailable: {command[0]}") from error
    except subprocess.CalledProcessError as error:
        raise LocalPackageError(
            f"native build failed with exit status {error.returncode}"
        ) from error


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def ensure_real_directory(path: Path, *, base: Path) -> None:
    try:
        relative = path.relative_to(base)
    except ValueError as error:
        raise LocalPackageError(f"managed output escapes the repository: {path}") from error
    current = base
    for component in relative.parts:
        current /= component
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            current.mkdir()
            continue
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise LocalPackageError(f"managed output component is not a real directory: {current}")


def require_regular_file(path: Path, description: str) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError as error:
        raise LocalPackageError(f"{description} is missing: {path}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise LocalPackageError(f"{description} is not a regular file: {path}")


def git_source_state() -> SourceState:
    commit = str(run_capture(["git", "rev-parse", "HEAD"])).strip()
    epoch_text = str(run_capture(["git", "show", "-s", "--format=%ct", "HEAD"])).strip()
    try:
        source_date_epoch = int(os.environ.get("SOURCE_DATE_EPOCH", epoch_text))
    except ValueError as error:
        raise LocalPackageError("SOURCE_DATE_EPOCH must be a non-negative integer") from error
    if source_date_epoch < 0:
        raise LocalPackageError("SOURCE_DATE_EPOCH must be a non-negative integer")

    status = str(
        run_capture(
            ["git", "status", "--porcelain=v1", "--untracked-files=all", "--", *SOURCE_INPUTS]
        )
    )
    difference = bytes(
        run_capture(
            ["git", "diff", "--binary", "--no-ext-diff", "HEAD", "--", *SOURCE_INPUTS],
            binary=True,
        )
    )
    untracked_output = bytes(
        run_capture(
            ["git", "ls-files", "--others", "--exclude-standard", "-z", "--", *SOURCE_INPUTS],
            binary=True,
        )
    )
    untracked = sorted(item for item in untracked_output.split(b"\0") if item)
    digest = hashlib.sha256()
    digest.update(b"audiobookai-local-source-v1\0")
    digest.update(commit.encode())
    digest.update(b"\0tracked-diff\0")
    digest.update(difference)
    for encoded_path in untracked:
        relative = encoded_path.decode("utf-8", errors="surrogateescape")
        path = REPOSITORY / relative
        metadata = path.lstat()
        digest.update(b"\0untracked\0")
        digest.update(encoded_path)
        digest.update(f"\0{stat.S_IMODE(metadata.st_mode):o}\0".encode())
        if stat.S_ISLNK(metadata.st_mode):
            digest.update(os.readlink(path).encode("utf-8", errors="surrogateescape"))
        elif stat.S_ISREG(metadata.st_mode):
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
        else:
            raise LocalPackageError(f"untracked build input is not a file: {path}")
    return SourceState(
        commit=commit,
        dirty=bool(status.strip()),
        digest=digest.hexdigest(),
        source_date_epoch=source_date_epoch,
    )


def workspace_version() -> str:
    manifest = tomllib.loads((REPOSITORY / "Cargo.toml").read_text(encoding="utf-8"))
    value = manifest.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(value, str) or not value:
        raise LocalPackageError("Cargo workspace version is missing")
    return value


def host_target() -> str:
    details = str(run_capture(["rustc", "-vV"]))
    for line in details.splitlines():
        if line.startswith("host: "):
            target = line.removeprefix("host: ").strip()
            break
    else:
        raise LocalPackageError("rustc did not report its host target")
    if sys.platform == "darwin" and target.endswith("-apple-darwin"):
        return target
    if sys.platform == "win32" and target.endswith("-pc-windows-msvc"):
        return target
    if sys.platform.startswith("linux") and target.endswith("-unknown-linux-gnu"):
        return target
    raise LocalPackageError(f"unsupported local Tauri host target: {target}")


def toolchain_versions() -> dict[str, str]:
    tauri_output = str(run_capture([PNPM, "--dir", "web", "exec", "tauri", "--version"]))
    tauri_lines = [line.strip() for line in tauri_output.splitlines() if line.strip()]
    return {
        "cargo": str(run_capture(["cargo", "--version"])).strip(),
        "node": str(run_capture(["node", "--version"])).strip(),
        "pnpm": str(run_capture([PNPM, "--version"])).strip(),
        "rustc": str(run_capture(["rustc", "--version"])).strip(),
        "tauriCli": tauri_lines[-1] if tauri_lines else "unknown",
    }


def bundle_configuration(target: str) -> tuple[str, list[str]]:
    if target.endswith("-apple-darwin"):
        return "macos-app", ["app"]
    if target.endswith("-pc-windows-msvc"):
        return "windows-nsis", ["nsis"]
    if target.endswith("-unknown-linux-gnu"):
        return "linux-packages", ["appimage", "deb"]
    raise LocalPackageError(f"unsupported local bundle target: {target}")


def install_frontend_dependencies(source_date_epoch: int) -> None:
    environment = os.environ.copy()
    environment["SOURCE_DATE_EPOCH"] = str(source_date_epoch)
    run_visible([PNPM, "install", "--frozen-lockfile"], environment)


def build_native(target: str, source_date_epoch: int) -> Path:
    tauri = REPOSITORY / "web" / "node_modules" / ".bin" / (
        "tauri.cmd" if sys.platform == "win32" else "tauri"
    )
    if not tauri.is_file():
        raise LocalPackageError(
            "Tauri dependencies are missing; run `pnpm --dir web install --frozen-lockfile` first"
        )
    ensure_real_directory(CARGO_TARGET_ROOT, base=REPOSITORY)
    _, bundles = bundle_configuration(target)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(CARGO_TARGET_ROOT)
    environment["SOURCE_DATE_EPOCH"] = str(source_date_epoch)
    environment["CARGO_PROFILE_DEV_DEBUG"] = "0"
    environment["CARGO_PROFILE_DEV_OPT_LEVEL"] = "1"
    environment["CARGO_PROFILE_DEV_STRIP"] = "symbols"
    command = [
        PNPM,
        "--dir",
        "web",
        "tauri",
        "build",
        "--debug",
        "--target",
        target,
        "--bundles",
        ",".join(bundles),
        "--no-sign",
        "--ci",
        "--",
        "--locked",
    ]
    run_visible(command, environment)
    return CARGO_TARGET_ROOT / target / "debug"


def normalized_tar_info(info: tarfile.TarInfo, source_date_epoch: int) -> tarfile.TarInfo:
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = source_date_epoch
    info.pax_headers = {}
    return info


def add_tar_entry(
    archive: tarfile.TarFile,
    source: Path,
    archive_name: str,
    source_date_epoch: int,
) -> None:
    metadata = source.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not (
        stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)
    ):
        raise LocalPackageError(f"macOS app bundle contains an unsafe entry: {source}")
    info = normalized_tar_info(
        archive.gettarinfo(str(source), arcname=archive_name), source_date_epoch
    )
    if stat.S_ISREG(metadata.st_mode):
        with source.open("rb") as contents:
            archive.addfile(info, contents)
    else:
        archive.addfile(info)


def archive_macos_app(source: Path, destination: Path, source_date_epoch: int) -> None:
    try:
        metadata = source.lstat()
    except FileNotFoundError as error:
        raise LocalPackageError(f"Tauri macOS app bundle is missing: {source}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise LocalPackageError(f"Tauri macOS app bundle is not a real directory: {source}")
    with destination.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.GNU_FORMAT) as archive:
                add_tar_entry(archive, source, source.name, source_date_epoch)
                for path in sorted(source.rglob("*"), key=lambda item: item.relative_to(source).as_posix()):
                    relative = path.relative_to(source).as_posix()
                    add_tar_entry(
                        archive,
                        path,
                        f"{source.name}/{relative}",
                        source_date_epoch,
                    )


def require_one(directory: Path, pattern: str, description: str) -> Path:
    matches = sorted(path for path in directory.glob(pattern) if path.is_file())
    if len(matches) != 1:
        raise LocalPackageError(
            f"expected one {description} in {directory}, found {len(matches)}"
        )
    require_regular_file(matches[0], description)
    return matches[0]


def copy_regular(source: Path, destination: Path, *, executable: bool = False) -> None:
    require_regular_file(source, "built native artifact")
    shutil.copyfile(source, destination)
    mode = source.stat().st_mode & 0o777
    destination.chmod(mode if executable else mode & ~0o111)


def collect_artifacts(
    build_root: Path,
    target: str,
    staging: Path,
    source_date_epoch: int,
) -> list[tuple[str, str]]:
    windows = target.endswith("-pc-windows-msvc")
    executable_name = "AudiobookAI.exe" if windows else "AudiobookAI"
    copy_regular(build_root / executable_name, staging / executable_name, executable=True)
    artifacts = [(executable_name, "executable")]
    bundle_root = build_root / "bundle"
    if target.endswith("-apple-darwin"):
        archive_name = "AudiobookAI.app.tar.gz"
        archive_macos_app(
            bundle_root / "macos" / "AudiobookAI.app",
            staging / archive_name,
            source_date_epoch,
        )
        artifacts.append((archive_name, "macos-app-archive"))
    elif windows:
        installer = require_one(bundle_root / "nsis", "*.exe", "NSIS installer")
        copy_regular(installer, staging / "AudiobookAI-installer.exe")
        artifacts.append(("AudiobookAI-installer.exe", "windows-installer"))
    else:
        appimage = require_one(bundle_root / "appimage", "*.AppImage", "AppImage package")
        package = require_one(bundle_root / "deb", "*.deb", "Debian package")
        copy_regular(appimage, staging / "AudiobookAI.AppImage", executable=True)
        copy_regular(package, staging / "AudiobookAI.deb")
        artifacts.extend(
            [
                ("AudiobookAI.AppImage", "linux-appimage"),
                ("AudiobookAI.deb", "linux-deb"),
            ]
        )
    return artifacts


def artifact_records(staging: Path, artifacts: Iterable[tuple[str, str]]) -> list[Artifact]:
    return sorted(
        (
            Artifact(
                name=name,
                kind=kind,
                sha256=sha256_file(staging / name),
                size=(staging / name).stat().st_size,
            )
            for name, kind in artifacts
        ),
        key=lambda item: item.name,
    )


def artifact_set_digest(artifacts: Iterable[Artifact]) -> str:
    digest = hashlib.sha256()
    for artifact in artifacts:
        digest.update(
            f"{artifact.name}\0{artifact.kind}\0{artifact.sha256}\0{artifact.size}\n".encode()
        )
    return digest.hexdigest()


def write_metadata(
    staging: Path,
    *,
    target: str,
    version: str,
    build_id: str,
    source: SourceState,
    tools: dict[str, str],
    artifacts: list[Artifact],
) -> None:
    bundle_kind, _ = bundle_configuration(target)
    immutable_relative = Path("builds") / target / build_id
    manifest = {
        "schemaVersion": 1,
        "productName": "AudiobookAI",
        "version": version,
        "target": target,
        "profile": "optimized-debug",
        "buildId": build_id,
        "bundleKind": bundle_kind,
        "signing": {
            "tauriCodeSigning": "disabled",
            "releaseIdentityConfigured": False,
            "platformAdHocSignaturePossible": target.endswith("-apple-darwin"),
        },
        "publishableRelease": False,
        "developmentRuntime": {
            "systemFfmpegOnPathAllowed": True,
        },
        "source": {
            "gitCommit": source.commit,
            "dirty": source.dirty,
            "digest": source.digest,
            "sourceDateEpoch": source.source_date_epoch,
        },
        "toolchain": dict(sorted(tools.items())),
        "immutableDirectory": immutable_relative.as_posix(),
        "stableDirectory": (Path("current") / target).as_posix(),
        "checksumFile": "SHA256SUMS",
        "artifacts": [
            {
                "file": artifact.name,
                "kind": artifact.kind,
                "sha256": artifact.sha256,
                "size": artifact.size,
            }
            for artifact in artifacts
        ],
    }
    (staging / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    checksum_names = sorted([artifact.name for artifact in artifacts] + ["manifest.json"])
    (staging / "SHA256SUMS").write_text(
        "".join(f"{sha256_file(staging / name)}  {name}\n" for name in checksum_names),
        encoding="utf-8",
    )


def directory_matches(left: Path, right: Path) -> bool:
    left_entries = sorted(left.iterdir(), key=lambda path: path.name)
    right_entries = sorted(right.iterdir(), key=lambda path: path.name)
    if any(
        stat.S_ISLNK(path.lstat().st_mode) or not stat.S_ISREG(path.lstat().st_mode)
        for path in [*left_entries, *right_entries]
    ):
        return False
    left_files = [path.name for path in left_entries]
    right_files = [path.name for path in right_entries]
    return left_files == right_files and all(
        sha256_file(left / name) == sha256_file(right / name) for name in left_files
    )


def atomic_publish_file(source: Path, destination: Path) -> None:
    require_regular_file(source, "immutable local artifact")
    try:
        metadata = destination.lstat()
    except FileNotFoundError:
        pass
    else:
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise LocalPackageError(f"current artifact destination is unsafe: {destination}")
    temporary = destination.with_name(f".{destination.name}.{uuid.uuid4().hex}.tmp")
    try:
        os.link(source, temporary)
    except OSError:
        shutil.copyfile(source, temporary)
        temporary.chmod(source.stat().st_mode & 0o777)
    os.replace(temporary, destination)


def publish_current(snapshot: Path, current: Path, build_id: str) -> None:
    ensure_real_directory(current, base=REPOSITORY)
    names = sorted(path.name for path in snapshot.iterdir() if path.is_file())
    for name in [item for item in names if item not in {"manifest.json", "SHA256SUMS"}]:
        atomic_publish_file(snapshot / name, current / name)
    atomic_publish_file(snapshot / "manifest.json", current / "manifest.json")
    atomic_publish_file(snapshot / "SHA256SUMS", current / "SHA256SUMS")
    pointer = current / "CURRENT"
    try:
        pointer_metadata = pointer.lstat()
    except FileNotFoundError:
        pass
    else:
        if stat.S_ISLNK(pointer_metadata.st_mode) or not stat.S_ISREG(pointer_metadata.st_mode):
            raise LocalPackageError(f"current build pointer is unsafe: {pointer}")
    temporary = current / f".CURRENT.{uuid.uuid4().hex}.tmp"
    temporary.write_text(f"{build_id}\n", encoding="utf-8")
    os.replace(temporary, pointer)


def verify_checksums(directory: Path) -> None:
    checksum_file = directory / "SHA256SUMS"
    require_regular_file(checksum_file, "checksum manifest")
    for line in checksum_file.read_text(encoding="utf-8").splitlines():
        digest, separator, name = line.partition("  ")
        if separator != "  " or len(digest) != 64 or Path(name).name != name:
            raise LocalPackageError(f"invalid checksum entry: {line}")
        require_regular_file(directory / name, "checksummed artifact")
        if sha256_file(directory / name) != digest:
            raise LocalPackageError(f"checksum verification failed: {directory / name}")


def publish_snapshot(staging: Path, target: str, build_id: str) -> tuple[Path, Path]:
    snapshot_parent = OUTPUT_ROOT / "builds" / target
    ensure_real_directory(snapshot_parent, base=REPOSITORY)
    snapshot = snapshot_parent / build_id
    try:
        snapshot_metadata = snapshot.lstat()
    except FileNotFoundError:
        os.replace(staging, snapshot)
    else:
        if (
            stat.S_ISLNK(snapshot_metadata.st_mode)
            or not stat.S_ISDIR(snapshot_metadata.st_mode)
            or not directory_matches(staging, snapshot)
        ):
            raise LocalPackageError(f"immutable local build ID collision: {snapshot}")
    current = OUTPUT_ROOT / "current" / target
    publish_current(snapshot, current, build_id)
    verify_checksums(snapshot)
    verify_checksums(current)
    return snapshot, current


def main() -> int:
    try:
        ensure_real_directory(OUTPUT_ROOT, base=REPOSITORY)
        initial_source = git_source_state()
        target = host_target()
        version = workspace_version()
        install_frontend_dependencies(initial_source.source_date_epoch)
        tools = toolchain_versions()
        print(
            f"Building AudiobookAI {version} for {target} "
            f"from source {initial_source.digest[:12]}"
        )
        build_root = build_native(target, initial_source.source_date_epoch)
        final_source = git_source_state()
        if final_source != initial_source:
            raise LocalPackageError(
                "build inputs changed while the native package was being built; rerun the command"
            )
        with tempfile.TemporaryDirectory(prefix=".staging-", dir=OUTPUT_ROOT) as temporary:
            staging = Path(temporary)
            collected = collect_artifacts(
                build_root,
                target,
                staging,
                initial_source.source_date_epoch,
            )
            artifacts = artifact_records(staging, collected)
            build_id = (
                f"{version}-{initial_source.digest[:12]}-"
                f"{artifact_set_digest(artifacts)[:12]}"
            )
            write_metadata(
                staging,
                target=target,
                version=version,
                build_id=build_id,
                source=initial_source,
                tools=tools,
                artifacts=artifacts,
            )
            snapshot, current = publish_snapshot(staging, target, build_id)
        print(f"Immutable local build: {snapshot}")
        print(f"Current local build:   {current}")
        for artifact in artifacts:
            print(f"Artifact: {current / artifact.name}")
        print(f"Manifest: {current / 'manifest.json'}")
        print(f"Checksums: {current / 'SHA256SUMS'}")
        return 0
    except (OSError, LocalPackageError, tomllib.TOMLDecodeError) as error:
        print(f"local native packaging failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
