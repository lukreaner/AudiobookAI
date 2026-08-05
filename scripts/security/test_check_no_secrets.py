from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("check_no_secrets.py")
SPEC = importlib.util.spec_from_file_location("check_no_secrets", MODULE_PATH)
assert SPEC and SPEC.loader
scanner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = scanner
SPEC.loader.exec_module(scanner)


class SecretScannerTests(unittest.TestCase):
    def test_rejects_private_keys_without_echoing_material(self) -> None:
        material = b"-----BEGIN " + b"PRIVATE KEY-----\nprivate-material"
        findings = scanner.scan("private.pem", material)
        self.assertEqual(findings[0].rule, "private-key")

    def test_rejects_provider_tokens(self) -> None:
        token = b"sk-" + (b"A7" * 20)
        findings = scanner.scan("config.txt", b"OPENAI_API_KEY=" + token)
        self.assertTrue(any(finding.rule == "openai-style-token" for finding in findings))

    def test_allows_empty_examples_and_explicit_dummy_values(self) -> None:
        content = b"OPENAI_API_KEY=\nTEST_API_KEY=not-a-real-key\n"
        self.assertEqual(scanner.scan(".env.example", content), [])

    def test_dummy_words_inside_real_token_shapes_do_not_bypass_scanning(self) -> None:
        token = b"sk-example" + (b"A7" * 16)
        findings = scanner.scan("config.txt", token)
        self.assertTrue(any(finding.rule == "openai-style-token" for finding in findings))

    def test_rejects_high_entropy_labeled_values(self) -> None:
        content = b"client_" + b'secret="' + b"8dK2mP9qR4sT7vW1xY6z" + b'"'
        findings = scanner.scan("settings.toml", content)
        self.assertTrue(any(finding.rule == "credential-assignment" for finding in findings))

    def test_rejects_labeled_credential_literals_in_source_files(self) -> None:
        content = b"let api_" + b'key = "' + b"9fJ3qM8wR2tV6xB1kN7p" + b'";'
        findings = scanner.scan("src/main.rs", content)
        self.assertTrue(any(finding.rule == "credential-assignment" for finding in findings))

    def test_git_blob_limit_covers_the_largest_normal_github_file(self) -> None:
        self.assertGreaterEqual(scanner.MAX_FILE_BYTES, 100 * 1024 * 1024)

    def test_scans_token_patterns_inside_binary_artifacts(self) -> None:
        token = b"github_pat_" + (b"B7" * 24)
        findings = scanner.scan("artifact:bundle.bin", b"\0binary\0" + token + b"\0")
        self.assertTrue(any(finding.rule == "github-token" for finding in findings))

    def test_rejects_credential_store_filenames_even_when_empty(self) -> None:
        for name in (
            ".envrc",
            ".npmrc",
            ".pypirc",
            ".netrc",
            "_netrc",
            "auth.json",
            "secrets.json",
            "secrets.toml",
            "secrets.yaml",
            "secrets.yml",
        ):
            with self.subTest(name=name):
                findings = scanner.scan(name, b"")
                self.assertTrue(
                    any(finding.rule == "sensitive-file-name" for finding in findings)
                )

    def test_rejects_package_registry_tokens(self) -> None:
        samples = (
            (b"glpat-" + b"A7" * 12, "gitlab-token"),
            (b"npm_" + b"B8" * 18, "npm-token"),
            (b"pypi-" + b"C9" * 18, "pypi-token"),
        )
        for token, expected_rule in samples:
            with self.subTest(rule=expected_rule):
                findings = scanner.scan("artifact.bin", token)
                self.assertTrue(any(finding.rule == expected_rule for finding in findings))

    def test_scans_labeled_high_entropy_values_inside_binary_artifacts(self) -> None:
        content = b"\0binary\0api_key=\"9fJ3qM8wR2tV6xB1kN7p\"\0"
        findings = scanner.scan("artifact:bundle.bin", content)
        self.assertTrue(any(finding.rule == "credential-assignment" for finding in findings))

    def test_rejects_malformed_exact_push_revisions_before_git_is_called(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "malformed Git revision"):
            scanner.revision_files(["HEAD; upload-something"])

    def test_rejects_unlabeled_json_web_tokens(self) -> None:
        value = b"eyJhbGciOiJIUzI1NiJ9." + b"cHJpdmF0ZS1wYXlsb2Fk" + b"." + (b"aB3d" * 10)
        findings = scanner.scan("notes.txt", value)
        self.assertTrue(any(finding.rule == "jwt" for finding in findings))

    def test_refuses_to_skip_oversized_release_artifacts(self) -> None:
        original_limit = scanner.MAX_ARTIFACT_BYTES
        try:
            scanner.MAX_ARTIFACT_BYTES = 1
            with tempfile.TemporaryDirectory() as directory:
                artifact = Path(directory) / "candidate.bin"
                artifact.write_bytes(b"ab")
                with self.assertRaisesRegex(RuntimeError, "exceeds the scan limit"):
                    scanner.explicit_files([str(artifact)])
        finally:
            scanner.MAX_ARTIFACT_BYTES = original_limit


if __name__ == "__main__":
    unittest.main()
