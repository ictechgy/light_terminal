#!/usr/bin/env python3
"""Detect simple drift between lterm top-level help and the contract manifest.

This is intentionally a small CI proof-of-concept: it checks that every
non-help top-level command exposed by `lterm --help` is represented by the
machine-readable public contract manifest, either as a command or alias, and
that every manifest command/alias still appears in top-level help.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

COMMAND_LINE_RE = re.compile(r"^\s{2}([a-z][a-z0-9-]*)\s{2,}")
ALIASES_RE = re.compile(r"\[aliases: ([^\]]+)\]")
IGNORED_HELP_COMMANDS = {"help"}
IGNORED_MANIFEST_COMMANDS = {"-a"}


def repo_root_for(manifest_path: Path) -> Path:
    if manifest_path.parent.name == "docs":
        return manifest_path.parent.parent
    return Path.cwd()


def load_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def manifest_entries(manifest: Any) -> list[dict[str, Any]]:
    entries = manifest.get("entries") if isinstance(manifest, dict) else None
    if not isinstance(entries, list):
        raise ValueError("manifest must be a JSON object with entries[]")
    return [entry for entry in entries if isinstance(entry, dict)]


def command_token(value: str) -> str | None:
    parts = value.split()
    if len(parts) < 2 or parts[0] != "lterm":
        return None
    return parts[1]


def manifest_tokens(manifest: Any) -> set[str]:
    tokens: set[str] = set()
    for entry in manifest_entries(manifest):
        command = entry.get("command")
        if isinstance(command, str):
            token = command_token(command)
            if token and token not in IGNORED_MANIFEST_COMMANDS:
                tokens.add(token)
        aliases = entry.get("aliases", [])
        if isinstance(aliases, list):
            for alias in aliases:
                if isinstance(alias, str):
                    token = command_token(alias)
                    if token and token not in IGNORED_MANIFEST_COMMANDS:
                        tokens.add(token)
    return tokens


def run_help(lterm_bin: str, repo_root: Path) -> str:
    env = os.environ.copy()
    env.setdefault("LTERM_RUNTIME_DIR", str(repo_root / "target" / "contract-drift-run"))
    proc = subprocess.run(
        [lterm_bin, "--help"],
        cwd=repo_root,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"{lterm_bin} --help failed with {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout


def help_tokens(help_text: str) -> set[str]:
    tokens: set[str] = set()
    in_commands = False
    for line in help_text.splitlines():
        stripped = line.strip()
        if stripped == "Commands:":
            in_commands = True
            continue
        if in_commands and stripped.startswith("Options"):
            break
        if not in_commands:
            continue
        match = COMMAND_LINE_RE.match(line)
        if match:
            command = match.group(1)
            if command not in IGNORED_HELP_COMMANDS:
                tokens.add(command)
            alias_match = ALIASES_RE.search(line)
            if alias_match:
                for alias in alias_match.group(1).split(","):
                    alias = alias.strip()
                    if alias and alias not in IGNORED_HELP_COMMANDS:
                        tokens.add(alias)
    return tokens


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--lterm-bin", required=True)
    args = parser.parse_args()

    manifest = load_json(args.manifest)
    repo_root = repo_root_for(args.manifest.resolve())
    manifest_set = manifest_tokens(manifest)
    help_set = help_tokens(run_help(args.lterm_bin, repo_root))

    missing_from_manifest = sorted(help_set - manifest_set)
    missing_from_help = sorted(manifest_set - help_set)
    if missing_from_manifest or missing_from_help:
        if missing_from_manifest:
            print(
                "ERROR: top-level help command(s) missing from contract manifest: "
                + ", ".join(missing_from_manifest),
                file=sys.stderr,
            )
        if missing_from_help:
            print(
                "ERROR: manifest command/alias token(s) missing from top-level help: "
                + ", ".join(missing_from_help),
                file=sys.stderr,
            )
        return 1

    print(f"PASS contract drift: {len(help_set)} top-level command token(s) covered by manifest")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
