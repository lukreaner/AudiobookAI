#!/usr/bin/env python3
"""Enforce the credential-free GitHub and local-signing release boundary."""

from __future__ import annotations

import re
from pathlib import Path
import sys


WORKFLOW = Path(".github/workflows/release.yml")
SNAPSHOT_WORKFLOW = Path(".github/workflows/snapshot.yml")
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
ACTION = re.compile(r"(?m)^\s*uses:\s*[^\s@]+@([^\s#]+)")


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


def require_pinned_actions(text: str) -> None:
    revisions = ACTION.findall(text)
    if not revisions:
        raise WorkflowPolicyError("workflow contains no third-party actions")
    for revision in revisions:
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise WorkflowPolicyError(
                f"third-party action is not pinned to a full commit: {revision}"
            )


def require_scans_before_publication(text: str) -> None:
    steps = list(STEP.finditer(text))
    if not steps:
        raise WorkflowPolicyError("workflow contains no inspectable steps")
    for index, step in enumerate(steps):
        body = step.group("body")
        is_upload = "uses: actions/upload-artifact@" in body
        is_attestation = "uses: actions/attest-build-provenance@" in body
        is_release = bool(
            re.search(r"gh release (?:create|upload)", body)
            or re.search(r"gh release edit .*--draft=false", body)
        )
        if not (is_upload or is_attestation or is_release):
            continue
        if index == 0 or not steps[index - 1].group("name").startswith("Secret-scan "):
            raise WorkflowPolicyError(
                f"external publication step lacks an immediate secret scan: {step.group('name')}"
            )


def validate(text: str) -> None:
    if re.search(r"\$\{\{\s*secrets\.", text, re.IGNORECASE):
        raise WorkflowPolicyError("GitHub Actions encrypted secrets are forbidden")

    require_pinned_actions(text)

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

    require_scans_before_publication(text)

    publish = job_section(text, "publish", None)
    if "if: github.event_name == 'push' || inputs.publish == true" not in publish:
        raise WorkflowPolicyError(
            "stable tag pushes must publish automatically after the aggregate gate"
        )
    if "needs: [quality, aggregate]" not in publish:
        raise WorkflowPolicyError("stable publication must depend on quality and aggregate")
    if "needs: [quality, release-authorized, native-package]" not in aggregate:
        raise WorkflowPolicyError("stable aggregate must wait for the complete native matrix")
    checkout = publish.find("uses: actions/checkout@")
    download = publish.find("uses: actions/download-artifact@")
    if checkout < 0 or download < 0 or checkout > download:
        raise WorkflowPolicyError(
            "publish must check out the immutable scanner before downloading a candidate"
        )
    if "ref: ${{ needs.quality.outputs.commit_sha }}" not in publish:
        raise WorkflowPolicyError("publish must check out the quality-gated commit SHA")
    if text.count("ref: ${{ needs.quality.outputs.commit_sha }}") != 5:
        raise WorkflowPolicyError("every downstream stable job must use the quality-gated commit SHA")

    create = publish.find('gh release create "${RELEASE_TAG}"')
    upload = publish.find('gh release upload "${RELEASE_TAG}"')
    verify = publish.find("scripts/release/verify_release_upload.py")
    make_public = publish.find('gh release edit "${RELEASE_TAG}" --draft=false')
    if min(create, upload, verify, make_public) < 0 or not (
        create < upload < verify < make_public
    ):
        raise WorkflowPolicyError(
            "stable release must be drafted, uploaded, byte-verified, then published"
        )
    create_step = publish[create:upload]
    if "--draft" not in create_step or "--verify-tag" not in create_step:
        raise WorkflowPolicyError("stable release creation must be an unpublished verified-tag draft")
    if "release/dist/*" in create_step:
        raise WorkflowPolicyError("stable assets must not be attached during release creation")


def validate_snapshot(text: str) -> None:
    if re.search(r"\$\{\{\s*secrets\.", text, re.IGNORECASE):
        raise WorkflowPolicyError("snapshot workflow must not use encrypted secrets")
    require_pinned_actions(text)
    require_scans_before_publication(text)

    header = text.split("\njobs:", maxsplit=1)[0]
    if "permissions:\n  contents: read" not in header:
        raise WorkflowPolicyError("snapshot workflow default token must be contents-read only")

    trigger = text.split("on:\n", maxsplit=1)[1].split("\npermissions:", maxsplit=1)[0]
    if (
        'workflows: ["Native CI"]' not in trigger
        or "types: [completed]" not in trigger
        or "branches: [main]" not in trigger
    ):
        raise WorkflowPolicyError(
            "snapshot workflow must follow completed Native CI runs for main"
        )
    if re.search(r"(?m)^  (?:push|pull_request|workflow_dispatch):", trigger):
        raise WorkflowPolicyError(
            "snapshot workflow must not bypass the Native CI completion trigger"
        )
    required_gate = (
        "github.event.workflow_run.conclusion == 'success'",
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.head_branch == 'main'",
        "github.event.workflow_run.head_repository.full_name == github.repository",
        "ref: ${{ github.event.workflow_run.head_sha }}",
    )
    for expression in required_gate:
        if expression not in text:
            raise WorkflowPolicyError(
                f"snapshot workflow is missing immutable Native CI gate: {expression}"
            )
    if (
        "rolling-development-snapshot-${{ github.event.workflow_run.event }}-"
        "${{ github.event.workflow_run.head_branch }}-"
        "${{ github.event.workflow_run.conclusion }}"
        not in header
    ):
        raise WorkflowPolicyError(
            "unsuccessful Native CI runs must not cancel successful snapshot work"
        )
    if "self-hosted" in text:
        raise WorkflowPolicyError("development snapshots must use only GitHub-hosted runners")
    for runner in ("windows-2022", "macos-15", "ubuntu-22.04"):
        if runner not in text:
            raise WorkflowPolicyError(f"snapshot matrix is missing GitHub-hosted runner {runner}")
    for target in (
        "x86_64-pc-windows-msvc",
        "universal-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ):
        if target not in text:
            raise WorkflowPolicyError(f"snapshot matrix is missing target {target}")
    if "--debug" not in text or "prepare_snapshot_config.py" not in text:
        raise WorkflowPolicyError("snapshot installers must use the isolated debug overlay")
    forbidden = (
        "prepare_tauri_config.py",
        "TAURI_SIGNING_PRIVATE_KEY",
        "RELEASE_GPG_KEY_ID",
        "APPLE_SIGNING_IDENTITY",
        "WINDOWS_CERTIFICATE_THUMBPRINT",
        "verified-sidecars",
    )
    for value in forbidden:
        if value in text:
            raise WorkflowPolicyError(
                f"development snapshot must remain independent of stable release input: {value}"
            )

    native = job_section(text, "native-package", "aggregate")
    aggregate = job_section(text, "aggregate", "publish")
    publish = job_section(text, "publish", None)
    if "permissions:\n      contents: write" not in publish:
        raise WorkflowPolicyError("only snapshot publication may receive contents-write")
    if "needs: quality" not in native:
        raise WorkflowPolicyError("snapshot native matrix must depend on quality")
    if "needs: [quality, native-package]" not in aggregate:
        raise WorkflowPolicyError("snapshot aggregate must wait for every native target")
    if "needs: [quality, aggregate]" not in publish:
        raise WorkflowPolicyError("snapshot publication must wait for aggregate verification")
    checksum = aggregate.find("scripts/release/generate_checksums.py")
    matrix_gate = aggregate.find("scripts/release/verify_snapshot_artifacts.py")
    candidate_upload = aggregate.find("Upload complete development release candidate")
    if min(checksum, matrix_gate, candidate_upload) < 0 or not (
        matrix_gate < checksum < candidate_upload
    ):
        raise WorkflowPolicyError(
            "snapshot checksums and candidate upload must follow the complete matrix gate"
        )

    checkout = publish.find("uses: actions/checkout@")
    download = publish.find("uses: actions/download-artifact@")
    create = publish.find('gh release create "${SNAPSHOT_TAG}"')
    upload = publish.find('gh release upload "${SNAPSHOT_TAG}"')
    verify = publish.find("scripts/release/verify_release_upload.py")
    make_public = publish.find('gh release edit "${SNAPSHOT_TAG}" --draft=false')
    if checkout < 0 or download < 0 or checkout > download:
        raise WorkflowPolicyError(
            "snapshot publish must check out the immutable scanner before candidate download"
        )
    if min(create, upload, verify, make_public) < 0 or not (
        create < upload < verify < make_public
    ):
        raise WorkflowPolicyError(
            "snapshot release must be drafted, uploaded, byte-verified, then published"
        )
    create_step = publish[create:upload]
    if "--draft" not in create_step or "--prerelease" not in create_step:
        raise WorkflowPolicyError("snapshot release must begin as a development prerelease draft")
    if "snapshot/dist/*" in create_step:
        raise WorkflowPolicyError("snapshot assets must not be attached during release creation")
    if "DEVELOPMENT-BUILD.txt" not in text or "development-snapshot-" not in text:
        raise WorkflowPolicyError("snapshot artifacts and tag must be clearly development-labelled")


def main() -> int:
    try:
        validate(WORKFLOW.read_text(encoding="utf-8"))
        validate_snapshot(SNAPSHOT_WORKFLOW.read_text(encoding="utf-8"))
    except (OSError, IndexError, WorkflowPolicyError) as error:
        print(f"release workflow policy failed: {error}", file=sys.stderr)
        return 2
    print("stable and development workflows enforce staged, credential-safe publication")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
