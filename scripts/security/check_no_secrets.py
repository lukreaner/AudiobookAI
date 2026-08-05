#!/usr/bin/env python3
"""Fail closed when private keys or high-confidence credential formats enter Git."""

from __future__ import annotations

import argparse
import math
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path


MAX_FILE_BYTES = 128 * 1024 * 1024
MAX_ARTIFACT_BYTES = 512 * 1024 * 1024
SAFE_DUMMY_VALUES = frozenset((
    "example",
    "placeholder",
    "not-a-real",
    "not-a-real-key",
    "not_real",
    "dummy",
    "redacted",
    "required_value",
    "changeme",
    "test-password",
))
FORBIDDEN_FILE_NAMES = {
    ".env",
    ".envrc",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "_netrc",
    "auth.json",
    "credentials.json",
    "service-account.json",
    "secrets.json",
    "secrets.toml",
    "secrets.yaml",
    "secrets.yml",
    "id_rsa",
    "id_ed25519",
}
FORBIDDEN_FILE_SUFFIXES = {".p12", ".pfx", ".jks", ".keystore", ".key"}


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    rule: str


TOKEN_RULES: tuple[tuple[str, re.Pattern[bytes]], ...] = (
    ("private-key", re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |PGP )?PRIVATE KEY(?: BLOCK)?-----")),
    ("openai-style-token", re.compile(rb"\bsk-(?:proj-|ant-[A-Za-z0-9_-]+-)?[A-Za-z0-9_-]{20,}\b")),
    ("github-token", re.compile(rb"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})\b")),
    ("gitlab-token", re.compile(rb"\bglpat-[A-Za-z0-9_-]{20,}\b")),
    ("npm-token", re.compile(rb"\bnpm_[A-Za-z0-9]{30,}\b")),
    ("pypi-token", re.compile(rb"\bpypi-[A-Za-z0-9_-]{30,}\b")),
    ("slack-token", re.compile(rb"\bxox[baprs]-[A-Za-z0-9-]{20,}\b")),
    ("aws-access-key", re.compile(rb"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b")),
    ("google-api-key", re.compile(rb"\bAIza[A-Za-z0-9_-]{30,}\b")),
    ("stripe-live-key", re.compile(rb"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}\b")),
    ("hugging-face-token", re.compile(rb"\bhf_[A-Za-z0-9]{30,}\b")),
    (
        "jwt",
        re.compile(rb"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{16,}\b"),
    ),
)

LABELED_QUOTED_VALUE = re.compile(
    rb"(?i)(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key|password)"
    rb"[ \t]*[:=][ \t]*[\"']([A-Za-z0-9_./+=:@-]{12,})[\"']"
)
SOURCE_LABELED_QUOTED_VALUE = re.compile(
    rb"(?i)(?:(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key)"
    rb"[ \t]*[:=]|password[ \t]*=)[ \t]*[\"']([A-Za-z0-9_./+=:@-]{12,})[\"']"
)
ENV_VALUE = re.compile(
    rb"(?im)^[ \t]*(?:export[ \t]+)?[A-Z][A-Z0-9_]*(?:API_KEY|TOKEN|SECRET|PASSWORD|PRIVATE_KEY)"
    rb"[ \t]*=[ \t]*[\"']?([^\r\n#\"']{12,})"
)


def git(*arguments: str, input_bytes: bytes | None = None) -> bytes:
    result = subprocess.run(
        ["git", *arguments],
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.decode("utf-8", "replace").strip() or "git command failed")
    return result.stdout


def current_files() -> list[tuple[str, bytes]]:
    names = git("ls-files", "-co", "--exclude-standard", "-z").split(b"\0")
    files: list[tuple[str, bytes]] = []
    for encoded in names:
        if not encoded:
            continue
        path = Path(encoded.decode("utf-8", "surrogateescape"))
        try:
            if path.is_file() and path.stat().st_size <= MAX_FILE_BYTES:
                files.append((path.as_posix(), path.read_bytes()))
        except OSError:
            continue
    return files


def staged_files() -> list[tuple[str, bytes]]:
    names = git("diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR").split(b"\0")
    files: list[tuple[str, bytes]] = []
    for encoded in names:
        if not encoded:
            continue
        path = encoded.decode("utf-8", "surrogateescape")
        try:
            content = git("show", f":{path}")
        except RuntimeError:
            continue
        if len(content) <= MAX_FILE_BYTES:
            files.append((path, content))
    return files


def reachable_files(revisions: list[str], *, prefix: str) -> list[tuple[str, bytes]]:
    if not revisions:
        return []
    for revision in revisions:
        if not re.fullmatch(r"[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64}|--all", revision):
            raise RuntimeError("refusing to inspect a malformed Git revision")
    objects = git("rev-list", "--objects", *revisions).splitlines()
    files: list[tuple[str, bytes]] = []
    seen: set[str] = set()
    for item in objects:
        parts = item.decode("utf-8", "surrogateescape").split(" ", 1)
        if len(parts) != 2 or parts[0] in seen:
            continue
        object_id, path = parts
        seen.add(object_id)
        kind = git("cat-file", "-t", object_id).strip()
        if kind != b"blob":
            continue
        try:
            size = int(git("cat-file", "-s", object_id))
        except (RuntimeError, ValueError):
            continue
        if size > MAX_FILE_BYTES:
            continue
        files.append((f"{prefix}:{path}", git("cat-file", "blob", object_id)))
    return files


def history_files() -> list[tuple[str, bytes]]:
    return reachable_files(["--all"], prefix="history")


def revision_files(revisions: list[str]) -> list[tuple[str, bytes]]:
    """Return every path-bearing blob reachable from the exact objects being pushed."""

    return reachable_files(revisions, prefix="push")


def explicit_files(roots: list[str]) -> list[tuple[str, bytes]]:
    files: list[tuple[str, bytes]] = []
    for raw_root in roots:
        root = Path(raw_root)
        candidates = [root] if root.is_file() else root.rglob("*") if root.is_dir() else []
        for path in candidates:
            try:
                if path.is_symlink():
                    raise RuntimeError("refusing to upload an artifact tree containing a symbolic link")
                if not path.is_file():
                    continue
                if path.stat().st_size > MAX_ARTIFACT_BYTES:
                    raise RuntimeError("refusing to upload an artifact file that exceeds the scan limit")
                files.append((f"artifact:{path.as_posix()}", path.read_bytes()))
            except OSError:
                raise RuntimeError("could not inspect every release artifact") from None
    return files


def entropy(value: bytes) -> float:
    counts = Counter(value)
    length = len(value)
    return -sum((count / length) * math.log2(count / length) for count in counts.values())


def safe_dummy(value: bytes) -> bool:
    return value.strip().strip(b"\"'").decode("ascii", "ignore").lower() in SAFE_DUMMY_VALUES


def line_number(content: bytes, offset: int) -> int:
    return content.count(b"\n", 0, offset) + 1


def scan(path: str, content: bytes) -> list[Finding]:
    findings: list[Finding] = []
    pure_path = (
        path.removeprefix("history:")
        .removeprefix("push:")
        .removeprefix("artifact:")
    )
    name = Path(pure_path).name.lower()
    suffix = Path(pure_path).suffix.lower()
    if name in FORBIDDEN_FILE_NAMES or suffix in FORBIDDEN_FILE_SUFFIXES:
        findings.append(Finding(path, 1, "sensitive-file-name"))
    for rule, pattern in TOKEN_RULES:
        for match in pattern.finditer(content):
            if not safe_dummy(match.group(0)):
                findings.append(Finding(path, line_number(content, match.start()), rule))
    labeled_patterns: list[re.Pattern[bytes]] = []
    # Credential literals are unsafe in source code just as they are in config files. Avoid
    # interpreting binary blobs as text, but inspect every textual format instead of relying on
    # an extension allowlist that a newly added source or fixture file could bypass.
    if b"\0" not in content:
        if name.startswith(".env") or suffix in {".env", ".json", ".yaml", ".yml", ".toml", ".ini", ".conf"}:
            labeled_patterns.extend((LABELED_QUOTED_VALUE, ENV_VALUE))
        else:
            # UI translation objects often contain labels such as `password: "LAN password"`.
            # In source, keep generic password detection to assignment syntax while retaining
            # both object-key and assignment detection for credential-specific names.
            labeled_patterns.append(SOURCE_LABELED_QUOTED_VALUE)
    else:
        # Release binaries and archives are untrusted input too. Restrict the
        # binary scan to explicit credential labels to avoid interpreting
        # arbitrary machine code as source while still catching embedded keys.
        labeled_patterns.extend((LABELED_QUOTED_VALUE, ENV_VALUE))
    for match in (match for pattern in labeled_patterns for match in pattern.finditer(content)):
        value = match.group(1)
        if safe_dummy(value):
            continue
        if entropy(value) >= 3.2:
            findings.append(Finding(path, line_number(content, match.start()), "credential-assignment"))
    return findings


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--current", action="store_true", help="scan tracked and untracked non-ignored files")
    parser.add_argument("--staged", action="store_true", help="scan the exact staged blobs")
    parser.add_argument("--history", action="store_true", help="scan every reachable Git blob")
    parser.add_argument(
        "--revision",
        action="append",
        default=[],
        help="scan every blob reachable from this exact Git object (repeatable)",
    )
    parser.add_argument("--path", action="append", default=[], help="scan a file or directory before upload; may be repeated")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not (args.current or args.staged or args.history or args.revision or args.path):
        args.current = True
    candidates: list[tuple[str, bytes]] = []
    try:
        if args.current:
            candidates.extend(current_files())
        if args.staged:
            candidates.extend(staged_files())
        if args.history:
            candidates.extend(history_files())
        if args.revision:
            candidates.extend(revision_files(args.revision))
        if args.path:
            candidates.extend(explicit_files(args.path))
    except RuntimeError as error:
        print(f"Secret scan failed closed: {error}", file=sys.stderr)
        return 2
    findings = sorted(
        {finding for path, content in candidates for finding in scan(path, content)},
        key=lambda finding: (finding.path, finding.line, finding.rule),
    )
    if findings:
        print("Secret scan failed. Matched values are intentionally never printed.", file=sys.stderr)
        for finding in findings:
            print(f"{finding.path}:{finding.line}: {finding.rule}", file=sys.stderr)
        return 2
    print(f"Secret scan passed ({len(candidates)} candidate blobs).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
