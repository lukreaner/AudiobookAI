from pathlib import Path
import unittest

from scripts.release.validate_workflow import WorkflowPolicyError, validate


WORKFLOW = Path(".github/workflows/release.yml")


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


if __name__ == "__main__":
    unittest.main()
