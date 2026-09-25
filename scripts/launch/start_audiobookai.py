#!/usr/bin/env python3
"""Start AudiobookAI from a source checkout, building it first when the source changed.

This is what the `Start AudiobookAI.*` files in the repository root run. It reuses the local
native build from `scripts/packaging/build_local_native.py`: when the current sources match the
last build, the app starts immediately; otherwise it is rebuilt first, which takes a few minutes
the first time.
"""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


REPOSITORY = Path(__file__).resolve().parents[2]
PREREQUISITES = {
    "git": "https://git-scm.com/downloads",
    "cargo": "https://rustup.rs",
    "rustc": "https://rustup.rs",
    "node": "https://nodejs.org (version 26)",
    "pnpm": "https://pnpm.io/installation (version 11)",
}


class LaunchError(RuntimeError):
    """A failure the user can act on."""


def load_build_module():
    path = REPOSITORY / "scripts" / "packaging" / "build_local_native.py"
    spec = importlib.util.spec_from_file_location("build_local_native", path)
    if spec is None or spec.loader is None:
        raise LaunchError(f"the build script is missing: {path}")
    module = importlib.util.module_from_spec(spec)
    # Dataclasses resolve their annotations through sys.modules while the module executes.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def check_prerequisites() -> None:
    missing = [
        f"  - {tool}: {hint}"
        for tool, hint in PREREQUISITES.items()
        if shutil.which(tool) is None
    ]
    if shutil.which("ffmpeg") is None or shutil.which("ffprobe") is None:
        print("Note: FFmpeg/ffprobe were not found on PATH. The app starts, but audio")
        print("conversion and export need them (https://ffmpeg.org/download.html).\n")
    if missing:
        raise LaunchError(
            "these tools are required to build AudiobookAI from source:\n" + "\n".join(missing)
        )


def current_build(target: str, source_digest: str) -> Path | None:
    """Returns the up-to-date executable, or None when a rebuild is needed."""
    current = REPOSITORY / "artifacts" / "local-native" / "current" / target
    executable = current / ("AudiobookAI.exe" if sys.platform == "win32" else "AudiobookAI")
    try:
        manifest = json.loads((current / "manifest.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if manifest.get("source", {}).get("digest") != source_digest or not executable.is_file():
        return None
    return executable


def launch(executable: Path) -> None:
    options: dict = {
        "cwd": executable.parent,
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
    }
    if sys.platform == "win32":
        options["creationflags"] = (
            subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP
        )
    else:
        # Keep the app running after the launcher's terminal window closes.
        options["start_new_session"] = True
    subprocess.Popen([str(executable)], **options)  # noqa: S603 - fixed local executable


def run() -> None:
    os.chdir(REPOSITORY)
    check_prerequisites()
    build = load_build_module()
    try:
        target = build.host_target()
        source = build.git_source_state()
    except build.LocalPackageError as error:
        raise LaunchError(str(error)) from error

    executable = current_build(target, source.digest)
    if executable is None:
        print("The source code changed since the last build, so AudiobookAI is being built now.")
        print("The first build takes several minutes; later builds are much faster.\n")
        if build.main() != 0:
            raise LaunchError("the build failed; see the messages above")
        executable = current_build(target, build.git_source_state().digest)
        if executable is None:
            raise LaunchError("the build finished, but no matching app was published")
    else:
        print("AudiobookAI is up to date.")
    launch(executable)
    print("AudiobookAI is starting. You can close this window.")


def main() -> int:
    print("AudiobookAI\n")
    try:
        run()
        return 0
    except (LaunchError, OSError) as error:
        print(f"\nAudiobookAI could not be started: {error}", file=sys.stderr)
        if sys.stdin is not None and sys.stdin.isatty():
            input("\nPress Enter to close this window.")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
