#!/usr/bin/env python3
"""Unit tests for the isolated cross-repository incident-matrix verifier."""

from __future__ import annotations

import importlib.util
import shutil
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("verify_parent_provenance_rollback_matrix.py")
SPEC = importlib.util.spec_from_file_location("incident_matrix", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matrix)


class IncidentMatrixVerifierTests(unittest.TestCase):
    def test_isolated_environment_removes_every_provenance_variable(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            original = matrix.os.environ.copy()
            try:
                for name in matrix.PROVENANCE_ENV:
                    matrix.os.environ[name] = "must-not-escape"
                env = matrix.isolated_lterm_env(root)
            finally:
                matrix.os.environ.clear()
                matrix.os.environ.update(original)

            for name in matrix.PROVENANCE_ENV:
                self.assertNotIn(name, env)
            self.assertEqual(env["LTERM_RUNTIME_DIR"], str(root / "run"))
            self.assertEqual(env["LTERM_DATA_DIR"], str(root / "data"))
            self.assertEqual(env["TMPDIR"], str(root / "tmp"))
            self.assertEqual(env["TMUX_TMPDIR"], str(root / "tmux"))
            for directory in ("home", "run", "data", "tmp", "tmux"):
                self.assertEqual((root / directory).stat().st_mode & 0o777, 0o700)

    def test_row_lookup_fails_closed_when_fixture_identity_is_absent(self) -> None:
        with self.assertRaisesRegex(matrix.MatrixFailure, "missing"):
            matrix.row_named([{"name": "synthetic-parent"}], "missing")

    def test_omx_patterns_cover_stale_multi_row_owner_and_signal_boundaries(self) -> None:
        self.assertIn("multi-row", matrix.OMX_TMUX_TEST_PATTERN)
        self.assertIn("attempt-owned", matrix.OMX_TMUX_TEST_PATTERN)
        self.assertIn("PID", matrix.OMX_TMUX_TEST_PATTERN)
        self.assertIn("ownership", matrix.OMX_RUNTIME_TEST_PATTERN)
        self.assertIn("startup direct", matrix.OMX_RUNTIME_TEST_PATTERN)

    def test_omx_environment_cannot_inherit_active_team_state(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            root = Path(raw_root)
            original = matrix.os.environ.copy()
            try:
                matrix.os.environ["OMX_TEAM_WORKER"] = "active/worker-3"
                matrix.os.environ["OMX_TEAM_STATE_ROOT"] = "/live/team/state"
                matrix.os.environ["OMX_SESSION_ID"] = "live-session"
                matrix.os.environ["TMUX_PANE"] = "%production"
                matrix.os.environ["TMUX_TMPDIR"] = "/live/tmux"
                env = matrix.isolated_omx_env(root)
            finally:
                matrix.os.environ.clear()
                matrix.os.environ.update(original)

            for name in (
                "OMX_TEAM_WORKER",
                "OMX_TEAM_STATE_ROOT",
                "OMX_SESSION_ID",
                "TMUX_PANE",
            ):
                self.assertNotIn(name, env)
            self.assertEqual(env["TMUX_TMPDIR"], str(root / "tmux"))
            self.assertEqual((root / "tmux").stat().st_mode & 0o777, 0o700)

    def test_private_tmux_boundaries_are_unique_and_explicit(self) -> None:
        with (
            tempfile.TemporaryDirectory() as first,
            tempfile.TemporaryDirectory() as second,
        ):
            first_socket = matrix.private_tmux_socket(Path(first))
            second_socket = matrix.private_tmux_socket(Path(second))
            self.assertNotEqual(first_socket, second_socket)
            self.assertTrue(first_socket.is_absolute())
            self.assertEqual(first_socket.parent.stat().st_mode & 0o777, 0o700)
            command = matrix.tmux_args(
                Path("/opt/private/tmux"), first_socket, "list-panes"
            )
            self.assertEqual(
                command[1:5], ["-S", str(first_socket), "-f", "/dev/null"]
            )

    def test_tmux_command_rejects_non_matrix_socket(self) -> None:
        with self.assertRaisesRegex(matrix.MatrixFailure, "private matrix socket"):
            matrix.tmux_args(Path("/usr/bin/tmux"), Path("/tmp/default"), "ls")

    def test_compatibility_cells_are_explicit_and_only_fixed_fixed_executes(self) -> None:
        cells = matrix.compatibility_cells(fixed_lterm=True, fixed_omx=True)
        self.assertEqual(
            [cell["id"] for cell in cells],
            ["old|old", "old|fixed", "fixed|old", "fixed|fixed"],
        )
        self.assertEqual(
            [cell["id"] for cell in cells if cell["execution"] == "executed"],
            ["fixed|fixed"],
        )

    @unittest.skipUnless(shutil.which("tmux"), "real tmux is unavailable")
    def test_real_tmux_force_rollback_observes_residue_policy(self) -> None:
        with tempfile.TemporaryDirectory() as raw_root:
            report = matrix.verify_real_tmux_force_rollback(
                Path(shutil.which("tmux")), Path(raw_root)
            )
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(
            report["coverage_scope"], "pane_primitive_not_startTeam_runtime"
        )
        self.assertEqual(report["owner_validation"], "exact_match_before_kill_pane")
        self.assertEqual(report["foreground_descendant"], "terminated_with_pane")
        self.assertEqual(
            report["escaped_descendant"],
            "allowed_residue_then_exact_pid_cleaned",
        )
        self.assertEqual(len(report["preserved_panes"]), 2)


if __name__ == "__main__":
    unittest.main()
