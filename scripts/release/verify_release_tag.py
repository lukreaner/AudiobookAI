#!/usr/bin/env python3
"""Verify a release tag locally without a network API or authentication token."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys


FINGERPRINT = re.compile(r"^[0-9A-Fa-f]{40}(?:[0-9A-Fa-f]{24})?$")
TAG = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$")


class TagError(RuntimeError):
    """A release-tag validation failure whose message contains no secret material."""


def git(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            ["git", *arguments],
            check=check,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError as error:
        raise TagError("git is required for local release-tag verification") from error
    except subprocess.CalledProcessError as error:
        raise TagError("local release-tag verification failed") from error


def verified_fingerprints(status: str) -> set[str]:
    fingerprints: set[str] = set()
    for line in status.splitlines():
        if not line.startswith("[GNUPG:] VALIDSIG "):
            continue
        for field in line.split()[2:]:
            if FINGERPRINT.fullmatch(field):
                fingerprints.add(field.upper())
    return fingerprints


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--expected-fingerprint", required=True)
    args = parser.parse_args()

    try:
        if not TAG.fullmatch(args.tag):
            raise TagError("release tag does not use the required vMAJOR.MINOR.PATCH form")
        if not FINGERPRINT.fullmatch(args.expected_fingerprint):
            raise TagError("expected release-tag fingerprint is missing or malformed")

        object_type = git("cat-file", "-t", args.tag).stdout.strip()
        if object_type != "tag":
            raise TagError("release tag must be annotated, not lightweight")

        tag_commit = git("rev-parse", f"{args.tag}^{{commit}}").stdout.strip()
        head_commit = git("rev-parse", "HEAD").stdout.strip()
        if tag_commit != head_commit:
            raise TagError("release tag does not resolve to the checked-out commit")

        verification = git("verify-tag", "--raw", args.tag, check=False)
        if verification.returncode != 0:
            raise TagError("release tag does not have a valid locally trusted signature")
        fingerprints = verified_fingerprints(verification.stdout + verification.stderr)
        if args.expected_fingerprint.upper() not in fingerprints:
            raise TagError("release tag was not signed by the configured release identity")
    except TagError as error:
        print(f"release tag gate failed: {error}", file=sys.stderr)
        return 2

    print(f"release tag {args.tag} is locally verified and matches HEAD")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
