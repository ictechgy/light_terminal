#!/usr/bin/env python3
from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "quad_brainstorming_manifest_summary.py"


def write_manifest(path: Path, **overrides: object) -> None:
    manifest: dict[str, object] = {
        "schema_version": "1",
        "session_id": path.parent.name,
        "created_at": "2026-05-19T00:00:00Z",
        "preset": "architecture-review",
        "context_summary": {
            "sources": ["brief"],
            "raw_bytes": 100,
            "redacted_bytes": 80,
            "excluded_sensitive_paths": 0,
        },
        "redaction": {"status": "ok", "redactor": "review_core", "report_path": None},
        "provider_statuses": [
            {"provider": "claude", "configured": True, "runnable": False, "status": "auth-required", "detail": "login needed"},
            {"provider": "codex", "configured": True, "runnable": True, "status": "usable", "detail": "local"},
            {"provider": "gemini", "configured": True, "runnable": False, "status": "unsafe-mode", "detail": "no safe mode"},
            {"provider": "forge", "configured": False, "runnable": False, "status": "missing", "detail": "not installed"},
        ],
        "quorum": "solo-degraded",
        "confidence_cap": "low",
        "persistence_policy": "redacted-artifacts",
        "artifact_policy": {"saved": ["adr.md", "manifest.json"], "raw_persistence": "off"},
        "telemetry": {"status": "disabled", "transport": "none"},
    }
    manifest.update(overrides)
    path.write_text(json.dumps(manifest), encoding="utf-8")


class QuadBrainstormingManifestSummaryTests(unittest.TestCase):
    def test_summarizes_local_manifests_without_leaking_raw_fields(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            write_manifest(run / "manifest.json", raw_context="SECRET_TOKEN_SHOULD_NOT_PRINT")

            proc = subprocess.run(
                ["python3", str(SCRIPT), "--json", str(root)],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertNotIn("SECRET_TOKEN_SHOULD_NOT_PRINT", proc.stdout)
            summary = json.loads(proc.stdout)
            self.assertEqual(summary["manifest_count"], 1)
            self.assertEqual(summary["preset_counts"], {"architecture-review": 1})
            self.assertEqual(summary["quorum_counts"], {"solo-degraded": 1})
            self.assertEqual(summary["saved_adr_count"], 1)
            self.assertEqual(summary["telemetry_counts"], {"disabled": 1})
            self.assertEqual(summary["raw_fields_ignored"], {"raw_context": 1})
            self.assertEqual(summary["provider_status_counts"]["codex"], {"usable": 1})
            self.assertEqual(summary["skipped_provider_reasons"]["claude:auth-required"], 1)

    def test_malformed_nested_values_are_unknown_without_leaking_raw_values(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            secret = "SECRET_TOKEN_SHOULD_NOT_PRINT"
            write_manifest(
                run / "manifest.json",
                preset={"raw_context": secret},
                quorum=[secret],
                confidence_cap={"transcript": secret},
                telemetry={"status": {"raw_provider_output": secret}, "transport": "none"},
                provider_statuses=[
                    {
                        "provider": {"raw_context": secret},
                        "configured": True,
                        "runnable": False,
                        "status": {"raw_provider_output": secret},
                        "detail": "malformed",
                    }
                ],
            )

            proc = subprocess.run(
                ["python3", str(SCRIPT), "--json", str(root)],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertNotIn(secret, proc.stdout)
            summary = json.loads(proc.stdout)
            self.assertEqual(summary["preset_counts"], {"unknown": 1})
            self.assertEqual(summary["quorum_counts"], {"unknown": 1})
            self.assertEqual(summary["confidence_counts"], {"unknown": 1})
            self.assertEqual(summary["telemetry_counts"], {"unknown": 1})
            self.assertEqual(summary["provider_status_counts"]["unknown"], {"unknown": 1})
            self.assertEqual(summary["skipped_provider_reasons"], {"unknown:unknown": 1})

    def test_explicit_missing_path_returns_error(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            missing = Path(tmp) / "missing"
            proc = subprocess.run(
                ["python3", str(SCRIPT), str(missing)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertEqual(proc.returncode, 1)
            self.assertIn("errors\t1", proc.stdout)
            self.assertIn("path does not exist", proc.stderr)

    def test_invalid_utf8_manifest_reports_error(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            (run / "manifest.json").write_bytes(b"\xff\xfe\xfa")

            proc = subprocess.run(
                ["python3", str(SCRIPT), "--json", str(root)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertEqual(proc.returncode, 1)
            summary = json.loads(proc.stdout)
            self.assertEqual(summary["manifest_count"], 0)
            self.assertEqual(summary["error_count"], 1)
            self.assertEqual(summary["invalid_manifest_count"], 1)
            self.assertIn("manifest.json", summary["errors"][0])

    def test_missing_required_fields_are_invalid_not_valid_manifests(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            (run / "manifest.json").write_text(json.dumps({"schema_version": "1"}), encoding="utf-8")

            proc = subprocess.run(
                ["python3", str(SCRIPT), "--json", str(root)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertEqual(proc.returncode, 1)
            summary = json.loads(proc.stdout)
            self.assertEqual(summary["manifest_count"], 0)
            self.assertEqual(summary["invalid_manifest_count"], 1)
            self.assertGreater(summary["error_count"], 0)
            self.assertTrue(any("missing required field 'session_id'" in error for error in summary["errors"]))

    def test_nested_raw_fields_are_counted_without_values_or_ancestor_keys(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            secret = "SECRET_TOKEN_SHOULD_NOT_PRINT"
            malicious_key = f"ancestor-{secret}"
            write_manifest(
                run / "manifest.json",
                context_summary={
                    "sources": ["brief"],
                    "raw_bytes": 100,
                    "redacted_bytes": 80,
                    "excluded_sensitive_paths": 0,
                    "nested": {malicious_key: {"raw_provider_stdout": secret}},
                },
                provider_statuses=[
                    {
                        "provider": "codex",
                        "configured": True,
                        "runnable": True,
                        "status": "usable",
                        "detail": {malicious_key: {"transcript": secret}},
                    }
                ],
            )

            proc = subprocess.run(
                ["python3", str(SCRIPT), "--json", str(root)],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertNotIn(secret, proc.stdout)
            summary = json.loads(proc.stdout)
            self.assertEqual(summary["manifest_count"], 1)
            self.assertEqual(
                summary["raw_fields_ignored"],
                {
                    "raw_provider_stdout": 1,
                    "transcript": 1,
                },
            )

    def test_text_output_is_stable_and_local(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-brainstorm-summary-test-") as tmp:
            root = Path(tmp)
            run = root / "run-1"
            run.mkdir()
            write_manifest(run / "manifest.json", preset="risk-scan", quorum="partial", confidence_cap="medium")

            proc = subprocess.run(
                ["python3", str(SCRIPT), str(root)],
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=REPO_ROOT,
            )
            self.assertIn("manifests\t1", proc.stdout)
            self.assertIn("saved_adr_count\t1", proc.stdout)
            self.assertIn("risk-scan\t1", proc.stdout)
            self.assertIn("partial\t1", proc.stdout)
            self.assertIn("codex\tusable:1", proc.stdout)
            self.assertEqual(proc.stderr, "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
