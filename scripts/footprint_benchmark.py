#!/usr/bin/env python3
"""Measure isolated lterm/tmux footprint baselines.

The harness is intentionally opt-in and dependency-free.  It emits structured JSON
first and can derive a compact Markdown summary from the same report.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence

SCHEMA_VERSION = 1
DEFAULT_TIMEOUT_SECONDS = 10.0
WORKFLOW_MARKER_PREFIX = "LTERM_FOOTPRINT_MARKER"
AI_CLI_NAMES = {
    "omx",
    "omc",
    "codex",
    "claude",
    "gemini",
    "forge",
    "agy",
    "opencode",
    "kiro",
    "aider",
    "goose",
    "qwen",
}


@dataclass(frozen=True)
class CommandResult:
    argv: list[str]
    returncode: int
    stdout: str
    stderr: str
    elapsed_ms: float
    timed_out: bool = False

    @property
    def ok(self) -> bool:
        return self.returncode == 0 and not self.timed_out


def monotonic_ms() -> float:
    return time.perf_counter_ns() / 1_000_000.0


def elapsed_since_ms(start_ms: float) -> float:
    return monotonic_ms() - start_ms


def numeric_summary(samples: Sequence[float | int]) -> dict[str, Any]:
    """Return stable min/median/max/count summary for numeric samples."""
    numeric = [float(sample) for sample in samples]
    if not numeric:
        return {"count": 0, "min": None, "median": None, "max": None}
    return {
        "count": len(numeric),
        "min": min(numeric),
        "median": statistics.median(numeric),
        "max": max(numeric),
    }


def metric_ok(samples: Sequence[float | int], unit: str = "ms", **extra: Any) -> dict[str, Any]:
    return {
        "status": "ok",
        "unit": unit,
        "samples": list(samples),
        "summary": numeric_summary(samples),
        **extra,
    }


def metric_skipped(reason: str, unit: str = "ms", **extra: Any) -> dict[str, Any]:
    return {
        "status": "skipped",
        "unit": unit,
        "samples": [],
        "summary": numeric_summary([]),
        "reason": reason,
        **extra,
    }


def metric_error(reason: str, unit: str = "ms", **extra: Any) -> dict[str, Any]:
    return {
        "status": "error",
        "unit": unit,
        "samples": [],
        "summary": numeric_summary([]),
        "reason": reason,
        **extra,
    }


def rss_result(value: int | None, reason: str | None = None) -> dict[str, Any]:
    if value is None:
        return {"status": "skipped", "unit": "KiB", "rss_kib": None, "rss_reason": reason}
    return {"status": "ok", "unit": "KiB", "rss_kib": value, "rss_reason": None}


def run_command(
    argv: Sequence[str],
    *,
    env: Mapping[str, str] | None = None,
    cwd: str | Path | None = None,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    input_text: str | None = None,
) -> CommandResult:
    start = monotonic_ms()
    try:
        completed = subprocess.run(
            list(argv),
            input=input_text,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            cwd=str(cwd) if cwd is not None else None,
            env=dict(env) if env is not None else None,
            timeout=timeout,
            check=False,
        )
        return CommandResult(
            argv=list(argv),
            returncode=completed.returncode,
            stdout=completed.stdout,
            stderr=completed.stderr,
            elapsed_ms=elapsed_since_ms(start),
        )
    except subprocess.TimeoutExpired as exc:
        return CommandResult(
            argv=list(argv),
            returncode=124,
            stdout=(exc.stdout or "") if isinstance(exc.stdout, str) else "",
            stderr=((exc.stderr or "") if isinstance(exc.stderr, str) else "")
            + f"\n[TIMEOUT after {timeout}s]\n",
            elapsed_ms=elapsed_since_ms(start),
            timed_out=True,
        )
    except FileNotFoundError as exc:
        return CommandResult(
            argv=list(argv),
            returncode=127,
            stdout="",
            stderr=str(exc),
            elapsed_ms=elapsed_since_ms(start),
        )
    except OSError as exc:
        return CommandResult(
            argv=list(argv),
            returncode=126,
            stdout="",
            stderr=str(exc),
            elapsed_ms=elapsed_since_ms(start),
        )


class CleanupStack:
    def __init__(self) -> None:
        self._actions: list[tuple[str, Callable[[], None]]] = []

    def add(self, name: str, func: Callable[[], None]) -> None:
        self._actions.append((name, func))

    def run(self) -> list[dict[str, str]]:
        results: list[dict[str, str]] = []
        while self._actions:
            name, func = self._actions.pop()
            try:
                func()
                results.append({"name": name, "status": "ok"})
            except Exception as exc:  # pragma: no cover - defensive cleanup evidence
                results.append({"name": name, "status": "error", "error": str(exc)})
        return results


@dataclass
class BenchmarkConfig:
    iterations: int
    sessions: int
    workflow_iterations: int
    timeout: float
    lterm_bin: str
    tmux_bin: str
    keep_temp: bool
    temp_root: str | None


def default_lterm_bin() -> str:
    candidate = Path("target/debug/lterm")
    if candidate.exists() and os.access(candidate, os.X_OK):
        return str(candidate)
    return "lterm"


def resolve_binary(binary: str) -> str | None:
    if os.sep in binary or (os.altsep and os.altsep in binary):
        path = Path(binary)
        return str(path) if path.exists() and os.access(path, os.X_OK) else None
    return shutil.which(binary)


def base_env() -> dict[str, str]:
    env = os.environ.copy()
    for key in ["LTERM_SOCKET", "TMUX", "TMUX_PANE"]:
        env.pop(key, None)
    for key in list(env):
        if key.startswith("CMUX_"):
            env.pop(key, None)
    return env


class ToolContext:
    def __init__(self, tool: str, config: BenchmarkConfig) -> None:
        self.tool = tool
        self.config = config
        self.cleanup = CleanupStack()
        root_parent = Path(config.temp_root) if config.temp_root else None
        self.temp_dir = Path(tempfile.mkdtemp(prefix=f"lterm-footprint-{tool}-", dir=root_parent))
        self.cleanup.add("remove-temp-root", self._remove_temp_root)
        self.run_id = uuid.uuid4().hex[:12]
        self.env = base_env()
        if tool == "lterm":
            runtime = self.temp_dir / "runtime"
            data = self.temp_dir / "data"
            runtime.mkdir(parents=True, exist_ok=True)
            data.mkdir(parents=True, exist_ok=True)
            os.chmod(runtime, 0o700)
            os.chmod(data, 0o700)
            self.env["LTERM_RUNTIME_DIR"] = str(runtime)
            self.env["LTERM_DATA_DIR"] = str(data)
        self.tmux_server_name = f"lterm-footprint-{self.run_id}"

    def _remove_temp_root(self) -> None:
        if not self.config.keep_temp:
            shutil.rmtree(self.temp_dir, ignore_errors=True)

    def add_cleanup(self, name: str, func: Callable[[], None]) -> None:
        self.cleanup.add(name, func)

    def finish(self) -> list[dict[str, str]]:
        return self.cleanup.run()


class ProcessHandle:
    def __init__(self, proc: subprocess.Popen[str], name: str) -> None:
        self.proc = proc
        self.name = name

    def terminate(self, timeout: float = 2.0) -> None:
        if self.proc.poll() is not None:
            try:
                self.proc.communicate(timeout=0.1)
            except Exception:
                pass
            return
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
        except Exception:
            self.proc.terminate()
        try:
            self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            except Exception:
                self.proc.kill()
            try:
                self.proc.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.proc.wait(timeout=timeout)


def spawn_lterm_daemon(ctx: ToolContext, lterm_bin: str) -> ProcessHandle:
    proc = subprocess.Popen(
        [lterm_bin, "daemon"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
        env=ctx.env,
        start_new_session=True,
    )
    handle = ProcessHandle(proc, "lterm-daemon")
    ctx.add_cleanup("terminate-lterm-daemon-process", handle.terminate)
    ctx.add_cleanup(
        "lterm-shutdown",
        lambda: run_command([lterm_bin, "shutdown"], env=ctx.env, timeout=ctx.config.timeout),
    )
    return handle


def wait_for_lterm_ready(
    ctx: ToolContext, lterm_bin: str, handle: ProcessHandle, timeout: float
) -> tuple[bool, str | None]:
    deadline = time.monotonic() + timeout
    last_error = "doctor probe did not run"
    while time.monotonic() < deadline:
        if handle.proc.poll() is not None:
            return False, f"daemon exited early with code {handle.proc.returncode}"
        probe = run_command([lterm_bin, "doctor", "--json"], env=ctx.env, timeout=min(1.0, timeout))
        if probe.ok:
            try:
                report = json.loads(probe.stdout)
                if report.get("daemon_reachable") is True:
                    return True, None
                last_error = f"daemon_reachable={report.get('daemon_reachable')!r}"
            except json.JSONDecodeError as exc:
                last_error = f"invalid doctor JSON: {exc}"
        else:
            last_error = f"doctor rc={probe.returncode}: {probe.stderr.strip()}"
        time.sleep(0.05)
    return False, f"timed out waiting for lterm daemon readiness: {last_error}"


def start_lterm_daemon_and_measure(ctx: ToolContext, lterm_bin: str) -> tuple[dict[str, Any], int | None]:
    start = monotonic_ms()
    try:
        handle = spawn_lterm_daemon(ctx, lterm_bin)
    except OSError as exc:
        return metric_error(f"failed to spawn lterm daemon: {exc}"), None
    ready, reason = wait_for_lterm_ready(ctx, lterm_bin, handle, ctx.config.timeout)
    if not ready:
        return metric_error(reason or "lterm daemon readiness failed"), handle.proc.pid
    return metric_ok([elapsed_since_ms(start)]), handle.proc.pid


def tmux_prefix(ctx: ToolContext, tmux_bin: str) -> list[str]:
    return [tmux_bin, "-L", ctx.tmux_server_name]


def tmux_command(ctx: ToolContext, tmux_bin: str, args: Sequence[str]) -> list[str]:
    return tmux_prefix(ctx, tmux_bin) + list(args)


def start_tmux_server_and_measure(ctx: ToolContext, tmux_bin: str) -> tuple[dict[str, Any], int | None]:
    ctx.add_cleanup(
        "tmux-kill-server",
        lambda: run_command(tmux_command(ctx, tmux_bin, ["kill-server"]), env=ctx.env, timeout=ctx.config.timeout),
    )
    start = monotonic_ms()
    result = run_command(
        tmux_command(ctx, tmux_bin, ["start-server", ";", "set-option", "-g", "exit-empty", "off"]),
        env=ctx.env,
        timeout=ctx.config.timeout,
    )
    if not result.ok:
        return metric_skipped(f"tmux start-server/exit-empty setup failed: {short_result(result)}"), None
    pid = poll_tmux_pid(ctx, tmux_bin)
    if pid is None:
        return metric_skipped("tmux server started but display-message -p '#{pid}' did not expose a PID"), None
    return metric_ok([elapsed_since_ms(start)]), pid


def parse_pid(text: str) -> int | None:
    stripped = text.strip()
    if not stripped:
        return None
    first = stripped.splitlines()[0].strip()
    try:
        value = int(first)
    except ValueError:
        return None
    return value if value > 0 else None


def poll_tmux_pid(ctx: ToolContext, tmux_bin: str) -> int | None:
    deadline = time.monotonic() + ctx.config.timeout
    while time.monotonic() < deadline:
        probe = run_command(
            tmux_command(ctx, tmux_bin, ["display-message", "-p", "#{pid}"]),
            env=ctx.env,
            timeout=min(1.0, ctx.config.timeout),
        )
        if probe.ok:
            pid = parse_pid(probe.stdout)
            if pid is not None:
                return pid
        time.sleep(0.05)
    return None


def short_result(result: CommandResult, limit: int = 180) -> str:
    text = result.stderr.strip() or result.stdout.strip() or f"rc={result.returncode}"
    text = " ".join(text.split())
    return text[:limit]


def read_rss_kib(pid: int, timeout: float = 2.0) -> tuple[int | None, str | None]:
    if pid <= 0:
        return None, "invalid PID"
    result = run_command(["ps", "-o", "rss=", "-p", str(pid)], timeout=timeout)
    if not result.ok:
        return None, f"ps failed: {short_result(result)}"
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if not lines:
        return None, "ps returned no RSS output"
    try:
        rss = int(lines[0].split()[0])
    except (ValueError, IndexError):
        return None, f"invalid ps RSS output: {lines[0]!r}"
    if rss < 0:
        return None, f"invalid negative RSS: {rss}"
    return rss, None


def lterm_start_session(ctx: ToolContext, lterm_bin: str, name: str, command: str = "sleep 30") -> CommandResult:
    return run_command(
        [lterm_bin, "start", "-d", "-n", name, "sh", "-lc", command],
        env=ctx.env,
        timeout=ctx.config.timeout,
    )


def tmux_new_session(ctx: ToolContext, tmux_bin: str, name: str, command: str = "sleep 30") -> CommandResult:
    return run_command(
        tmux_command(ctx, tmux_bin, ["new-session", "-d", "-s", name, command]),
        env=ctx.env,
        timeout=ctx.config.timeout,
    )


def benchmark_lterm_daemon_ready(config: BenchmarkConfig, lterm_bin: str) -> tuple[dict[str, Any], list[dict[str, str]]]:
    samples: list[float] = []
    cleanup_reports: list[dict[str, str]] = []
    for _ in range(config.iterations):
        ctx = ToolContext("lterm", config)
        try:
            metric, _pid = start_lterm_daemon_and_measure(ctx, lterm_bin)
            if metric["status"] != "ok":
                return metric, cleanup_reports + ctx.finish()
            samples.extend(metric["samples"])
        finally:
            cleanup_reports.extend(ctx.finish())
    return metric_ok(samples), cleanup_reports


def benchmark_tmux_daemon_ready(config: BenchmarkConfig, tmux_bin: str) -> tuple[dict[str, Any], list[dict[str, str]]]:
    samples: list[float] = []
    cleanup_reports: list[dict[str, str]] = []
    skipped_reason: str | None = None
    for _ in range(config.iterations):
        ctx = ToolContext("tmux", config)
        try:
            metric, _pid = start_tmux_server_and_measure(ctx, tmux_bin)
            if metric["status"] == "ok":
                samples.extend(metric["samples"])
            else:
                skipped_reason = metric.get("reason", "tmux daemon readiness skipped")
        finally:
            cleanup_reports.extend(ctx.finish())
    if samples:
        return metric_ok(samples), cleanup_reports
    return metric_skipped(skipped_reason or "tmux readiness probe unavailable"), cleanup_reports


def benchmark_first_session(
    tool: str, config: BenchmarkConfig, binary: str
) -> tuple[dict[str, Any], list[dict[str, str]]]:
    samples: list[float] = []
    cleanup_reports: list[dict[str, str]] = []
    for _ in range(config.iterations):
        ctx = ToolContext(tool, config)
        session = f"footprint-first-{ctx.run_id}"
        if tool == "lterm":
            ctx.add_cleanup("lterm-shutdown", lambda b=binary, e=ctx.env: run_command([b, "shutdown"], env=e, timeout=config.timeout))
            cmd = lambda: lterm_start_session(ctx, binary, session)
        else:
            ctx.add_cleanup("tmux-kill-server", lambda c=ctx, b=binary: run_command(tmux_command(c, b, ["kill-server"]), env=c.env, timeout=config.timeout))
            cmd = lambda: tmux_new_session(ctx, binary, session)
        start = monotonic_ms()
        result = cmd()
        elapsed = elapsed_since_ms(start)
        try:
            if not result.ok:
                return metric_error(f"{tool} first session failed: {short_result(result)}"), cleanup_reports + ctx.finish()
            samples.append(elapsed)
        finally:
            cleanup_reports.extend(ctx.finish())
    return metric_ok(samples), cleanup_reports


def benchmark_lterm_rss(config: BenchmarkConfig, lterm_bin: str) -> tuple[dict[str, Any], list[dict[str, str]]]:
    ctx = ToolContext("lterm", config)
    rss_metrics: dict[str, Any]
    try:
        ready, pid = start_lterm_daemon_and_measure(ctx, lterm_bin)
        if ready["status"] != "ok" or pid is None:
            rss_metrics = {"idle_daemon": rss_result(None, ready.get("reason", "daemon not ready"))}
        else:
            idle, idle_reason = read_rss_kib(pid)
            one_session = f"footprint-one-{ctx.run_id}"
            one = lterm_start_session(ctx, lterm_bin, one_session)
            if not one.ok:
                one_rss = rss_result(None, f"one active session failed: {short_result(one)}")
                many_rss = rss_result(None, "multiple sessions skipped because one active session failed")
            else:
                value, reason = read_rss_kib(pid)
                one_rss = rss_result(value, reason)
                for index in range(1, config.sessions):
                    result = lterm_start_session(ctx, lterm_bin, f"footprint-many-{ctx.run_id}-{index}")
                    if not result.ok:
                        many_rss = rss_result(None, f"multiple sessions failed at {index}: {short_result(result)}")
                        break
                else:
                    value, reason = read_rss_kib(pid)
                    many_rss = rss_result(value, reason)
            rss_metrics = {
                "idle_daemon": rss_result(idle, idle_reason),
                "one_active_session": one_rss,
                "multiple_sessions": {**many_rss, "session_count": config.sessions},
            }
    finally:
        cleanup = ctx.finish()
    return rss_metrics, cleanup


def benchmark_tmux_rss(config: BenchmarkConfig, tmux_bin: str) -> tuple[dict[str, Any], list[dict[str, str]]]:
    ctx = ToolContext("tmux", config)
    rss_metrics: dict[str, Any]
    try:
        ready, pid = start_tmux_server_and_measure(ctx, tmux_bin)
        if ready["status"] != "ok" or pid is None:
            rss_metrics = {"idle_daemon": rss_result(None, ready.get("reason", "tmux PID probe failed"))}
        else:
            idle, idle_reason = read_rss_kib(pid)
            session = f"footprint-rss-{ctx.run_id}"
            first = tmux_new_session(ctx, tmux_bin, session)
            if not first.ok:
                one_rss = rss_result(None, f"tmux new-session failed: {short_result(first)}")
                many_rss = rss_result(None, "multiple sessions skipped because one active session failed")
            else:
                value, reason = read_rss_kib(pid)
                one_rss = rss_result(value, reason)
                for index in range(1, config.sessions):
                    result = tmux_new_session(ctx, tmux_bin, f"footprint-many-{ctx.run_id}-{index}")
                    if not result.ok:
                        many_rss = rss_result(None, f"multiple sessions failed at {index}: {short_result(result)}")
                        break
                else:
                    value, reason = read_rss_kib(pid)
                    many_rss = rss_result(value, reason)
            rss_metrics = {
                "idle_daemon": rss_result(idle, idle_reason),
                "one_active_session": one_rss,
                "multiple_sessions": {**many_rss, "session_count": config.sessions},
            }
    finally:
        cleanup = ctx.finish()
    return rss_metrics, cleanup


def assert_no_ai_cli(argv: Sequence[str]) -> None:
    for token in argv:
        name = Path(token).name
        if name in AI_CLI_NAMES:
            raise RuntimeError(f"benchmark workflow must not invoke AI CLI: {name}")


def run_checked_step(
    step: str,
    argv: Sequence[str],
    *,
    env: Mapping[str, str],
    timeout: float,
    marker: str | None = None,
) -> dict[str, Any]:
    assert_no_ai_cli(argv)
    start = monotonic_ms()
    result = run_command(argv, env=env, timeout=timeout)
    record: dict[str, Any] = {
        "step": step,
        "status": "ok" if result.ok else "error",
        "elapsed_ms": elapsed_since_ms(start),
        "argv_preview": argv_preview(argv),
    }
    if marker is not None:
        record["marker_found"] = marker in result.stdout
    if not result.ok:
        record["reason"] = short_result(result)
    return record


def argv_preview(argv: Sequence[str]) -> list[str]:
    # Keep JSON useful without embedding long shell payloads.
    return [arg if len(arg) <= 80 else arg[:77] + "..." for arg in argv]


def workflow_lterm_iteration(ctx: ToolContext, lterm_bin: str, marker: str) -> dict[str, Any]:
    controller = f"footprint-controller-{ctx.run_id}"
    expected_output = f"{marker}_EXECUTED"
    steps: list[dict[str, Any]] = []
    ctx.add_cleanup("lterm-shutdown", lambda: run_command([lterm_bin, "shutdown"], env=ctx.env, timeout=ctx.config.timeout))
    steps.append(
        run_checked_step(
            "create_controller_session",
            [lterm_bin, "tmux-compat", "new-session", "-d", "-s", controller, "sh"],
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    helper_result = run_command(
        [
            lterm_bin,
            "tmux-compat",
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            f"{controller}:0",
            "sh",
        ],
        env=ctx.env,
        timeout=ctx.config.timeout,
    )
    helper_pane = helper_result.stdout.strip().splitlines()[0] if helper_result.ok and helper_result.stdout.strip() else f"{controller}:0"
    steps.append(
        {
            "step": "create_detached_helper_pane",
            "status": "ok" if helper_result.ok else "error",
            "elapsed_ms": helper_result.elapsed_ms,
            "argv_preview": argv_preview(helper_result.argv),
            **({"reason": short_result(helper_result)} if not helper_result.ok else {"helper_pane": helper_pane}),
        }
    )
    steps.append(
        run_checked_step(
            "list_panes",
            [lterm_bin, "tmux-compat", "list-panes", "-a", "-F", "#{pane_id}"],
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    steps.append(
        run_checked_step(
            "send_marker",
            [
                lterm_bin,
                "tmux-compat",
                "send-keys",
                "-t",
                helper_pane,
                f"printf '%s%s\\n' '{marker}_' 'EXECUTED'",
                "C-m",
            ],
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    capture = run_until_marker(
        [lterm_bin, "tmux-compat", "capture-pane", "-p", "-t", helper_pane],
        env=ctx.env,
        timeout=ctx.config.timeout,
        marker=expected_output,
    )
    steps.append({"step": "capture_marker", **capture})
    steps.append(
        run_checked_step(
            "cleanup_helper",
            [lterm_bin, "tmux-compat", "kill-pane", "-t", helper_pane],
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    steps.append(
        run_checked_step(
            "cleanup_controller",
            [lterm_bin, "tmux-compat", "kill-session", "-t", controller],
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    return summarize_workflow_iteration(steps)


def workflow_tmux_iteration(ctx: ToolContext, tmux_bin: str, marker: str) -> dict[str, Any]:
    controller = f"footprint-controller-{ctx.run_id}"
    expected_output = f"{marker}_EXECUTED"
    steps: list[dict[str, Any]] = []
    ctx.add_cleanup("tmux-kill-server", lambda: run_command(tmux_command(ctx, tmux_bin, ["kill-server"]), env=ctx.env, timeout=ctx.config.timeout))
    steps.append(
        run_checked_step(
            "create_controller_session",
            tmux_command(ctx, tmux_bin, ["new-session", "-d", "-s", controller, "sh"]),
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    helper_result = run_command(
        tmux_command(
            ctx,
            tmux_bin,
            ["split-window", "-d", "-P", "-F", "#{pane_id}", "-t", f"{controller}:0", "sh"],
        ),
        env=ctx.env,
        timeout=ctx.config.timeout,
    )
    helper_pane = helper_result.stdout.strip().splitlines()[0] if helper_result.ok and helper_result.stdout.strip() else f"{controller}:0"
    steps.append(
        {
            "step": "create_detached_helper_pane",
            "status": "ok" if helper_result.ok else "error",
            "elapsed_ms": helper_result.elapsed_ms,
            "argv_preview": argv_preview(helper_result.argv),
            **({"reason": short_result(helper_result)} if not helper_result.ok else {"helper_pane": helper_pane}),
        }
    )
    steps.append(
        run_checked_step(
            "list_panes",
            tmux_command(ctx, tmux_bin, ["list-panes", "-a", "-F", "#{pane_id}"]),
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    steps.append(
        run_checked_step(
            "send_marker",
            tmux_command(
                ctx,
                tmux_bin,
                ["send-keys", "-t", helper_pane, f"printf '%s%s\\n' '{marker}_' 'EXECUTED'", "C-m"],
            ),
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    capture = run_until_marker(
        tmux_command(ctx, tmux_bin, ["capture-pane", "-p", "-t", helper_pane]),
        env=ctx.env,
        timeout=ctx.config.timeout,
        marker=expected_output,
    )
    steps.append({"step": "capture_marker", **capture})
    steps.append(
        run_checked_step(
            "cleanup_helper",
            tmux_command(ctx, tmux_bin, ["kill-pane", "-t", helper_pane]),
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    steps.append(
        run_checked_step(
            "cleanup_controller",
            tmux_command(ctx, tmux_bin, ["kill-session", "-t", controller]),
            env=ctx.env,
            timeout=ctx.config.timeout,
        )
    )
    return summarize_workflow_iteration(steps)


def run_until_marker(
    argv: Sequence[str], *, env: Mapping[str, str], timeout: float, marker: str
) -> dict[str, Any]:
    assert_no_ai_cli(argv)
    start = monotonic_ms()
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = run_command(argv, env=env, timeout=min(1.0, timeout))
        if last.ok and marker in last.stdout:
            return {
                "status": "ok",
                "elapsed_ms": elapsed_since_ms(start),
                "marker_found": True,
                "argv_preview": argv_preview(argv),
            }
        time.sleep(0.05)
    reason = "marker not captured"
    if last is not None and not last.ok:
        reason = short_result(last)
    return {
        "status": "error",
        "elapsed_ms": elapsed_since_ms(start),
        "marker_found": False,
        "argv_preview": argv_preview(argv),
        "reason": reason,
    }


def summarize_workflow_iteration(steps: list[dict[str, Any]]) -> dict[str, Any]:
    status = "ok" if all(step.get("status") == "ok" for step in steps) else "error"
    return {"status": status, "elapsed_ms": sum(float(step.get("elapsed_ms", 0.0)) for step in steps), "steps": steps}


def benchmark_workflow(
    tool: str, config: BenchmarkConfig, binary: str
) -> tuple[dict[str, Any], list[dict[str, str]]]:
    iterations: list[dict[str, Any]] = []
    cleanup_reports: list[dict[str, str]] = []
    for _ in range(config.workflow_iterations):
        ctx = ToolContext(tool, config)
        marker = f"{WORKFLOW_MARKER_PREFIX}_{ctx.run_id}"
        try:
            iteration = workflow_lterm_iteration(ctx, binary, marker) if tool == "lterm" else workflow_tmux_iteration(ctx, binary, marker)
            iterations.append(iteration)
        finally:
            cleanup_reports.extend(ctx.finish())
    ok_iterations = [item for item in iterations if item["status"] == "ok"]
    if not ok_iterations:
        reason = next((step.get("reason") for item in iterations for step in item.get("steps", []) if step.get("status") != "ok"), "workflow failed")
        return metric_error(str(reason), iterations=iterations), cleanup_reports
    return metric_ok([item["elapsed_ms"] for item in ok_iterations], iterations=iterations), cleanup_reports


def tool_version(binary: str) -> dict[str, Any]:
    result = run_command([binary, "--version"], timeout=3.0)
    return {
        "status": "ok" if result.ok else "error",
        "stdout": result.stdout.strip().splitlines()[:3],
        "stderr": result.stderr.strip().splitlines()[:3],
        "returncode": result.returncode,
    }


def benchmark_lterm(config: BenchmarkConfig, resolved: str) -> dict[str, Any]:
    cleanup: list[dict[str, str]] = []
    daemon_ready, cleanup_part = benchmark_lterm_daemon_ready(config, resolved)
    cleanup.extend(cleanup_part)
    first_session, cleanup_part = benchmark_first_session("lterm", config, resolved)
    cleanup.extend(cleanup_part)
    rss, cleanup_part = benchmark_lterm_rss(config, resolved)
    cleanup.extend(cleanup_part)
    workflow, cleanup_part = benchmark_workflow("lterm", config, resolved)
    cleanup.extend(cleanup_part)
    metrics = {
        "daemon_ready_ms": daemon_ready,
        "first_session_ms": first_session,
        "rss_kib": rss,
        "omx_style_workflow_ms": workflow,
    }
    status = "ok" if all_metric_statuses_ok_or_skipped(metrics, allow_skipped=False) else "error"
    return {"status": status, "metrics": metrics, "cleanup": cleanup}


def benchmark_tmux(config: BenchmarkConfig, resolved: str | None) -> dict[str, Any]:
    if resolved is None:
        return {
            "status": "skipped",
            "reason": "tmux binary not found",
            "metrics": {
                "daemon_ready_ms": metric_skipped("tmux binary not found"),
                "first_session_ms": metric_skipped("tmux binary not found"),
                "rss_kib": {"idle_daemon": rss_result(None, "tmux binary not found")},
                "omx_style_workflow_ms": metric_skipped("tmux binary not found"),
            },
            "cleanup": [],
        }
    cleanup: list[dict[str, str]] = []
    daemon_ready, cleanup_part = benchmark_tmux_daemon_ready(config, resolved)
    cleanup.extend(cleanup_part)
    first_session, cleanup_part = benchmark_first_session("tmux", config, resolved)
    cleanup.extend(cleanup_part)
    rss, cleanup_part = benchmark_tmux_rss(config, resolved)
    cleanup.extend(cleanup_part)
    workflow, cleanup_part = benchmark_workflow("tmux", config, resolved)
    cleanup.extend(cleanup_part)
    metrics = {
        "daemon_ready_ms": daemon_ready,
        "first_session_ms": first_session,
        "rss_kib": rss,
        "omx_style_workflow_ms": workflow,
    }
    status = "ok" if not metric_tree_has_status(metrics, "error") else "error"
    return {"status": status, "metrics": metrics, "cleanup": cleanup}


def metric_tree_has_status(node: Any, status: str) -> bool:
    if isinstance(node, Mapping):
        if node.get("status") == status:
            return True
        return any(metric_tree_has_status(value, status) for value in node.values())
    if isinstance(node, list):
        return any(metric_tree_has_status(value, status) for value in node)
    return False


def all_metric_statuses_ok_or_skipped(metrics: Mapping[str, Any], *, allow_skipped: bool) -> bool:
    if metric_tree_has_status(metrics, "error"):
        return False
    if not allow_skipped and metric_tree_has_status(metrics, "skipped"):
        # RSS can be legitimately unavailable without failing the lterm lane.
        non_rss = {key: value for key, value in metrics.items() if key != "rss_kib"}
        return not metric_tree_has_status(non_rss, "skipped")
    return True


def build_report(config: BenchmarkConfig) -> tuple[dict[str, Any], int]:
    lterm_resolved = resolve_binary(config.lterm_bin)
    tmux_resolved = resolve_binary(config.tmux_bin)
    tools = {
        "lterm": {
            "requested": config.lterm_bin,
            "resolved": lterm_resolved,
            "available": lterm_resolved is not None,
            "version": tool_version(lterm_resolved) if lterm_resolved else None,
        },
        "tmux": {
            "requested": config.tmux_bin,
            "resolved": tmux_resolved,
            "available": tmux_resolved is not None,
            "version": tool_version(tmux_resolved) if tmux_resolved else None,
        },
    }
    if lterm_resolved is None:
        lterm_result = {
            "status": "error",
            "reason": "lterm binary not found",
            "metrics": {
                "daemon_ready_ms": metric_error("lterm binary not found"),
                "first_session_ms": metric_error("lterm binary not found"),
                "rss_kib": {"idle_daemon": rss_result(None, "lterm binary not found")},
                "omx_style_workflow_ms": metric_error("lterm binary not found"),
            },
            "cleanup": [],
        }
    else:
        lterm_result = benchmark_lterm(config, lterm_resolved)
    tmux_result = benchmark_tmux(config, tmux_resolved)
    report = {
        "schema_version": SCHEMA_VERSION,
        "generated_at_unix_secs": int(time.time()),
        "config": {
            "iterations": config.iterations,
            "sessions": config.sessions,
            "workflow_iterations": config.workflow_iterations,
            "timeout_seconds": config.timeout,
            "keep_temp": config.keep_temp,
        },
        "environment": {
            "platform": platform.platform(),
            "python_version": platform.python_version(),
            "machine": platform.machine(),
            "processor": platform.processor(),
            "cwd": os.getcwd(),
            "rss_command": "ps -o rss= -p <pid>",
        },
        "tools": tools,
        "results": {"lterm": lterm_result, "tmux": tmux_result},
    }
    exit_code = 1 if lterm_result.get("status") == "error" else 0
    return report, exit_code


def render_markdown(report: Mapping[str, Any]) -> str:
    lines = [
        "# lterm Footprint Benchmark Baseline",
        "",
        f"- Schema version: `{report.get('schema_version')}`",
        f"- Generated at (unix seconds): `{report.get('generated_at_unix_secs')}`",
        f"- Platform: `{report.get('environment', {}).get('platform')}`",
        "",
        "Host-dependent numbers are not pass/fail criteria; compare reports only with their metadata.",
        "",
        "## Summary",
        "",
        "| Tool | Status | daemon_ready_ms median | first_session_ms median | workflow_ms median | RSS notes |",
        "| --- | --- | ---: | ---: | ---: | --- |",
    ]
    for tool in ("lterm", "tmux"):
        result = report.get("results", {}).get(tool, {})
        metrics = result.get("metrics", {}) if isinstance(result, Mapping) else {}
        daemon = median_text(metrics.get("daemon_ready_ms"))
        first = median_text(metrics.get("first_session_ms"))
        workflow = median_text(metrics.get("omx_style_workflow_ms"))
        rss_notes = rss_summary_text(metrics.get("rss_kib"))
        lines.append(f"| {tool} | {result.get('status', 'unknown')} | {daemon} | {first} | {workflow} | {rss_notes} |")
    lines.extend(["", "## Tool availability", ""])
    for tool, data in report.get("tools", {}).items():
        lines.append(f"- `{tool}`: requested `{data.get('requested')}`, resolved `{data.get('resolved')}`, available `{data.get('available')}`")
    lines.extend(["", "## Methodology", ""])
    lines.extend(
        [
            "- `daemon_ready_ms`: lterm starts an isolated daemon and waits for `doctor --json`; tmux starts an isolated server and probes `display-message -p '#{pid}'`.",
            "- `first_session_ms`: cold detached session creation in isolated state.",
            "- `rss_kib`: `ps -o rss= -p <pid>` for the owned daemon/server PID when available.",
            "- `omx_style_workflow_ms`: bounded local tmux-compatible operations: create controller, split helper, list, send marker, capture, cleanup.",
            "- Missing tmux or unavailable RSS is recorded as `skipped`, not hidden.",
        ]
    )
    return "\n".join(lines) + "\n"


def median_text(metric: Any) -> str:
    if not isinstance(metric, Mapping):
        return "n/a"
    if metric.get("status") != "ok":
        return "n/a"
    median = metric.get("summary", {}).get("median")
    return "n/a" if median is None else f"{float(median):.2f}"


def rss_summary_text(node: Any) -> str:
    if not isinstance(node, Mapping):
        return "n/a"
    parts: list[str] = []
    for key in ("idle_daemon", "one_active_session", "multiple_sessions"):
        value = node.get(key)
        if not isinstance(value, Mapping):
            continue
        if value.get("rss_kib") is None:
            parts.append(f"{key}: skipped")
        else:
            parts.append(f"{key}: {value.get('rss_kib')} KiB")
    return "; ".join(parts) if parts else "n/a"


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run isolated lterm/tmux footprint benchmark baseline")
    parser.add_argument("--quick", action="store_true", help="Use 1 iteration, 2 sessions, and 1 workflow iteration")
    parser.add_argument("--iterations", type=positive_int, default=5)
    parser.add_argument("--sessions", type=positive_int, default=8)
    parser.add_argument("--workflow-iterations", type=positive_int, default=3)
    parser.add_argument("--timeout", type=positive_float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--lterm-bin", default=os.environ.get("LTERM_BIN", default_lterm_bin()))
    parser.add_argument("--tmux-bin", default=os.environ.get("TMUX_BIN", "tmux"))
    parser.add_argument("--json", dest="json_path", help="Write JSON report to this path")
    parser.add_argument("--markdown", dest="markdown_path", help="Write Markdown summary to this path")
    parser.add_argument("--keep-temp", action="store_true", help="Keep isolated temp roots for debugging")
    parser.add_argument("--temp-root", help="Parent directory for isolated temp roots")
    return parser.parse_args(argv)


def config_from_args(args: argparse.Namespace) -> BenchmarkConfig:
    iterations = 1 if args.quick else args.iterations
    sessions = 2 if args.quick else args.sessions
    workflow_iterations = 1 if args.quick else args.workflow_iterations
    return BenchmarkConfig(
        iterations=iterations,
        sessions=sessions,
        workflow_iterations=workflow_iterations,
        timeout=args.timeout,
        lterm_bin=args.lterm_bin,
        tmux_bin=args.tmux_bin,
        keep_temp=args.keep_temp,
        temp_root=args.temp_root,
    )


def write_text(path: str | None, text: str) -> None:
    if not path:
        return
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    config = config_from_args(args)
    report, exit_code = build_report(config)
    json_text = json.dumps(report, indent=2, sort_keys=True)
    if args.json_path:
        write_text(args.json_path, json_text + "\n")
    else:
        print(json_text)
    if args.markdown_path:
        write_text(args.markdown_path, render_markdown(report))
    return exit_code


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
