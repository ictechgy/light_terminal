#!/usr/bin/env python3
"""Unit tests for scripts/footprint_benchmark.py."""

from __future__ import annotations

import json
import os
import signal
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import footprint_benchmark as fb


def kill_fake_daemons(tmp_path: Path) -> None:
    pid_file = tmp_path / "fake-lterm-daemons.pid"
    if not pid_file.exists():
        return
    for line in pid_file.read_text(encoding="utf-8").splitlines():
        try:
            os.kill(int(line), signal.SIGKILL)
        except (ValueError, ProcessLookupError, PermissionError):
            pass


def fake_daemon_pids(tmp_path: Path) -> list[int]:
    pid_file = tmp_path / "fake-lterm-daemons.pid"
    if not pid_file.exists():
        return []
    pids: list[int] = []
    for line in pid_file.read_text(encoding="utf-8").splitlines():
        try:
            pids.append(int(line))
        except ValueError:
            pass
    return pids


def assert_fake_daemons_gone(testcase: unittest.TestCase, tmp_path: Path) -> None:
    for pid in fake_daemon_pids(tmp_path):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            continue
        testcase.fail(f"fake daemon still running after harness cleanup: pid {pid}")


def command_names_from_log(log_path: Path) -> list[str]:
    names: list[str] = []
    for line in log_path.read_text(encoding="utf-8").splitlines():
        row = json.loads(line)
        argv = row.get("argv") or []
        if argv:
            names.append(Path(argv[0]).name)
    return names


class FootprintBenchmarkUnitTests(unittest.TestCase):
    def test_numeric_summary_uses_median_min_max(self) -> None:
        self.assertEqual(fb.numeric_summary([]), {"count": 0, "min": None, "median": None, "max": None})
        self.assertEqual(fb.numeric_summary([3, 1, 2]), {"count": 3, "min": 1.0, "median": 2.0, "max": 3.0})
        self.assertEqual(fb.numeric_summary([1.0, 5.0]), {"count": 2, "min": 1.0, "median": 3.0, "max": 5.0})

    def test_parse_pid_and_rss_parser(self) -> None:
        self.assertEqual(fb.parse_pid("123\n"), 123)
        self.assertIsNone(fb.parse_pid("not-a-pid\n"))
        self.assertIsNone(fb.parse_pid("0\n"))
        with mock.patch.object(fb, "run_command", return_value=fb.CommandResult(["ps"], 0, "  456\n", "", 1.0)):
            self.assertEqual(fb.read_rss_kib(123), (456, None))
        with mock.patch.object(fb, "run_command", return_value=fb.CommandResult(["ps"], 0, " nope\n", "", 1.0)):
            value, reason = fb.read_rss_kib(123)
            self.assertIsNone(value)
            self.assertIn("invalid ps RSS output", reason or "")

    def test_missing_tmux_is_skipped_not_lterm_error(self) -> None:
        config = fb.BenchmarkConfig(
            iterations=1,
            sessions=2,
            workflow_iterations=1,
            timeout=1.0,
            lterm_bin="lterm",
            tmux_bin="missing-tmux",
            keep_temp=False,
            temp_root=None,
        )
        result = fb.benchmark_tmux(config, None)
        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["metrics"]["daemon_ready_ms"]["status"], "skipped")

    def test_json_report_shape_has_distinct_startup_metrics(self) -> None:
        fake_lterm = {
            "status": "ok",
            "metrics": {
                "daemon_ready_ms": fb.metric_ok([1.0]),
                "first_session_ms": fb.metric_ok([2.0]),
                "rss_kib": {"idle_daemon": fb.rss_result(None, "unavailable")},
                "omx_style_workflow_ms": fb.metric_ok([3.0], iterations=[]),
            },
            "cleanup": [],
        }
        fake_tmux = {"status": "skipped", "metrics": {}, "cleanup": []}
        with mock.patch.object(fb, "resolve_binary", side_effect=lambda value: value), mock.patch.object(
            fb, "tool_version", return_value={"status": "ok"}
        ), mock.patch.object(fb, "benchmark_lterm", return_value=fake_lterm), mock.patch.object(
            fb, "benchmark_tmux", return_value=fake_tmux
        ):
            report, code = fb.build_report(
                fb.BenchmarkConfig(1, 2, 1, 1.0, "lterm", "tmux", False, None)
            )
        self.assertEqual(code, 0)
        metrics = report["results"]["lterm"]["metrics"]
        self.assertIn("daemon_ready_ms", metrics)
        self.assertIn("first_session_ms", metrics)
        self.assertNotIn("startup_ms", metrics)
        json.dumps(report)

    def test_cleanup_stack_runs_lifo(self) -> None:
        calls: list[str] = []
        cleanup = fb.CleanupStack()
        cleanup.add("first", lambda: calls.append("first"))
        cleanup.add("second", lambda: calls.append("second"))
        report = cleanup.run()
        self.assertEqual(calls, ["second", "first"])
        self.assertEqual([item["status"] for item in report], ["ok", "ok"])

    def test_workflow_rejects_ai_cli_invocation(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "AI CLI"):
            fb.assert_no_ai_cli(["omx", "status"])


class FootprintBenchmarkFakeBinaryTests(unittest.TestCase):
    def test_quick_run_with_fake_lterm_and_missing_tmux(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fake_lterm = write_fake_lterm(tmp_path)
            output_json = tmp_path / "report.json"
            output_md = tmp_path / "report.md"
            try:
                code = fb.main(
                    [
                        "--quick",
                        "--lterm-bin",
                        str(fake_lterm),
                        "--tmux-bin",
                        str(tmp_path / "missing-tmux"),
                        "--json",
                        str(output_json),
                        "--markdown",
                        str(output_md),
                        "--timeout",
                        "2",
                    ]
                )
            finally:
                kill_fake_daemons(tmp_path)
            self.assertEqual(code, 0)
            assert_fake_daemons_gone(self, tmp_path)
            report = json.loads(output_json.read_text(encoding="utf-8"))
            self.assertEqual(report["schema_version"], fb.SCHEMA_VERSION)
            self.assertEqual(report["results"]["lterm"]["status"], "ok")
            self.assertEqual(report["results"]["tmux"]["status"], "skipped")
            lterm_metrics = report["results"]["lterm"]["metrics"]
            self.assertEqual(lterm_metrics["daemon_ready_ms"]["status"], "ok")
            self.assertEqual(lterm_metrics["first_session_ms"]["status"], "ok")
            self.assertEqual(lterm_metrics["omx_style_workflow_ms"]["status"], "ok")
            self.assertIn("lterm Footprint Benchmark", output_md.read_text(encoding="utf-8"))
            log_path = tmp_path / "fake-lterm.log"
            log_text = log_path.read_text(encoding="utf-8")
            self.assertIn("LTERM_RUNTIME_DIR", log_text)
            self.assertNotIn("omx", command_names_from_log(log_path))
            self.assertNotIn("codex", command_names_from_log(log_path))

    def test_quick_run_with_fake_lterm_and_fake_tmux_records_required_sequence(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fake_lterm = write_fake_lterm(tmp_path)
            fake_tmux = write_fake_tmux(tmp_path)
            output_json = tmp_path / "report.json"
            try:
                code = fb.main(
                    [
                        "--quick",
                        "--lterm-bin",
                        str(fake_lterm),
                        "--tmux-bin",
                        str(fake_tmux),
                        "--json",
                        str(output_json),
                        "--timeout",
                        "2",
                    ]
                )
            finally:
                kill_fake_daemons(tmp_path)
            self.assertEqual(code, 0)
            assert_fake_daemons_gone(self, tmp_path)
            report = json.loads(output_json.read_text(encoding="utf-8"))
            self.assertIn("daemon_ready_ms", report["results"]["tmux"]["metrics"])
            steps = report["results"]["tmux"]["metrics"]["omx_style_workflow_ms"]["iterations"][0]["steps"]
            self.assertEqual(
                [step["step"] for step in steps],
                [
                    "create_controller_session",
                    "create_detached_helper_pane",
                    "list_panes",
                    "send_marker",
                    "capture_marker",
                    "cleanup_helper",
                    "cleanup_controller",
                ],
            )
            log = (tmp_path / "fake-tmux.log").read_text(encoding="utf-8")
            self.assertIn("-L", log)
            self.assertIn("split-window", log)
            self.assertIn("capture-pane", log)


def write_fake_lterm(tmp_path: Path) -> Path:
    script = tmp_path / "fake-lterm.py"
    log = tmp_path / "fake-lterm.log"
    state = tmp_path / "fake-lterm-state.txt"
    daemon_pids = tmp_path / "fake-lterm-daemons.pid"
    script.write_text(
        textwrap.dedent(
            f"""
            #!/usr/bin/env python3
            import json, os, re, signal, sys, time
            log_path = {str(log)!r}
            state_path = {str(state)!r}
            daemon_pids_path = {str(daemon_pids)!r}
            argv = sys.argv[1:]
            def marker_from_command(command):
                marker = 'BROKEN_FAKE_MARKER'
                if 'LTERM_FOOTPRINT_MARKER_' in command and 'EXECUTED' in command:
                    match = re.search(r"'(LTERM_FOOTPRINT_MARKER_[^']+_)'\\s+'EXECUTED'", command)
                    if match:
                        return match.group(1) + 'EXECUTED'
                if command.startswith('printf '):
                    return command.split('printf ', 1)[1]
                return marker

            with open(log_path, 'a', encoding='utf-8') as fh:
                fh.write(json.dumps({{'argv': argv, 'env': {{'LTERM_RUNTIME_DIR': os.environ.get('LTERM_RUNTIME_DIR'), 'LTERM_DATA_DIR': os.environ.get('LTERM_DATA_DIR')}}}}, sort_keys=True) + '\\n')

            if argv == ['--version']:
                print('fake-lterm 1.0')
                raise SystemExit(0)
            if argv and argv[0] == 'daemon':
                with open(daemon_pids_path, 'a', encoding='utf-8') as fh:
                    fh.write(str(os.getpid()) + '\\n')
                signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
                while True:
                    time.sleep(0.1)
            if argv[:2] == ['doctor', '--json']:
                print(json.dumps({{'daemon_reachable': True}}))
                raise SystemExit(0)
            if argv and argv[0] == 'shutdown':
                raise SystemExit(0)
            if argv and argv[0] == 'start':
                print('fake\\t%1\\tsh')
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'new-session']:
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'split-window']:
                print('%99')
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'list-panes']:
                print('%1\\n%99')
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'send-keys']:
                command = next((part for part in argv if part.startswith('printf ')), '')
                marker = marker_from_command(command)
                with open(state_path, 'w', encoding='utf-8') as state_fh:
                    state_fh.write(marker)
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'capture-pane']:
                try:
                    with open(state_path, encoding='utf-8') as state_fh:
                        print(state_fh.read())
                except FileNotFoundError:
                    print('BROKEN_FAKE_MARKER')
                raise SystemExit(0)
            if argv[:2] == ['tmux-compat', 'kill-pane'] or argv[:2] == ['tmux-compat', 'kill-session']:
                raise SystemExit(0)
            print('unexpected fake lterm args', argv, file=sys.stderr)
            raise SystemExit(1)
            """
        ).lstrip(),
        encoding="utf-8",
    )
    script.chmod(0o755)
    return script


def write_fake_tmux(tmp_path: Path) -> Path:
    script = tmp_path / "fake-tmux.py"
    log = tmp_path / "fake-tmux.log"
    state = tmp_path / "fake-tmux-state.txt"
    script.write_text(
        textwrap.dedent(
            f"""
            #!/usr/bin/env python3
            import json, os, re, sys
            log_path = {str(log)!r}
            state_path = {str(state)!r}
            argv = sys.argv[1:]
            def marker_from_command(command):
                marker = 'BROKEN_FAKE_MARKER'
                if 'LTERM_FOOTPRINT_MARKER_' in command and 'EXECUTED' in command:
                    match = re.search(r"'(LTERM_FOOTPRINT_MARKER_[^']+_)'\\s+'EXECUTED'", command)
                    if match:
                        return match.group(1) + 'EXECUTED'
                if command.startswith('printf '):
                    return command.split('printf ', 1)[1]
                return marker

            with open(log_path, 'a', encoding='utf-8') as fh:
                fh.write(json.dumps({{'argv': argv}}, sort_keys=True) + '\\n')

            def marker_from_command(command):
                marker = 'BROKEN_FAKE_MARKER'
                if 'LTERM_FOOTPRINT_MARKER_' in command and 'EXECUTED' in command:
                    match = re.search(r"'(LTERM_FOOTPRINT_MARKER_[^']+_)'\\s+'EXECUTED'", command)
                    if match:
                        return match.group(1) + 'EXECUTED'
                if command.startswith('printf '):
                    return command.split('printf ', 1)[1]
                return marker

            if argv == ['--version']:
                print('tmux fake')
                raise SystemExit(0)
            if len(argv) >= 2 and argv[0] == '-L':
                argv = argv[2:]
            if argv and argv[0] == 'start-server':
                raise SystemExit(0)
            if argv[:3] == ['display-message', '-p', '#{{pid}}']:
                print(os.getpid())
                raise SystemExit(0)
            if argv and argv[0] == 'new-session':
                raise SystemExit(0)
            if argv and argv[0] == 'split-window':
                print('%77')
                raise SystemExit(0)
            if argv and argv[0] == 'list-panes':
                print('%1\\n%77')
                raise SystemExit(0)
            if argv and argv[0] == 'send-keys':
                command = next((part for part in argv if part.startswith('printf ')), '')
                marker = marker_from_command(command)
                with open(state_path, 'w', encoding='utf-8') as state_fh:
                    state_fh.write(marker)
                raise SystemExit(0)
            if argv and argv[0] == 'capture-pane':
                try:
                    with open(state_path, encoding='utf-8') as state_fh:
                        print(state_fh.read())
                except FileNotFoundError:
                    print('BROKEN_FAKE_MARKER')
                raise SystemExit(0)
            if argv and argv[0] in ('kill-pane', 'kill-session', 'kill-server'):
                raise SystemExit(0)
            print('unexpected fake tmux args', argv, file=sys.stderr)
            raise SystemExit(1)

            def marker_from_command(command):
                marker = 'BROKEN_FAKE_MARKER'
                if 'LTERM_FOOTPRINT_MARKER_' in command and 'EXECUTED' in command:
                    match = re.search(r"'(LTERM_FOOTPRINT_MARKER_[^']+_)'\\s+'EXECUTED'", command)
                    if match:
                        return match.group(1) + 'EXECUTED'
                if command.startswith('printf '):
                    return command.split('printf ', 1)[1]
                return marker
            """
        ).lstrip(),
        encoding="utf-8",
    )
    script.chmod(0o755)
    return script


if __name__ == "__main__":
    unittest.main()
