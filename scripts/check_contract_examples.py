#!/usr/bin/env python3
"""Run manifest-listed lterm examples found in contract docs.

Only examples explicitly listed in docs/contract-manifest.json are eligible. Code
blocks or cookbook snippets that are not manifest-listed are deliberately ignored
so they cannot become accidental 1.0 release blockers.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Example:
    source: Path
    line: int
    command: str


def load_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def repo_root_for(manifest_path: Path) -> Path:
    if manifest_path.parent.name == "docs":
        return manifest_path.parent.parent
    return Path.cwd()


def lterm_prefix(repo_root: Path, explicit: str | None) -> list[str]:
    if explicit:
        return [explicit]
    env_bin = os.environ.get("LTERM_BIN")
    if env_bin:
        return [env_bin]
    for candidate in (repo_root / "target" / "debug" / "lterm", repo_root / "target" / "release" / "lterm"):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return [str(candidate)]
    return ["cargo", "run", "--quiet", "--"]


def manifest_examples(manifest: dict[str, Any]) -> dict[str, dict[str, Any]]:
    examples: dict[str, dict[str, Any]] = {}
    for entry in manifest.get("entries", []):
        if not isinstance(entry, dict):
            continue
        for command in entry.get("examples", []) or []:
            if not isinstance(command, str):
                continue
            examples[command.strip()] = entry
    return examples


def iter_shell_code_examples(path: Path) -> list[Example]:
    examples: list[Example] = []
    in_fence = False
    fence_lang = ""
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        stripped = raw.strip()
        if stripped.startswith("```"):
            if in_fence:
                in_fence = False
                fence_lang = ""
            else:
                in_fence = True
                fence_lang = stripped[3:].strip().lower()
            continue
        if not in_fence:
            continue
        if fence_lang not in {"", "bash", "sh", "shell", "console", "text"}:
            continue
        command = stripped[2:].strip() if stripped.startswith("$ ") else stripped
        if command.startswith("lterm "):
            examples.append(Example(path, lineno, command))
    return examples


def command_argv(command: str, prefix: list[str]) -> list[str]:
    parts = shlex.split(command)
    if not parts or parts[0] != "lterm":
        raise ValueError(f"not an lterm command: {command}")
    return [*prefix, *parts[1:]]


SESSION_ENV_KEYS = (
    "LTERM_SOCKET",
    "LTERM_SESSION",
    "LTERM_PANE",
    "LTERM_PARENT_TOKEN",
    "TMUX",
    "TMUX_PANE",
)


def scrub_lterm_session_env(env: dict[str, str]) -> None:
    for key in SESSION_ENV_KEYS:
        env.pop(key, None)


def expected_success(entry: dict[str, Any]) -> bool:
    return entry.get("expected_exit_code", 0) == 0


def run_example(example: Example, entry: dict[str, Any], prefix: list[str], repo_root: Path) -> list[str]:
    errors: list[str] = []
    with tempfile.TemporaryDirectory(prefix="lterm-example-") as tmp:
        env = os.environ.copy()
        env["LTERM_RUNTIME_DIR"] = str(Path(tmp) / "run")
        env["LTERM_DATA_DIR"] = str(Path(tmp) / "data")
        env["TMPDIR"] = tmp
        scrub_lterm_session_env(env)
        try:
            proc = subprocess.run(
                command_argv(example.command, prefix),
                cwd=repo_root,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=int(entry.get("example_timeout_seconds", 15)),
                check=False,
            )
        except subprocess.TimeoutExpired:
            return [f"{example.source}:{example.line}: timed out running {example.command!r}"]
        finally:
            subprocess.run(
                [*prefix, "shutdown"],
                cwd=repo_root,
                env=env,
                text=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
                check=False,
            )
    expected = int(entry.get("expected_exit_code", 0))
    if proc.returncode != expected:
        errors.append(
            f"{example.source}:{example.line}: {example.command!r} exited {proc.returncode}, expected {expected}; stderr={proc.stderr.strip()!r}"
        )
    argv = shlex.split(example.command)
    if entry.get("json_output_stability") == "stable" and "--json" in argv and proc.stdout.strip():
        try:
            json.loads(proc.stdout)
        except json.JSONDecodeError as exc:
            errors.append(f"{example.source}:{example.line}: stable JSON example did not emit JSON: {exc}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--lterm-bin")
    parser.add_argument("docs", nargs="+", type=Path)
    args = parser.parse_args()

    manifest = load_json(args.manifest)
    examples_by_command = manifest_examples(manifest)
    if not examples_by_command:
        print("ERROR: manifest does not list any runnable contract examples", file=sys.stderr)
        return 1
    repo_root = repo_root_for(args.manifest)
    prefix = lterm_prefix(repo_root, args.lterm_bin)

    errors: list[str] = []
    discovered: list[Example] = []
    for doc in args.docs:
        if not doc.is_file():
            errors.append(f"document not found: {doc}")
            continue
        discovered.extend(iter_shell_code_examples(doc))

    runnable = [example for example in discovered if example.command in examples_by_command]
    for command in examples_by_command:
        matches = [example for example in runnable if example.command == command]
        if not matches:
            errors.append(f"manifest-listed example not present in checked docs: {command}")
    for example in runnable:
        errors.extend(run_example(example, examples_by_command[example.command], prefix, repo_root))

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    ignored = len(discovered) - len(runnable)
    print(f"PASS contract examples: ran {len(runnable)} manifest-listed example(s), ignored {ignored} unlisted example(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
