#!/usr/bin/env python3
"""Enforce the credential-free GitHub and local-signing release boundary."""

from __future__ import annotations

import re
from pathlib import Path
import sys


WORKFLOW = Path(".github/workflows/release.yml")
LEGACY_PRIVATE_INPUTS = (
    "AUDIOBOOKAI_SIDECAR_TOKEN",
    "WINDOWS_CERTIFICATE_BASE64",
    "WINDOWS_CERTIFICATE_PASSWORD",
    "APPLE_CERTIFICATE_BASE64",
    "APPLE_CERTIFICATE_PASSWORD",
    "APPLE_ID",
    "APPLE_PASSWORD",
    "APPLE_TEAM_ID",
    "RELEASE_GPG_PRIVATE_KEY",
)
LOCAL_PRIVATE_ENVIRONMENT = (
    "TAURI_SIGNING_PRIVATE_KEY",
    "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
)
PRIVATE_INPUT_NAME = re.compile(
    r"(?:secret|password|passphrase|token|private[_-]?key|certificate|credential)",
    re.IGNORECASE,
)
STEP = re.compile(
    r"(?ms)^      - name: (?P<name>[^\n]+)\n(?P<body>.*?)(?=^      - name: |\Z)"
)


class WorkflowPolicyError(RuntimeError):
    pass


def job_section(text: str, name: str, next_name: str | None) -> str:
    marker = f"\n  {name}:\n"
    if marker not in text:
        raise WorkflowPolicyError(f"release workflow is missing the {name} job")
    section = text.split(marker, maxsplit=1)[1]
    if next_name:
        next_marker = f"\n  {next_name}:\n"
        if next_marker not in section:
            raise WorkflowPolicyError(f"release workflow is missing the {next_name} job")
        section = section.split(next_marker, maxsplit=1)[0]
    return section


def validate(text: str) -> None:
    if re.search(r"\$\{\{\s*secrets\.", text, re.IGNORECASE):
        raise WorkflowPolicyError("GitHub Actions encrypted secrets are forbidden")

    for name in LEGACY_PRIVATE_INPUTS:
        if name in text:
            raise WorkflowPolicyError(f"legacy private release input is forbidden: {name}")

    for name in LOCAL_PRIVATE_ENVIRONMENT:
        assignment = re.compile(
            rf"(?m)^\s*{re.escape(name)}\s*:\s*\$\{{\{{.*$"
        )
        if assignment.search(text):
            raise WorkflowPolicyError(
                f"{name} must come only from the owner-controlled runner environment"
            )

    dispatch = text.split("  workflow_dispatch:\n", maxsplit=1)[1].split(
        "\npermissions:", maxsplit=1
    )[0]
    dispatch_inputs = re.findall(r"(?m)^      ([A-Za-z0-9_-]+):\s*$", dispatch)
    for input_name in dispatch_inputs:
        if PRIVATE_INPUT_NAME.search(input_name):
            raise WorkflowPolicyError(
                f"workflow_dispatch must not accept private material: {input_name}"
            )
    if set(dispatch_inputs) != {"tag", "publish"}:
        raise WorkflowPolicyError("workflow_dispatch may accept only tag and publish")

    authorized = job_section(text, "release-authorized", "verified-sidecars")
    native = job_section(text, "native-package", "aggregate")
    aggregate = job_section(text, "aggregate", "publish")
    for name, section in (
        ("release-authorized", authorized),
        ("native-package", native),
        ("aggregate", aggregate),
    ):
        if "runs-on: [self-hosted, audiobookai-release," not in section:
            raise WorkflowPolicyError(
                f"{name} must run on an explicitly labelled owner-controlled host"
            )

    steps = list(STEP.finditer(text))
    if not steps:
        raise WorkflowPolicyError("release workflow contains no inspectable steps")
    for index, step in enumerate(steps):
        body = step.group("body")
        is_upload = "uses: actions/upload-artifact@" in body
        is_attestation = "uses: actions/attest-build-provenance@" in body
        is_release = "gh release create" in body
        if not (is_upload or is_attestation or is_release):
            continue
        if index == 0 or not steps[index - 1].group("name").startswith("Secret-scan "):
            raise WorkflowPolicyError(
                f"external publication step lacks an immediate secret scan: {step.group('name')}"
            )

    publish = job_section(text, "publish", None)
    checkout = publish.find("uses: actions/checkout@")
    download = publish.find("uses: actions/download-artifact@")
    if checkout < 0 or download < 0 or checkout > download:
        raise WorkflowPolicyError(
            "publish must check out the immutable scanner before downloading a candidate"
        )


def main() -> int:
    try:
        validate(WORKFLOW.read_text(encoding="utf-8"))
    except (OSError, IndexError, WorkflowPolicyError) as error:
        print(f"release workflow policy failed: {error}", file=sys.stderr)
        return 2
    print("release workflow keeps private release material off GitHub")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
