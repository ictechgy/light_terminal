#!/usr/bin/env python3
"""Detect drift between lterm top-level help, contract docs, and the manifest.

This intentionally small CI proof-of-concept checks that every non-help top-level
command exposed by `lterm --help` is represented by the machine-readable public
contract manifest, every manifest command/alias still appears in top-level help,
each docs_owner mentions its owned commands and aliases, and the public contract
tables agree with manifest aliases, classifications, and raw-stream policies.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

COMMAND_LINE_RE = re.compile(r"^\s{2}([a-z][a-z0-9-]*)\s{2,}")
ALIASES_RE = re.compile(r"\[aliases: ([^\]]+)\]")
CODE_SPAN_RE = re.compile(r"(?<!`)`([^`\n]+)`(?!`)")
IGNORED_HELP_COMMANDS = {"help"}
IGNORED_MANIFEST_COMMANDS = {"-a"}
DOC_OWNER_PATHS = {
    "README.md": Path("README.md"),
    "docs/public-contract.md": Path("docs/public-contract.md"),
    "docs/agent-install.md": Path("docs/agent-install.md"),
}


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
    with tempfile.TemporaryDirectory(prefix="lterm-contract-drift-") as tmp:
        tmp_path = Path(tmp)
        env["LTERM_RUNTIME_DIR"] = str(tmp_path / "run")
        env["LTERM_DATA_DIR"] = str(tmp_path / "data")
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
    saw_commands = False
    for line in help_text.splitlines():
        stripped = line.strip()
        if stripped == "Commands:":
            in_commands = True
            saw_commands = True
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
                for raw_alias in alias_match.group(1).split(","):
                    alias = raw_alias.strip()
                    if alias and alias not in IGNORED_HELP_COMMANDS:
                        tokens.add(alias)
    if not saw_commands:
        raise ValueError("top-level help did not contain a Commands: section")
    if not tokens:
        raise ValueError("parsed empty command set from top-level help; clap output may have changed")
    return tokens


def drift_errors(manifest_set: set[str], help_set: set[str]) -> tuple[list[str], list[str]]:
    return sorted(help_set - manifest_set), sorted(manifest_set - help_set)


def command_is_mentioned(command: str, markdown: str) -> bool:
    for span in CODE_SPAN_RE.findall(markdown):
        if span == command or span.startswith(f"{command} "):
            return True
    return False


def owner_doc_errors(manifest: Any, repo_root: Path) -> list[str]:
    errors: list[str] = []
    cache: dict[str, str] = {}
    for entry in manifest_entries(manifest):
        owner = entry.get("docs_owner")
        owner_path = DOC_OWNER_PATHS.get(owner)
        if owner_path is None:
            if owner is not None:
                errors.append(f"{entry.get('command')}: unknown docs_owner {owner!r}")
            continue
        if owner not in cache:
            path = repo_root / owner_path
            if not path.exists():
                errors.append(f"{owner}: docs_owner file is missing")
                cache[owner] = ""
            else:
                cache[owner] = path.read_text(encoding="utf-8")
        markdown = cache[owner]
        for field, values in [
            ("command", [entry.get("command")]),
            ("alias", entry.get("aliases") if isinstance(entry.get("aliases"), list) else []),
        ]:
            for value in values:
                if isinstance(value, str) and not command_is_mentioned(value, markdown):
                    errors.append(f"{entry.get('command')}: {field} {value!r} missing from {owner}")
    return errors


def strip_code_value(cell: str) -> str:
    spans = CODE_SPAN_RE.findall(cell)
    if spans:
        return spans[0].strip()
    return cell.strip()


def split_markdown_row(line: str) -> list[str]:
    stripped = line.strip()
    if not stripped.startswith("|") or not stripped.endswith("|"):
        return []
    cells = [cell.strip() for cell in stripped.strip("|").split("|")]
    if not cells or all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
        return []
    return cells


def public_contract_rows(path: Path) -> dict[str, dict[str, Any]]:
    rows: dict[str, dict[str, Any]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        cells = split_markdown_row(line)
        if len(cells) < 3 or cells[0] == "Command":
            continue
        command_spans = [span.strip() for span in CODE_SPAN_RE.findall(cells[0]) if span.startswith("lterm ")]
        if len(command_spans) != 1:
            continue
        alias_spans = [span.strip() for span in CODE_SPAN_RE.findall(cells[1]) if span.startswith("lterm ")]
        raw_stream_policy = strip_code_value(cells[5]) if len(cells) >= 6 else None
        rows[command_spans[0]] = {
            "aliases": alias_spans,
            "classification": strip_code_value(cells[2]),
            "raw_stream_policy": raw_stream_policy,
        }
    return rows


def public_contract_errors(manifest: Any, repo_root: Path) -> list[str]:
    path = repo_root / "docs" / "public-contract.md"
    if not path.exists():
        return ["docs/public-contract.md: public contract doc is missing"]
    rows = public_contract_rows(path)
    entries = {entry.get("command"): entry for entry in manifest_entries(manifest)}
    expected_entries = {
        entry.get("command"): entry
        for entry in manifest_entries(manifest)
        if entry.get("docs_owner") == "docs/public-contract.md"
        and isinstance(entry.get("command"), str)
    }
    errors: list[str] = []

    for command in sorted(expected_entries):
        if command not in rows:
            errors.append(f"{command}: missing public-contract table row")

    for command, row in rows.items():
        entry = entries.get(command)
        if entry is None:
            errors.append(f"docs/public-contract.md: documented command {command!r} missing from manifest")
            continue
        manifest_aliases = set(entry.get("aliases", []) or [])
        row_aliases = set(row["aliases"])
        if manifest_aliases != row_aliases:
            errors.append(
                f"{command}: public-contract aliases {sorted(row_aliases)!r} "
                f"!= manifest aliases {sorted(manifest_aliases)!r}"
            )
        if row["classification"] != entry.get("classification"):
            errors.append(
                f"{command}: public-contract classification {row['classification']!r} "
                f"!= manifest classification {entry.get('classification')!r}"
            )
        raw_stream_policy = row.get("raw_stream_policy")
        if raw_stream_policy != entry.get("raw_stream_policy"):
            errors.append(
                f"{command}: public-contract raw_stream_policy {raw_stream_policy!r} "
                f"!= manifest raw_stream_policy {entry.get('raw_stream_policy')!r}"
            )
    return errors


def self_test() -> int:
    help_text = """Light Terminal

Commands:
  start   Create a persistent session [aliases: new]
  logs    Read sanitized scrollback [aliases: capture]
  env     Print shell exports
  help    Print this message

Options:
  -h, --help
"""
    expected_help = {"start", "new", "logs", "capture", "env"}
    parsed_help = help_tokens(help_text)
    if parsed_help != expected_help:
        raise AssertionError(f"help token parser mismatch: {parsed_help!r}")

    manifest = {
        "entries": [
            {"command": "lterm start", "aliases": ["lterm new"]},
            {"command": "lterm logs", "aliases": ["lterm capture"]},
            {"command": "lterm env", "aliases": []},
            {"command": "lterm -a NAME", "aliases": []},
        ]
    }
    parsed_manifest = manifest_tokens(manifest)
    if parsed_manifest != expected_help:
        raise AssertionError(f"manifest token parser mismatch: {parsed_manifest!r}")

    missing_from_manifest, missing_from_help = drift_errors(parsed_manifest - {"env"}, parsed_help)
    if missing_from_manifest != ["env"] or missing_from_help:
        raise AssertionError("missing-from-manifest drift self-test failed")

    missing_from_manifest, missing_from_help = drift_errors(parsed_manifest | {"notify"}, parsed_help)
    if missing_from_manifest or missing_from_help != ["notify"]:
        raise AssertionError("missing-from-help drift self-test failed")

    with tempfile.TemporaryDirectory(prefix="lterm-contract-drift-self-test-") as tmp:
        root = Path(tmp)
        (root / "docs").mkdir()
        (root / "README.md").write_text("Use `lterm env`.\n", encoding="utf-8")
        public_contract_markdown = """| Command | Aliases | Classification | Text output | JSON output | Raw stream policy |
| --- | --- | --- | --- | --- | --- |
| `lterm start` | `lterm new` | `stable` | none | none | `raw-transparent` |
| `lterm logs` | `lterm capture` | `stable` | stable | none | `sanitized-output-only` |
"""
        public_contract_path = root / "docs" / "public-contract.md"
        public_contract_path.write_text(public_contract_markdown, encoding="utf-8")
        doc_manifest = {
            "entries": [
                {
                    "command": "lterm start",
                    "aliases": ["lterm new"],
                    "classification": "stable",
                    "raw_stream_policy": "raw-transparent",
                    "docs_owner": "docs/public-contract.md",
                },
                {
                    "command": "lterm logs",
                    "aliases": ["lterm capture"],
                    "classification": "stable",
                    "raw_stream_policy": "sanitized-output-only",
                    "docs_owner": "docs/public-contract.md",
                },
                {
                    "command": "lterm env",
                    "aliases": [],
                    "classification": "stable",
                    "raw_stream_policy": "sanitized-output-only",
                    "docs_owner": "README.md",
                },
            ]
        }
        if owner_doc_errors(doc_manifest, root):
            raise AssertionError("owner docs self-test should pass")
        if public_contract_errors(doc_manifest, root):
            raise AssertionError("public-contract table self-test should pass")

        missing_doc_manifest = {"entries": [dict(doc_manifest["entries"][0], command="lterm wait")]}
        if not owner_doc_errors(missing_doc_manifest, root):
            raise AssertionError("owner docs self-test should catch missing command mention")

        unknown_owner_manifest = {
            "entries": [dict(doc_manifest["entries"][0], docs_owner="docs/unknown.md")]
        }
        if not owner_doc_errors(unknown_owner_manifest, root):
            raise AssertionError("owner docs self-test should catch unknown docs_owner")

        public_contract_path.write_text(
            public_contract_markdown.replace(
                "| `lterm logs` | `lterm capture` | `stable` | stable | none | `sanitized-output-only` |\n",
                "Narrative still mentions `lterm logs`.\n",
            ),
            encoding="utf-8",
        )
        if not public_contract_errors(doc_manifest, root):
            raise AssertionError("public-contract table self-test should catch missing table row")
        public_contract_path.write_text(public_contract_markdown, encoding="utf-8")

        stale_row_manifest = {
            "entries": [
                dict(doc_manifest["entries"][0], aliases=[]),
                doc_manifest["entries"][1],
            ]
        }
        if not public_contract_errors(stale_row_manifest, root):
            raise AssertionError("public-contract table self-test should catch alias drift")

        stale_classification_manifest = {
            "entries": [
                dict(doc_manifest["entries"][0], classification="best-effort"),
                doc_manifest["entries"][1],
            ]
        }
        if not public_contract_errors(stale_classification_manifest, root):
            raise AssertionError("public-contract table self-test should catch classification drift")

        public_contract_path.write_text(
            public_contract_markdown.replace(
                "| `lterm logs` | `lterm capture` | `stable` | stable | none | `sanitized-output-only` |",
                "| `lterm logs` | `lterm capture` | `stable` | stable | none |",
            ),
            encoding="utf-8",
        )
        if not public_contract_errors(doc_manifest, root):
            raise AssertionError("public-contract table self-test should catch raw-stream policy drift")

    print("PASS contract drift self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--lterm-bin")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if args.manifest is None or args.lterm_bin is None:
        parser.error("--manifest and --lterm-bin are required unless --self-test is used")

    manifest = load_json(args.manifest)
    repo_root = repo_root_for(args.manifest.resolve())
    manifest_set = manifest_tokens(manifest)
    help_set = help_tokens(run_help(args.lterm_bin, repo_root))

    missing_from_manifest, missing_from_help = drift_errors(manifest_set, help_set)
    docs_errors = owner_doc_errors(manifest, repo_root)
    public_errors = public_contract_errors(manifest, repo_root)
    if missing_from_manifest or missing_from_help or docs_errors or public_errors:
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
        for error in docs_errors:
            print(f"ERROR: docs owner drift: {error}", file=sys.stderr)
        for error in public_errors:
            print(f"ERROR: public contract table drift: {error}", file=sys.stderr)
        return 1

    print(
        "PASS contract drift: "
        f"{len(help_set)} top-level command token(s), "
        f"{len(manifest_entries(manifest))} docs owner reference(s), "
        f"{len(public_contract_rows(repo_root / 'docs' / 'public-contract.md'))} public-contract row(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
