#!/usr/bin/env python3
"""Fail closed unless Tauri's updater signer points to protected local material."""

from __future__ import annotations

import os
from pathlib import Path
import stat
import sys


class SignerError(RuntimeError):
    """A signer configuration error that never includes a private value."""


def verify(workspace: Path) -> None:
    key_reference = os.environ.get("TAURI_SIGNING_PRIVATE_KEY", "")
    password = os.environ.get("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "")
    if not key_reference or not password:
        raise SignerError("local updater key reference or unlock value is unavailable")
    if "\n" in key_reference or "\r" in key_reference:
        raise SignerError("updater key must be an absolute local file reference, not key content")

    candidate = Path(key_reference)
    if not candidate.is_absolute():
        raise SignerError("updater key reference must be an absolute local path")
    resolved = candidate.resolve()
    workspace = workspace.resolve()
    if resolved == workspace or workspace in resolved.parents:
        raise SignerError("updater key must remain outside the repository workspace")
    if not resolved.is_file():
        raise SignerError("configured local updater key file is unavailable")

    if os.name != "nt":
        mode = stat.S_IMODE(resolved.stat().st_mode)
        if mode & (stat.S_IRWXG | stat.S_IRWXO):
            raise SignerError("local updater key file must be owner-accessible only")


def main() -> int:
    try:
        verify(Path.cwd())
    except (OSError, SignerError) as error:
        print(f"updater signing setup failed: {error}", file=sys.stderr)
        return 2
    print("local updater signing material is isolated from the workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
