#!/usr/bin/env python3
"""Validate, fetch, extract, and verify pinned native sidecar bundles."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile


MAX_ARCHIVE_BYTES = 1_073_741_824
MAX_EXPANDED_BYTES = 2_147_483_648
MAX_ENTRIES = 20_000
SHA256 = re.compile(r"^[0-9a-f]{64}$")
PLACEHOLDER = "REQUIRED_"
TARGETS = {
    "universal-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
}
ALLOWED_ROOTS = {"bin", "licenses", "share", "source"}


class SidecarError(RuntimeError):
    """A safe, user-actionable sidecar validation failure."""


def load_manifest(path: Path, *, allow_unresolved: bool) -> dict:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SidecarError(f"cannot read sidecar manifest {path}: {error}") from error

    if manifest.get("schemaVersion") != 1:
        raise SidecarError("sidecar manifest schemaVersion must be 1")
    if set(manifest.get("bundles", {})) != TARGETS:
        raise SidecarError("sidecar manifest must define exactly the three supported targets")
    unresolved: list[str] = []
    validate_source(manifest, unresolved)
    signing_key = manifest.get("sidecarSigningKey", {})
    key_path = signing_key.get("path")
    if key_path != "packaging/sidecar-release-key.asc":
        raise SidecarError("sidecar signing key must use packaging/sidecar-release-key.asc")
    fingerprint = signing_key.get("fingerprint")
    if isinstance(fingerprint, str) and fingerprint.startswith(PLACEHOLDER):
        unresolved.append("sidecar signing fingerprint")
    elif not isinstance(fingerprint, str) or not re.fullmatch(r"[0-9A-Fa-f]{40}", fingerprint):
        raise SidecarError("sidecar signing fingerprint must contain 40 hexadecimal characters")
    elif not allow_unresolved and not Path(key_path).is_file():
        raise SidecarError(f"sidecar signing public key is missing: {key_path}")
    for target, bundle in manifest["bundles"].items():
        archive = bundle.get("archive", {})
        validate_https_url(archive.get("url"), f"{target} archive URL", unresolved)
        validate_digest(archive.get("sha256"), f"{target} archive", unresolved)
        validate_https_url(
            archive.get("signatureUrl"), f"{target} archive signature URL", unresolved
        )
        validate_digest(
            archive.get("signatureSha256"), f"{target} archive signature", unresolved
        )
        if archive.get("format") not in {"zip", "tar.gz"}:
            raise SidecarError(f"{target} archive format must be zip or tar.gz")
        required = bundle.get("requiredFiles")
        if not isinstance(required, list) or not required:
            raise SidecarError(f"{target} must list critical sidecar files")
        seen: set[str] = set()
        for item in required:
            relative = validate_relative_path(item.get("path"), f"{target} file")
            if relative in seen:
                raise SidecarError(f"{target} repeats critical path {relative}")
            seen.add(relative)
            validate_digest(item.get("sha256"), f"{target}:{relative}", unresolved)
            if not isinstance(item.get("executable"), bool):
                raise SidecarError(f"{target}:{relative} must declare executable true or false")

    if not allow_unresolved:
        if not manifest.get("releaseReady"):
            raise SidecarError(
                "sidecar manifest is not release-ready; audit every native bundle and set "
                "releaseReady only after review"
            )
        if unresolved:
            raise SidecarError("unresolved sidecar inputs: " + ", ".join(unresolved))
    return manifest


def validate_source(manifest: dict, unresolved: list[str]) -> None:
    ffmpeg = manifest.get("ffmpeg", {})
    if ffmpeg.get("version") != "8.1.1":
        raise SidecarError("FFmpeg must remain pinned to 8.1.1 for this release line")
    source = ffmpeg.get("source", {})
    validate_https_url(source.get("url"), "FFmpeg source URL", [])
    validate_https_url(source.get("signatureUrl"), "FFmpeg signature URL", [])
    if not SHA256.fullmatch(str(source.get("sha256", ""))):
        raise SidecarError("FFmpeg source SHA-256 is missing or malformed")
    flags = ffmpeg.get("configureFlags", [])
    if "--disable-gpl" not in flags or "--disable-nonfree" not in flags:
        raise SidecarError("FFmpeg must explicitly disable GPL and nonfree configure options")
    if "--enable-gpl" in flags or "--enable-nonfree" in flags:
        raise SidecarError("FFmpeg configure flags enable a forbidden licensing mode")
    lame = manifest.get("libmp3lame", {})
    if lame.get("version") != "3.100":
        raise SidecarError("libmp3lame must remain pinned to 3.100")
    validate_https_url(lame.get("source", {}).get("url"), "libmp3lame source URL", [])
    if not SHA256.fullmatch(str(lame.get("source", {}).get("sha256", ""))):
        raise SidecarError("libmp3lame source SHA-256 is missing or malformed")
    espeak = manifest.get("espeakNg", {}).get("source", {})
    validate_https_url(espeak.get("gitUrl"), "eSpeak NG Git URL", [])
    if not re.fullmatch(r"[0-9a-f]{40}", str(espeak.get("commit", ""))):
        raise SidecarError("eSpeak NG source must use a full 40-character commit")
    uv = manifest.get("uv", {})
    if uv.get("version") != "0.12.1":
        raise SidecarError("uv must remain pinned to 0.12.1 for MLX-audio installation")
    if uv.get("platforms") != ["universal-apple-darwin"]:
        raise SidecarError("uv must be bundled only with the Apple universal sidecar set")
    uv_source = uv.get("source", {})
    validate_https_url(uv_source.get("url"), "uv source URL", unresolved)
    validate_digest(uv_source.get("sha256"), "uv source", unresolved)
    validate_mlx_installer(manifest, unresolved)


def validate_mlx_installer(manifest: dict, unresolved: list[str]) -> None:
    installer = manifest.get("mlxAudioInstaller", {})
    if installer.get("package") != "mlx-audio[tts,server]":
        raise SidecarError("MLX-audio installer must lock the tts and server extras")
    if installer.get("version") != "0.4.6":
        raise SidecarError("MLX-audio installer must remain pinned to 0.4.6")
    if installer.get("target") != "aarch64-apple-darwin":
        raise SidecarError("MLX-audio installer lock must target Apple Silicon macOS")
    expected_layout = {
        "uvExecutable": "bin/uv",
        "pythonExecutable": "share/mlx-audio-installer/python/bin/python3",
        "installerLock": "share/mlx-audio-installer/installer.lock.json",
        "requirementsLock": "share/mlx-audio-installer/requirements.lock",
        "wheelhouse": "share/mlx-audio-installer/wheelhouse",
    }
    if installer.get("bundleLayout") != expected_layout:
        raise SidecarError("MLX-audio installer bundle layout must match the runtime contract")

    unresolved_before = len(unresolved)
    python = installer.get("managedPython", {})
    if python.get("implementation") != "cpython":
        raise SidecarError("MLX-audio managed Python must be CPython")
    python_version = python.get("version")
    if isinstance(python_version, str) and python_version.startswith(PLACEHOLDER):
        unresolved.append("MLX-audio managed Python version")
    elif not isinstance(python_version, str) or not re.fullmatch(
        r"3\.(?:10|11|12|13)\.\d+", python_version
    ):
        raise SidecarError("MLX-audio managed Python must use an exact supported patch version")
    validate_https_url(python.get("url"), "MLX-audio managed Python artifact URL", unresolved)
    validate_digest(python.get("sha256"), "MLX-audio managed Python artifact", unresolved)

    lock = installer.get("artifactLock", {})
    if lock.get("format") != "audiobookai-python-artifact-lock-v1":
        raise SidecarError("MLX-audio artifact lock format is unsupported")
    if lock.get("path") != "packaging/mlx-audio-installer.lock.json":
        raise SidecarError("MLX-audio artifact lock must use the audited packaging path")
    validate_digest(lock.get("sha256"), "MLX-audio artifact lock", unresolved)
    artifact_count = lock.get("artifactCount")
    if isinstance(artifact_count, str) and artifact_count.startswith(PLACEHOLDER):
        unresolved.append("MLX-audio artifact count")
    elif not isinstance(artifact_count, int) or isinstance(artifact_count, bool) or artifact_count < 1:
        raise SidecarError("MLX-audio artifact lock must declare a positive artifact count")
    closure = lock.get("completeTransitiveClosure")
    if closure is False:
        unresolved.append("MLX-audio complete transitive artifact closure")
    elif closure is not True:
        raise SidecarError("MLX-audio transitive-closure flag must be true or unresolved false")

    if len(unresolved) != unresolved_before:
        return
    lock_path = Path(lock["path"])
    if not lock_path.is_file():
        raise SidecarError(f"MLX-audio artifact lock is missing: {lock_path}")
    if sha256_file(lock_path) != lock["sha256"]:
        raise SidecarError("MLX-audio artifact lock SHA-256 does not match its manifest")
    validate_mlx_artifact_lock(
        lock_path,
        python_version=python_version,
        expected_count=artifact_count,
    )


def validate_mlx_artifact_lock(
    path: Path, *, python_version: str, expected_count: int
) -> None:
    try:
        lock = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SidecarError("cannot read the MLX-audio artifact lock") from error
    if lock.get("schemaVersion") != 1:
        raise SidecarError("MLX-audio artifact lock schemaVersion must be 1")
    if lock.get("package") != "mlx-audio[tts,server]" or lock.get("version") != "0.4.6":
        raise SidecarError("MLX-audio artifact lock package identity is invalid")
    if lock.get("target") != "aarch64-apple-darwin":
        raise SidecarError("MLX-audio artifact lock target is invalid")
    if lock.get("pythonVersion") != python_version:
        raise SidecarError("MLX-audio artifact lock Python version does not match")
    if lock.get("completeTransitiveClosure") is not True:
        raise SidecarError("MLX-audio artifact lock must attest a complete transitive closure")
    artifacts = lock.get("artifacts")
    if not isinstance(artifacts, list) or len(artifacts) != expected_count:
        raise SidecarError("MLX-audio artifact lock count does not match the manifest")
    seen: set[tuple[str, str, str]] = set()
    selected: dict[str, str] = {}
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            raise SidecarError("MLX-audio artifact lock entries must be objects")
        package = artifact.get("package")
        version = artifact.get("version")
        filename = artifact.get("filename")
        if not isinstance(package, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", package):
            raise SidecarError("MLX-audio artifact lock package name is invalid")
        if not isinstance(version, str) or not version or any(char.isspace() for char in version):
            raise SidecarError("MLX-audio artifact lock version is invalid")
        if (
            not isinstance(filename, str)
            or not filename
            or filename != PurePosixPath(filename).name
            or filename in {".", ".."}
        ):
            raise SidecarError("MLX-audio artifact filename is unsafe")
        identity = (package.lower().replace("_", "-"), version, filename)
        if identity in seen:
            raise SidecarError("MLX-audio artifact lock repeats an artifact")
        seen.add(identity)
        normalized = package.lower().replace("_", "-").replace(".", "-")
        if normalized in selected:
            raise SidecarError("MLX-audio artifact lock selects multiple artifacts for one package")
        if not filename.lower().endswith(".whl"):
            raise SidecarError("MLX-audio runtime artifacts must be binary wheels, not source archives")
        selected[normalized] = version
        artifact_unresolved: list[str] = []
        validate_https_url(artifact.get("url"), "MLX-audio artifact URL", artifact_unresolved)
        validate_digest(artifact.get("sha256"), "MLX-audio artifact", artifact_unresolved)
        if artifact_unresolved:
            raise SidecarError("MLX-audio artifact lock contains unresolved artifacts")
        if PurePosixPath(urllib.parse.urlsplit(artifact["url"]).path).name != filename:
            raise SidecarError("MLX-audio artifact URL does not match its filename")
    if selected.get("mlx-audio") != "0.4.6":
        raise SidecarError("MLX-audio artifact lock must contain the exact 0.4.6 wheel")


def validate_https_url(value: object, label: str, unresolved: list[str]) -> None:
    if isinstance(value, str) and value.startswith(PLACEHOLDER):
        unresolved.append(label)
        return
    if not isinstance(value, str):
        raise SidecarError(f"{label} is missing")
    parsed = urllib.parse.urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
    ):
        raise SidecarError(
            f"{label} must be a public HTTPS URL without credentials, query, or fragment"
        )


def validate_digest(value: object, label: str, unresolved: list[str]) -> None:
    if isinstance(value, str) and value.startswith(PLACEHOLDER):
        unresolved.append(label)
    elif not isinstance(value, str) or not SHA256.fullmatch(value):
        raise SidecarError(f"{label} SHA-256 must be 64 lowercase hexadecimal characters")


def validate_relative_path(value: object, label: str) -> str:
    if not isinstance(value, str) or not value or "\\" in value:
        raise SidecarError(f"{label} path must use non-empty POSIX syntax")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or "." in path.parts:
        raise SidecarError(f"{label} path is unsafe: {value}")
    if not path.parts or path.parts[0] not in ALLOWED_ROOTS:
        raise SidecarError(f"{label} path must be rooted in one of {sorted(ALLOWED_ROOTS)}")
    return path.as_posix()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fetch_archive(url: str, destination: Path) -> None:
    headers = {"User-Agent": "AudiobookAI-release-builder/1"}
    request = urllib.request.Request(url, headers=headers)
    opener = urllib.request.build_opener()
    written = 0
    try:
        with opener.open(request, timeout=120) as response, destination.open("wb") as out:
            while chunk := response.read(1024 * 1024):
                written += len(chunk)
                if written > MAX_ARCHIVE_BYTES:
                    raise SidecarError("sidecar archive exceeds the 1 GiB release limit")
                out.write(chunk)
    except SidecarError:
        raise
    except Exception as error:
        raise SidecarError(f"sidecar archive download failed: {error}") from error


def verify_signature(manifest: dict, signature: Path, archive: Path, home: Path) -> None:
    key_path = Path(manifest["sidecarSigningKey"]["path"])
    expected = manifest["sidecarSigningKey"]["fingerprint"].upper()
    home.mkdir(mode=0o700)
    try:
        subprocess.run(
            ["gpg", "--batch", "--quiet", "--homedir", str(home), "--import", str(key_path)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        result = subprocess.run(
            [
                "gpg",
                "--batch",
                "--homedir",
                str(home),
                "--status-fd",
                "1",
                "--verify",
                str(signature),
                str(archive),
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError as error:
        raise SidecarError("GnuPG is required to verify sidecar bundles") from error
    except subprocess.CalledProcessError as error:
        raise SidecarError("sidecar archive OpenPGP signature verification failed") from error
    fingerprints = [
        line.split()[2].upper()
        for line in result.stdout.splitlines()
        if line.startswith("[GNUPG:] VALIDSIG ") and len(line.split()) > 2
    ]
    if expected not in fingerprints:
        raise SidecarError("sidecar signature was not made by the pinned signing key")


def safe_destination(root: Path, name: str) -> Path:
    relative = validate_relative_path(name.rstrip("/"), "archive entry")
    destination = root.joinpath(*PurePosixPath(relative).parts)
    resolved_root = root.resolve()
    resolved_parent = destination.parent.resolve()
    if resolved_root != resolved_parent and resolved_root not in resolved_parent.parents:
        raise SidecarError(f"archive entry escapes extraction root: {name}")
    return destination


def extract_zip(archive: Path, root: Path) -> None:
    expanded = 0
    with zipfile.ZipFile(archive) as bundle:
        entries = bundle.infolist()
        if len(entries) > MAX_ENTRIES:
            raise SidecarError("sidecar archive contains too many entries")
        for item in entries:
            unix_mode = item.external_attr >> 16
            if stat.S_ISLNK(unix_mode):
                raise SidecarError(f"sidecar archive contains a symbolic link: {item.filename}")
            destination = safe_destination(root, item.filename)
            if item.is_dir():
                destination.mkdir(parents=True, exist_ok=True)
                continue
            expanded += item.file_size
            if expanded > MAX_EXPANDED_BYTES:
                raise SidecarError("expanded sidecar archive exceeds the 2 GiB release limit")
            destination.parent.mkdir(parents=True, exist_ok=True)
            with bundle.open(item) as source, destination.open("wb") as output:
                shutil.copyfileobj(source, output, 1024 * 1024)


def extract_tar(archive: Path, root: Path) -> None:
    expanded = 0
    with tarfile.open(archive, "r:gz") as bundle:
        entries = bundle.getmembers()
        if len(entries) > MAX_ENTRIES:
            raise SidecarError("sidecar archive contains too many entries")
        for item in entries:
            if not (item.isfile() or item.isdir()):
                raise SidecarError(f"sidecar archive contains a non-file entry: {item.name}")
            destination = safe_destination(root, item.name)
            if item.isdir():
                destination.mkdir(parents=True, exist_ok=True)
                continue
            expanded += item.size
            if expanded > MAX_EXPANDED_BYTES:
                raise SidecarError("expanded sidecar archive exceeds the 2 GiB release limit")
            source = bundle.extractfile(item)
            if source is None:
                raise SidecarError(f"could not read archive entry {item.name}")
            destination.parent.mkdir(parents=True, exist_ok=True)
            with source, destination.open("wb") as output:
                shutil.copyfileobj(source, output, 1024 * 1024)


def verify_contained_entry(root: Path, path: Path, *, directory: bool) -> Path:
    root_metadata = root.lstat()
    if not stat.S_ISDIR(root_metadata.st_mode) or stat.S_ISLNK(root_metadata.st_mode):
        raise SidecarError("verified sidecar root must be a non-symlink directory")
    canonical_root = root.resolve(strict=True)
    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise SidecarError("sidecar entry escapes the verified root") from error
    current = root
    try:
        for component in relative.parts:
            current = current / component
            metadata = current.lstat()
            if stat.S_ISLNK(metadata.st_mode):
                raise SidecarError("sidecar entry contains a symbolic-link component")
        metadata = path.lstat()
        canonical_path = path.resolve(strict=True)
    except OSError as error:
        raise SidecarError("sidecar entry is missing") from error
    expected = stat.S_ISDIR(metadata.st_mode) if directory else stat.S_ISREG(metadata.st_mode)
    if not expected or not canonical_path.is_relative_to(canonical_root):
        raise SidecarError("sidecar entry has an unsafe file type or location")
    return canonical_path


def verify_tree(manifest: dict, target: str, root: Path) -> None:
    if target not in manifest["bundles"]:
        raise SidecarError(f"unknown sidecar target {target}")
    canonical_root = verify_contained_entry(root, root, directory=True)
    for item in manifest["bundles"][target]["requiredFiles"]:
        path = root.joinpath(*PurePosixPath(item["path"]).parts)
        try:
            verify_contained_entry(root, path, directory=False)
        except SidecarError as error:
            raise SidecarError(f"required sidecar file is missing: {item['path']}") from error
        actual = sha256_file(path)
        if actual != item["sha256"]:
            raise SidecarError(f"sidecar checksum mismatch: {item['path']}")
        if item["executable"] and os.name != "nt":
            path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    if target == "universal-apple-darwin":
        verify_mlx_installer_payload(manifest, root, canonical_root)


def verify_mlx_installer_payload(manifest: dict, root: Path, canonical_root: Path) -> None:
    installer = manifest["mlxAudioInstaller"]
    layout = installer["bundleLayout"]
    lock_path = root.joinpath(*PurePosixPath(layout["installerLock"]).parts)
    if sha256_file(lock_path) != installer["artifactLock"]["sha256"]:
        raise SidecarError("bundled MLX-audio installer lock does not match the audited lock")
    validate_mlx_artifact_lock(
        lock_path,
        python_version=installer["managedPython"]["version"],
        expected_count=installer["artifactLock"]["artifactCount"],
    )
    lock = json.loads(lock_path.read_text(encoding="utf-8"))
    expected = {artifact["filename"]: artifact["sha256"] for artifact in lock["artifacts"]}
    wheelhouse = root.joinpath(*PurePosixPath(layout["wheelhouse"]).parts)
    try:
        verify_contained_entry(root, wheelhouse, directory=True)
    except SidecarError as error:
        raise SidecarError("bundled MLX-audio wheelhouse is missing") from error
    actual: set[str] = set()
    for wheel in wheelhouse.iterdir():
        try:
            verify_contained_entry(root, wheel, directory=False)
        except SidecarError as error:
            raise SidecarError("bundled MLX-audio wheelhouse contains a non-regular entry")
        if not wheel.resolve(strict=True).is_relative_to(canonical_root):
            raise SidecarError("bundled MLX-audio wheel escapes the verified sidecar tree")
        expected_hash = expected.get(wheel.name)
        if expected_hash is None:
            raise SidecarError("bundled MLX-audio wheelhouse contains an unlocked artifact")
        if sha256_file(wheel) != expected_hash:
            raise SidecarError("bundled MLX-audio wheel checksum mismatch")
        actual.add(wheel.name)
    if actual != set(expected):
        raise SidecarError("bundled MLX-audio wheelhouse is incomplete")


def fetch(
    manifest_path: Path,
    target: str,
    output: Path,
    supplied_archive: Path | None,
    supplied_signature: Path | None,
) -> None:
    manifest = load_manifest(manifest_path, allow_unresolved=False)
    if output.exists():
        raise SidecarError(f"refusing to replace existing sidecar directory {output}")
    bundle = manifest["bundles"].get(target)
    if bundle is None:
        raise SidecarError(f"unknown sidecar target {target}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="audiobookai-sidecars-", dir=output.parent) as temp_name:
        temporary = Path(temp_name)
        archive = temporary / ("bundle.zip" if bundle["archive"]["format"] == "zip" else "bundle.tar.gz")
        signature = temporary / "bundle.sig"
        if supplied_archive:
            if not supplied_archive.is_file():
                raise SidecarError(f"supplied sidecar archive does not exist: {supplied_archive}")
            shutil.copyfile(supplied_archive, archive)
        else:
            fetch_archive(bundle["archive"]["url"], archive)
        if supplied_signature:
            if not supplied_signature.is_file():
                raise SidecarError(
                    f"supplied sidecar signature does not exist: {supplied_signature}"
                )
            shutil.copyfile(supplied_signature, signature)
        else:
            fetch_archive(bundle["archive"]["signatureUrl"], signature)
        if sha256_file(archive) != bundle["archive"]["sha256"]:
            raise SidecarError(f"{target} sidecar archive checksum mismatch")
        if sha256_file(signature) != bundle["archive"]["signatureSha256"]:
            raise SidecarError(f"{target} sidecar archive signature checksum mismatch")
        verify_signature(manifest, signature, archive, temporary / "gnupg")
        extracted = temporary / "extracted"
        extracted.mkdir()
        if bundle["archive"]["format"] == "zip":
            extract_zip(archive, extracted)
        else:
            extract_tar(archive, extracted)
        verify_tree(manifest, target, extracted)
        provenance = {
            "schemaVersion": 1,
            "target": target,
            "archiveSha256": bundle["archive"]["sha256"],
            "archiveSignatureSha256": bundle["archive"]["signatureSha256"],
            "sidecarSigningFingerprint": manifest["sidecarSigningKey"]["fingerprint"],
            "ffmpegSourceSha256": manifest["ffmpeg"]["source"]["sha256"],
            "espeakCommit": manifest["espeakNg"]["source"]["commit"]
            if target in manifest["espeakNg"]["platforms"]
            else None,
            "uvSourceSha256": manifest["uv"]["source"]["sha256"]
            if target in manifest["uv"]["platforms"]
            else None,
        }
        (extracted / "sidecar-provenance.json").write_text(
            json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        extracted.rename(output)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest", type=Path, default=Path("packaging/sidecars.lock.json")
    )
    commands = parser.add_subparsers(dest="command", required=True)
    manifest = commands.add_parser("manifest", help="validate the lockfile schema")
    manifest.add_argument("--allow-unresolved", action="store_true")
    verify = commands.add_parser("verify", help="verify an already extracted target")
    verify.add_argument("target", choices=sorted(TARGETS))
    verify.add_argument("root", type=Path)
    fetch_command = commands.add_parser("fetch", help="fetch and verify a target bundle")
    fetch_command.add_argument("target", choices=sorted(TARGETS))
    fetch_command.add_argument("output", type=Path)
    fetch_command.add_argument("--archive", type=Path)
    fetch_command.add_argument("--signature", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "manifest":
            load_manifest(args.manifest, allow_unresolved=args.allow_unresolved)
        elif args.command == "verify":
            manifest = load_manifest(args.manifest, allow_unresolved=False)
            verify_tree(manifest, args.target, args.root)
        elif args.command == "fetch":
            fetch(
                args.manifest,
                args.target,
                args.output,
                args.archive,
                args.signature,
            )
        else:
            raise SidecarError(f"unsupported command {args.command}")
    except SidecarError as error:
        print(f"release input error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
