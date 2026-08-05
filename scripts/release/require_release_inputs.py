#!/usr/bin/env python3
"""Fail safely when a release-only environment value or input file is absent."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env", action="append", default=[], metavar="NAME")
    parser.add_argument("--file", action="append", default=[], type=Path, metavar="PATH")
    parser.add_argument("--directory", action="append", default=[], type=Path, metavar="PATH")
    args = parser.parse_args()

    missing_environment = [name for name in args.env if not os.environ.get(name)]
    missing_files = [str(path) for path in args.file if not path.is_file()]
    missing_directories = [str(path) for path in args.directory if not path.is_dir()]
    if missing_environment or missing_files or missing_directories:
        print("release inputs are incomplete; no artifact was published", file=sys.stderr)
        if missing_environment:
            print(
                "missing environment names: " + ", ".join(sorted(missing_environment)),
                file=sys.stderr,
            )
        if missing_files:
            print("missing files: " + ", ".join(sorted(missing_files)), file=sys.stderr)
        if missing_directories:
            print(
                "missing directories: " + ", ".join(sorted(missing_directories)),
                file=sys.stderr,
            )
        return 2
    print("release input presence check passed (values were not displayed)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
