#!/usr/bin/env python3
"""Run the parent-provenance/rollback incident matrix without live-state contact.

The lterm scenario always creates its own runtime/data directories and daemon.
The OMX scenario runs pre-built mock-tmux Node tests from a caller-supplied
source worktree, then a real forced rollback on a unique private tmux socket.
It never launches Team or contacts lterm/tmux production sessions.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


PROVENANCE_ENV = (
    "LTERM_PANE",
    "LTERM_PARENT_TOKEN",
    "LTERM_SOCKET",
    "TMUX",
    "TMUX_PANE",
)
OMX_TMUX_TEST_PATTERN = (
    "rejects multi-row and mismatched pane output|"
    "kills only exact attempt-owned panes|"
    "keeps the shared rollback primitive free of PID"
)
OMX_RUNTIME_TEST_PATTERN = (
    "startTeam fails closed on drifted pane ownership|"
    "startTeam rejects startup direct trigger success"
)
OMX_INHERITED_STATE_ENV = (
    "OMX_SESSION_ID",
    "OMX_TEAM_WORKER",
    "TMUX",
    "TMUX_PANE",
    "TMUX_TMPDIR",
)
COMPATIBILITY_CELLS = (
    {
        "id": "old|old",
        "lterm": "old",
        "omx": "old",
        "policy": "historical_unsafe_baseline_not_executed",
    },
    {
        "id": "old|fixed",
        "lterm": "old",
        "omx": "fixed",
        "policy": "rollback_safe_but_lost_parent_may_remain",
    },
    {
        "id": "fixed|old",
        "lterm": "fixed",
        "omx": "old",
        "policy": "parent_safe_but_destructive_rollback_cell_prohibited",
    },
    {
        "id": "fixed|fixed",
        "lterm": "fixed",
        "omx": "fixed",
        "policy": "executable_disposable_gate",
    },
)


class MatrixFailure(RuntimeError):
    """A fixture or safety assertion failed."""


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: float = 20,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        args,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        rendered = " ".join(shlex.quote(part) for part in args)
        raise MatrixFailure(
            f"command failed ({result.returncode}): {rendered}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def isolated_lterm_env(root: Path) -> dict[str, str]:
    env = os.environ.copy()
    for name in PROVENANCE_ENV:
        env.pop(name, None)
    env.update(
        {
            "HOME": str(root / "home"),
            "LTERM_RUNTIME_DIR": str(root / "run"),
            "LTERM_DATA_DIR": str(root / "data"),
            "TMPDIR": str(root / "tmp"),
            "TMUX_TMPDIR": str(root / "tmux"),
        }
    )
    for directory in ("home", "run", "data", "tmp", "tmux"):
        path = root / directory
        path.mkdir(parents=True, exist_ok=True)
        path.chmod(0o700)
    return env


def isolated_omx_env(root: Path | None = None) -> dict[str, str]:
    env = os.environ.copy()
    for name in tuple(env):
        if name.startswith("OMX_TEAM_") or name in OMX_INHERITED_STATE_ENV:
            env.pop(name, None)
    if root is not None:
        tmux_tmp = root / "tmux"
        tmux_tmp.mkdir(parents=True, exist_ok=True)
        tmux_tmp.chmod(0o700)
        env["TMUX_TMPDIR"] = str(tmux_tmp)
    return env


def private_tmux_socket(root: Path) -> Path:
    tmux_root = root / "tmux"
    tmux_root.mkdir(parents=True, exist_ok=True)
    tmux_root.chmod(0o700)
    socket_path = tmux_root / "matrix.sock"
    if not socket_path.is_absolute() or socket_path == Path("/tmp/tmux-default"):
        raise MatrixFailure(f"unsafe tmux socket path: {socket_path}")
    return socket_path


def tmux_args(binary: Path, socket_path: Path, *args: str) -> list[str]:
    if not socket_path.is_absolute() or socket_path.name != "matrix.sock":
        raise MatrixFailure(
            f"tmux command requires the private matrix socket: {socket_path}"
        )
    return [str(binary), "-S", str(socket_path), "-f", "/dev/null", *args]


def wait_for_file(path: Path, label: str) -> int:
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        try:
            value = int(path.read_text(encoding="utf-8").strip())
            if value > 1:
                return value
        except (FileNotFoundError, ValueError):
            pass
        time.sleep(0.05)
    raise MatrixFailure(f"timed out waiting for {label} pid artifact: {path}")


def process_identity(pid: int) -> str | None:
    result = subprocess.run(
        ["/bin/ps", "-o", "pid=,pgid=,lstart=,command=", "-p", str(pid)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    identity = result.stdout.strip()
    return identity or None


def wait_for_identity_gone(pid: int, identity: str, timeout: float = 5) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process_identity(pid) != identity:
            return True
        time.sleep(0.05)
    return process_identity(pid) != identity


def compatibility_cells(*, fixed_lterm: bool, fixed_omx: bool) -> list[dict[str, str]]:
    cells: list[dict[str, str]] = []
    for template in COMPATIBILITY_CELLS:
        cell = dict(template)
        cell["execution"] = (
            "executed"
            if cell["id"] == "fixed|fixed" and fixed_lterm and fixed_omx
            else "policy_only"
        )
        cells.append(cell)
    return cells


def lterm_json(binary: Path, env: dict[str, str], args: list[str]) -> Any:
    result = run([str(binary), *args], env=env)
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise MatrixFailure(
            f"lterm {' '.join(args)} returned non-JSON output: {result.stdout!r}"
        ) from error


def wait_for_daemon(binary: Path, env: dict[str, str]) -> None:
    deadline = time.monotonic() + 10
    last = "daemon did not answer"
    while time.monotonic() < deadline:
        try:
            report = lterm_json(binary, env, ["doctor", "--json"])
            if report.get("daemon_reachable") is True:
                return
            last = json.dumps(report, sort_keys=True)
        except (MatrixFailure, subprocess.TimeoutExpired) as error:
            last = str(error)
        time.sleep(0.05)
    raise MatrixFailure(f"isolated daemon failed to become ready: {last}")


def wait_for_sessions(
    binary: Path,
    env: dict[str, str],
    expected_names: set[str],
) -> list[dict[str, Any]]:
    deadline = time.monotonic() + 12
    last: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        try:
            rows = lterm_json(binary, env, ["ls", "--all", "--json"])
            if isinstance(rows, list):
                last = rows
                names = {row.get("name") for row in rows if isinstance(row, dict)}
                if expected_names <= names:
                    return rows
        except (MatrixFailure, subprocess.TimeoutExpired):
            pass
        time.sleep(0.05)
    raise MatrixFailure(
        "timed out waiting for disposable sessions "
        f"{sorted(expected_names)}; last_names="
        f"{sorted(str(row.get('name')) for row in last if isinstance(row, dict))}"
    )


def row_named(rows: list[dict[str, Any]], name: str) -> dict[str, Any]:
    for row in rows:
        if row.get("name") == name:
            return row
    raise MatrixFailure(f"session {name!r} missing from {rows!r}")


def verify_lterm(binary: Path) -> dict[str, Any]:
    binary = binary.resolve()
    if not binary.is_file():
        raise MatrixFailure(f"lterm binary is missing: {binary}")

    binary_version = run([str(binary), "--version"]).stdout.strip()

    with tempfile.TemporaryDirectory(prefix="lterm-incident-matrix-") as raw_root:
        root = Path(raw_root)
        env = isolated_lterm_env(root)
        daemon = subprocess.Popen(
            [str(binary), "daemon"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_for_daemon(binary, env)
            parent_name = "matrix-parent"
            child_name = "matrix-child"
            outside_name = "matrix-outside-control"
            launcher = root / "fully-stripped-launcher.sh"
            unset_args = " ".join(f"-u {name}" for name in PROVENANCE_ENV)
            launcher.write_text(
                "#!/bin/sh\n"
                "set -eu\n"
                f"env {unset_args} {shlex.quote(str(binary))} new --detach "
                f"-n {child_name} -- sh -lc 'sleep 30'\n"
                "sleep 30\n",
                encoding="utf-8",
            )
            launcher.chmod(0o700)

            run(
                [
                    str(binary),
                    "new",
                    "--tmux",
                    "--detach",
                    "-n",
                    parent_name,
                    "--",
                    str(launcher),
                ],
                env=env,
            )
            rows = wait_for_sessions(binary, env, {parent_name, child_name})
            parent = row_named(rows, parent_name)
            child = row_named(rows, child_name)
            if child.get("parent_pane_id") != parent.get("pane_id"):
                raise MatrixFailure(
                    "lost-parent regression: fully stripped descendant was not linked to "
                    "the nearest session; "
                    f"parent_pane={parent.get('pane_id')!r}, "
                    f"child_parent_pane={child.get('parent_pane_id')!r}"
                )
            if child.get("parent_session_id") != parent.get("id"):
                raise MatrixFailure(
                    "lost-parent regression: child parent_session_id does not match the "
                    "parent birth/session identity"
                )

            outside_env = env.copy()
            for name in PROVENANCE_ENV:
                outside_env.pop(name, None)
            run(
                [
                    str(binary),
                    "new",
                    "--detach",
                    "-n",
                    outside_name,
                    "--",
                    "sh",
                    "-lc",
                    "sleep 30",
                ],
                env=outside_env,
            )
            rows = wait_for_sessions(
                binary, env, {parent_name, child_name, outside_name}
            )
            outside = row_named(rows, outside_name)
            if outside.get("parent_pane_id") is not None:
                raise MatrixFailure(
                    "outside control acquired an implicit parent pane"
                )
            if outside.get("parent_session_id") is not None:
                raise MatrixFailure(
                    "outside control acquired an implicit parent session"
                )

            roots = lterm_json(binary, env, ["ls", "--json"])
            root_names = {row.get("name") for row in roots}
            if child_name in root_names or parent_name not in root_names:
                raise MatrixFailure(
                    f"root inventory is inconsistent: {sorted(str(name) for name in root_names)}"
                )
            children = lterm_json(binary, env, ["ls", "--children", "--json"])
            child_names = {row.get("name") for row in children}
            if child_name not in child_names or outside_name in child_names:
                raise MatrixFailure(
                    "child inventory is inconsistent: "
                    f"{sorted(str(name) for name in child_names)}"
                )

            return {
                "status": "PASS",
                "fixture": "disposable_lterm_daemon",
                "binary_version": binary_version,
                "provenance_env_removed": list(PROVENANCE_ENV),
                "lost_parent": "nearest_parent_recovered",
                "outside_control": "remained_root",
                "production_contact": False,
            }
        finally:
            try:
                subprocess.run(
                    [str(binary), "shutdown"],
                    env=env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=3,
                    check=False,
                )
            except subprocess.TimeoutExpired:
                pass
            try:
                daemon.wait(timeout=3)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)


def verify_real_tmux_force_rollback(
    tmux_binary: Path,
    root: Path,
) -> dict[str, Any]:
    tmux_binary = tmux_binary.resolve()
    if not tmux_binary.is_file():
        raise MatrixFailure(f"tmux binary is missing: {tmux_binary}")

    socket_path = private_tmux_socket(root)
    session_name = "matrix"
    owner_token = f"matrix-attempt-{os.getpid()}-{time.monotonic_ns()}"
    foreground_path = root / "foreground.pid"
    escaped_path = root / "escaped.pid"
    launcher = root / "pane-processes.py"
    launcher.write_text(
        "#!/usr/bin/env python3\n"
        "import os, subprocess\n"
        f"escaped = subprocess.Popen(['sleep', '30'], start_new_session=True, "
        "stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, "
        "stderr=subprocess.DEVNULL)\n"
        f"open({str(escaped_path)!r}, 'w').write(str(escaped.pid))\n"
        f"open({str(foreground_path)!r}, 'w').write(str(os.getpid()))\n"
        "os.execvp('sleep', ['sleep', '30'])\n",
        encoding="utf-8",
    )
    launcher.chmod(0o700)

    env = isolated_omx_env(root)
    escaped_pid: int | None = None
    escaped_identity: str | None = None
    try:
        run(
            tmux_args(
                tmux_binary,
                socket_path,
                "new-session",
                "-d",
                "-s",
                session_name,
                "-n",
                "leader",
                "sleep 30",
            ),
            env=env,
        )
        leader_pane = run(
            tmux_args(
                tmux_binary,
                socket_path,
                "display-message",
                "-p",
                "-t",
                f"{session_name}:leader.0",
                "#{pane_id}",
            ),
            env=env,
        ).stdout.strip()
        attempt_pane = run(
            tmux_args(
                tmux_binary,
                socket_path,
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                leader_pane,
                str(launcher),
            ),
            env=env,
        ).stdout.strip()
        control_pane = run(
            tmux_args(
                tmux_binary,
                socket_path,
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                leader_pane,
                "sleep 30",
            ),
            env=env,
        ).stdout.strip()
        run(
            tmux_args(
                tmux_binary,
                socket_path,
                "set-option",
                "-p",
                "-t",
                attempt_pane,
                "@omx_attempt_id",
                owner_token,
            ),
            env=env,
        )

        foreground_pid = wait_for_file(foreground_path, "foreground")
        escaped_pid = wait_for_file(escaped_path, "escaped")
        foreground_identity = process_identity(foreground_pid)
        escaped_identity = process_identity(escaped_pid)
        if foreground_identity is None or escaped_identity is None:
            raise MatrixFailure("fixture process identity disappeared before rollback")

        observed_owner = run(
            tmux_args(
                tmux_binary,
                socket_path,
                "display-message",
                "-p",
                "-t",
                attempt_pane,
                "#{@omx_attempt_id}",
            ),
            env=env,
        ).stdout.strip()
        if observed_owner != owner_token:
            raise MatrixFailure(
                f"attempt pane owner mismatch: {observed_owner!r} != {owner_token!r}"
            )

        # This is the real forced rollback boundary: after exact owner validation,
        # delete only the attempt pane on the explicitly named private socket.
        run(
            tmux_args(tmux_binary, socket_path, "kill-pane", "-t", attempt_pane),
            env=env,
        )
        panes = set(
            run(
                tmux_args(
                    tmux_binary,
                    socket_path,
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_id}",
                ),
                env=env,
            ).stdout.splitlines()
        )
        if attempt_pane in panes or not {leader_pane, control_pane} <= panes:
            raise MatrixFailure(
                "forced rollback did not remove only the attempt-owned pane; "
                f"attempt={attempt_pane!r}, survivors={sorted(panes)!r}"
            )
        if not wait_for_identity_gone(foreground_pid, foreground_identity):
            raise MatrixFailure("pane foreground process survived forced rollback")
        if process_identity(escaped_pid) != escaped_identity:
            raise MatrixFailure(
                "intentionally escaped descendant did not survive long enough "
                "to validate policy"
            )

        return {
            "status": "PASS",
            "fixture": "real_tmux_private_socket_forced_rollback",
            "coverage_scope": "pane_primitive_not_startTeam_runtime",
            "socket_scope": "unique_private_absolute_socket",
            "attempt_pane": attempt_pane,
            "preserved_panes": [leader_pane, control_pane],
            "owner_validation": "exact_match_before_kill_pane",
            "foreground_descendant": "terminated_with_pane",
            "escaped_descendant": "allowed_residue_then_exact_pid_cleaned",
            "production_contact": False,
        }
    finally:
        if escaped_pid is not None and escaped_identity is not None:
            if process_identity(escaped_pid) == escaped_identity:
                try:
                    os.kill(escaped_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                if not wait_for_identity_gone(escaped_pid, escaped_identity, timeout=2):
                    if process_identity(escaped_pid) == escaped_identity:
                        try:
                            os.kill(escaped_pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        wait_for_identity_gone(escaped_pid, escaped_identity, timeout=2)
        try:
            subprocess.run(
                tmux_args(tmux_binary, socket_path, "kill-server"),
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=3,
                check=False,
            )
        except subprocess.TimeoutExpired:
            pass


def verify_omx(omx_repo: Path, tmux_binary: Path) -> dict[str, Any]:
    omx_repo = omx_repo.resolve()
    source_commit = run(
        ["git", "rev-parse", "HEAD"], cwd=omx_repo, timeout=10
    ).stdout.strip()
    package = json.loads((omx_repo / "package.json").read_text(encoding="utf-8"))
    tests = [
        (
            omx_repo / "dist/team/__tests__/tmux-session.test.js",
            OMX_TMUX_TEST_PATTERN,
            (
                "rejects multi-row and mismatched pane output",
                "kills only exact attempt-owned panes",
                "keeps the shared rollback primitive free of PID",
            ),
        ),
        (
            omx_repo / "dist/team/__tests__/runtime.test.js",
            OMX_RUNTIME_TEST_PATTERN,
            ("startTeam",),
        ),
    ]
    for test_file, _, _ in tests:
        if not test_file.is_file():
            raise MatrixFailure(
                f"pre-built OMX test is missing: {test_file}; build the source worktree first"
            )

    with tempfile.TemporaryDirectory(prefix="omx-incident-matrix-") as raw_root:
        root = Path(raw_root)
        outputs: list[dict[str, str]] = []
        for test_file, pattern, expected_names in tests:
            result = run(
                [
                    "node",
                    "--test",
                    f"--test-name-pattern={pattern}",
                    str(test_file),
                ],
                cwd=omx_repo,
                env=isolated_omx_env(root),
                timeout=120,
            )
            if "# pass 0" in result.stdout:
                raise MatrixFailure(
                    f"OMX pattern selected no tests in {test_file}: {pattern!r}"
                )
            for expected_name in expected_names:
                if expected_name not in result.stdout:
                    raise MatrixFailure(
                        f"OMX pattern did not execute {expected_name!r} in {test_file}"
                    )
            outputs.append(
                {
                    "test": str(test_file.relative_to(omx_repo)),
                    "pattern": pattern,
                }
            )
        real_tmux = verify_real_tmux_force_rollback(tmux_binary, root)

    return {
        "status": "PASS",
        "fixture": "mock_tmux_and_intercepted_process_signal",
        "source_commit": source_commit,
        "package_version": package.get("version"),
        "checks": outputs,
        "real_tmux": real_tmux,
        "rollback_evidence": {
            "startTeam_runtime": "prebuilt_injected_failure_tests",
            "real_tmux": "pane_primitive_only",
        },
        "stale_session": "owner_mismatch_failed_closed",
        "multi_row_pid": "rejected",
        "rollback_target": real_tmux["attempt_pane"],
        "destructive_signal_targets": [],
        "protected_panes_absent_from_signal_targets": real_tmux["preserved_panes"],
        "production_contact": False,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lterm-bin", type=Path)
    parser.add_argument("--omx-repo", type=Path)
    parser.add_argument(
        "--tmux-bin",
        type=Path,
        default=Path(found) if (found := shutil.which("tmux")) else None,
        help="real tmux binary used only with an explicit private -S socket",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="optional path for the JSON result (stdout is always emitted)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.lterm_bin is None and args.omx_repo is None:
        print("at least one of --lterm-bin or --omx-repo is required", file=sys.stderr)
        return 2

    report: dict[str, Any] = {
        "schema_version": 1,
        "column_order": ["lterm", "omx"],
        "isolation": "disposable_only",
        "production_contact": False,
        "compatibility_cells": compatibility_cells(
            fixed_lterm=args.lterm_bin is not None,
            fixed_omx=args.omx_repo is not None,
        ),
        "results": {},
    }
    try:
        if args.lterm_bin is not None:
            report["results"]["lterm"] = verify_lterm(args.lterm_bin)
        if args.omx_repo is not None:
            if args.tmux_bin is None:
                raise MatrixFailure(
                    "--omx-repo requires --tmux-bin (or tmux on PATH) for the "
                    "real private-socket fixture"
                )
            report["results"]["omx"] = verify_omx(args.omx_repo, args.tmux_bin)
    except (MatrixFailure, subprocess.TimeoutExpired) as error:
        report["status"] = "FAIL"
        report["error"] = str(error)
        rendered = json.dumps(report, indent=2, sort_keys=True)
        print(rendered)
        if args.output is not None:
            args.output.write_text(rendered + "\n", encoding="utf-8")
        return 1

    report["status"] = "PASS"
    rendered = json.dumps(report, indent=2, sort_keys=True)
    print(rendered)
    if args.output is not None:
        args.output.write_text(rendered + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
