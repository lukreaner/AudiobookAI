from pathlib import Path
import unittest

from scripts.release.validate_workflow import (
    WorkflowPolicyError,
    validate,
    validate_snapshot,
)


WORKFLOW = Path(".github/workflows/release.yml")
SNAPSHOT_WORKFLOW = Path(".github/workflows/snapshot.yml")


class ReleaseWorkflowPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_committed_workflow_has_no_github_secret_or_private_dispatch_input(self) -> None:
        validate(self.workflow)

    def test_rejects_actions_encrypted_secret_expression(self) -> None:
        unsafe = self.workflow.replace(
            "  CARGO_INCREMENTAL: \"0\"",
            "  PRIVATE_VALUE: ${{ secrets.RELEASE_VALUE }}\n  CARGO_INCREMENTAL: \"0\"",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_private_workflow_dispatch_input(self) -> None:
        unsafe = self.workflow.replace(
            "      publish:\n",
            "      signing_password:\n"
            "        description: Forbidden private input\n"
            "        required: true\n"
            "        type: string\n"
            "      publish:\n",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_upload_without_immediately_preceding_scan(self) -> None:
        unsafe = self.workflow.replace(
            "      - name: Secret-scan verified sidecar upload\n",
            "      - name: Validate verified sidecar upload\n",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_stable_tag_run_that_does_not_auto_publish(self) -> None:
        unsafe = self.workflow.replace(
            "if: github.event_name == 'push' || inputs.publish == true",
            "if: github.event_name == 'workflow_dispatch' && inputs.publish == true",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_release_creation_that_is_not_a_draft(self) -> None:
        unsafe = self.workflow.replace("          --draft\n", "", 1)
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_publication_without_remote_byte_verification(self) -> None:
        unsafe = self.workflow.replace(
            "scripts/release/verify_release_upload.py",
            "scripts/release/skip_release_upload_verification.py",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_mutable_downstream_checkout(self) -> None:
        unsafe = self.workflow.replace(
            "ref: ${{ needs.quality.outputs.commit_sha }}",
            "ref: ${{ env.RELEASE_TAG }}",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)

    def test_rejects_unpinned_action(self) -> None:
        unsafe = self.workflow.replace(
            "actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5",
            "actions/checkout@v4",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate(unsafe)


class SnapshotWorkflowPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = SNAPSHOT_WORKFLOW.read_text(encoding="utf-8")

    def test_committed_snapshot_is_github_hosted_and_staged(self) -> None:
        validate_snapshot(self.workflow)

    def test_rejects_snapshot_stable_signer_dependency(self) -> None:
        unsafe = self.workflow.replace(
            "  CARGO_INCREMENTAL: \"0\"",
            "  TAURI_SIGNING_PRIVATE_KEY: local\n  CARGO_INCREMENTAL: \"0\"",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_without_debug_build(self) -> None:
        unsafe = self.workflow.replace("          --debug\n", "", 1)
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_when_native_ci_is_not_required_to_succeed(self) -> None:
        unsafe = self.workflow.replace(
            "github.event.workflow_run.conclusion == 'success'",
            "github.event.workflow_run.conclusion != 'cancelled'",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_failed_ci_cancelling_a_successful_snapshot(self) -> None:
        unsafe = self.workflow.replace(
            "-${{ github.event.workflow_run.conclusion }}",
            "",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_direct_push_trigger_that_bypasses_native_ci(self) -> None:
        unsafe = self.workflow.replace(
            "  workflow_run:\n    workflows: [\"Native CI\"]\n    types: [completed]\n",
            "  push:\n",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_mutable_workflow_run_checkout(self) -> None:
        unsafe = self.workflow.replace(
            "ref: ${{ github.event.workflow_run.head_sha }}",
            "ref: main",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_default_contents_write_permission(self) -> None:
        unsafe = self.workflow.replace(
            "permissions:\n  contents: read",
            "permissions:\n  contents: write",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_publication_before_byte_verification(self) -> None:
        unsafe = self.workflow.replace(
            "scripts/release/verify_release_upload.py",
            "scripts/release/skip_release_upload_verification.py",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_upload_without_immediate_scan(self) -> None:
        unsafe = self.workflow.replace(
            "      - name: Secret-scan development draft asset upload\n",
            "      - name: Inspect development draft asset upload\n",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_without_post_publication_pruning(self) -> None:
        unsafe = self.workflow.replace(
            'gh release delete "${tag}" --cleanup-tag --yes',
            'echo "leaving superseded snapshot ${tag}"',
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)

    def test_rejects_snapshot_pruning_without_preserving_the_current_release(self) -> None:
        unsafe = self.workflow.replace(
            'if test "${tag}" != "${SNAPSHOT_TAG}"; then',
            "if true; then",
            1,
        )
        with self.assertRaises(WorkflowPolicyError):
            validate_snapshot(unsafe)


if __name__ == "__main__":
    unittest.main()
