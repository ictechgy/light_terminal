#!/usr/bin/env python3
"""Validate docs/contract-manifest.json for the lterm 1.0 contract gate."""
from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
from pathlib import Path
from typing import Any

TEXT_STABILITY = {"stable", "best-effort", "none"}
JSON_STABILITY = {"stable", "best-effort", "none"}
DOCS_OWNERS = {"README.md", "docs/public-contract.md", "docs/agent-install.md", "other"}
CLASSIFICATIONS = {"stable", "compatibility-stable", "best-effort", "internal", "explicit-raw-unsafe"}
RAW_POLICIES = {"not-applicable", "raw-transparent", "sanitized-output-only"}
REQUIRED_FIELDS = {
    "command",
    "aliases",
    "text_output_stability",
    "json_output_stability",
    "schema_path",
    "tests",
    "docs_owner",
    "classification",
    "raw_stream_policy",
}
ATTACH_NAMES = {"resume", "attach", "a", "open", "attach-or-new"}
RAW_SURFACE_NAMES = {"raw-pty-stream", "raw_pty_stream", "attached-pty", "attach-pty"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()

    repo_root = args.manifest.resolve().parent.parent if args.manifest.parent.name == "docs" else Path.cwd()
    errors = validate_manifest_file(args.manifest, repo_root)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"PASS: {args.manifest} contract manifest is valid")
    return 0


def validate_manifest_file(path: Path, repo_root: Path) -> list[str]:
    errors: list[str] = []
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return [f"manifest not found: {path}"]
    except json.JSONDecodeError as exc:
        return [f"manifest is not valid JSON: {exc}"]

    try:
        entries = manifest_entries(manifest)
    except ValueError as exc:
        return [str(exc)]

    if not entries:
        errors.append("manifest must contain at least one command entry")
        return errors

    rust_tests = collect_rust_test_names(repo_root)
    ci_targets = collect_ci_target_names(repo_root)
    seen_commands: set[str] = set()
    for index, entry in enumerate(entries):
        label = entry_label(index, entry)
        if not isinstance(entry, dict):
            errors.append(f"{label}: entry must be an object")
            continue
        missing = sorted(REQUIRED_FIELDS - set(entry))
        if missing:
            errors.append(f"{label}: missing required field(s): {', '.join(missing)}")
            continue

        command = entry.get("command")
        if not isinstance(command, str) or not command.strip():
            errors.append(f"{label}: command must be a non-empty string")
        elif command in seen_commands:
            errors.append(f"{label}: duplicate command {command!r}")
        else:
            seen_commands.add(command)

        aliases = entry.get("aliases")
        if not isinstance(aliases, list) or not all(isinstance(alias, str) for alias in aliases):
            errors.append(f"{label}: aliases must be an array of strings")

        errors.extend(enum_check(label, entry, "text_output_stability", TEXT_STABILITY))
        errors.extend(enum_check(label, entry, "json_output_stability", JSON_STABILITY))
        errors.extend(enum_check(label, entry, "docs_owner", DOCS_OWNERS))
        errors.extend(enum_check(label, entry, "classification", CLASSIFICATIONS))
        errors.extend(enum_check(label, entry, "raw_stream_policy", RAW_POLICIES))

        schema_path = entry.get("schema_path")
        if schema_path is not None and not isinstance(schema_path, str):
            errors.append(f"{label}: schema_path must be a string or null")
        if entry.get("json_output_stability") == "stable" and not schema_path:
            errors.append(f"{label}: stable JSON output requires a non-null schema_path")

        tests = entry.get("tests")
        if not isinstance(tests, list) or not tests or not all(isinstance(test, str) and test.strip() for test in tests):
            errors.append(f"{label}: tests must be a non-empty array of strings")
        elif not any(test_target_exists(test, repo_root, rust_tests, ci_targets) for test in tests):
            errors.append(f"{label}: tests names no real target: {tests!r}")

        surfaces = entry.get("surface_contracts", [])
        if surfaces is not None and not isinstance(surfaces, list):
            errors.append(f"{label}: surface_contracts must be an array when present")
            surfaces = []
        if isinstance(surfaces, list):
            errors.extend(validate_surface_contracts(label, surfaces))

        names = {command} | set(aliases or []) if isinstance(aliases, list) else {command}
        if any(name in ATTACH_NAMES for name in names if isinstance(name, str)):
            if not has_raw_attach_surface(surfaces if isinstance(surfaces, list) else []):
                errors.append(
                    f"{label}: attach/resume-like entry must include a raw-pty-stream surface_contract "
                    "classified explicit-raw-unsafe with raw_stream_policy raw-transparent"
                )

        if entry.get("classification") == "explicit-raw-unsafe" or (
            entry.get("raw_stream_policy") == "raw-transparent"
            and not has_raw_attach_surface(surfaces if isinstance(surfaces, list) else [])
        ):
            errors.extend(validate_raw_surface_not_schema_stable(label, entry))

    return errors


def manifest_entries(manifest: Any) -> list[Any]:
    if isinstance(manifest, list):
        return manifest
    if isinstance(manifest, dict):
        for key in ("commands", "entries"):
            value = manifest.get(key)
            if isinstance(value, list):
                return value
        raise ValueError("manifest object must contain a commands[] or entries[] array")
    raise ValueError("manifest must be a JSON object with commands[]/entries[] or a bare command array")


def enum_check(label: str, entry: dict[str, Any], field: str, allowed: set[str]) -> list[str]:
    value = entry.get(field)
    if value not in allowed:
        return [f"{label}: {field} must be one of {sorted(allowed)}, got {value!r}"]
    return []


def validate_surface_contracts(label: str, surfaces: list[Any]) -> list[str]:
    errors: list[str] = []
    for index, surface in enumerate(surfaces):
        surface_label = f"{label}.surface_contracts[{index}]"
        if not isinstance(surface, dict):
            errors.append(f"{surface_label}: surface contract must be an object")
            continue
        if "classification" in surface and surface["classification"] not in CLASSIFICATIONS:
            errors.append(f"{surface_label}: invalid classification {surface['classification']!r}")
        if "raw_stream_policy" in surface and surface["raw_stream_policy"] not in RAW_POLICIES:
            errors.append(f"{surface_label}: invalid raw_stream_policy {surface['raw_stream_policy']!r}")
        if surface.get("raw_stream_policy") == "raw-transparent":
            errors.extend(validate_raw_surface_not_schema_stable(surface_label, surface))
    return errors


def validate_raw_surface_not_schema_stable(label: str, surface: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    if surface.get("json_output_stability") == "stable":
        errors.append(f"{label}: raw-transparent PTY streams cannot be schema-stable JSON")
    if surface.get("text_output_stability") == "stable":
        errors.append(f"{label}: raw-transparent PTY streams cannot be stable sanitized text")
    if surface.get("schema_path"):
        errors.append(f"{label}: raw-transparent PTY streams must not declare schema_path")
    return errors


def has_raw_attach_surface(surfaces: list[Any]) -> bool:
    for surface in surfaces:
        if not isinstance(surface, dict):
            continue
        name = str(surface.get("surface") or surface.get("name") or surface.get("id") or "")
        if name in RAW_SURFACE_NAMES and surface.get("classification") == "explicit-raw-unsafe" and surface.get("raw_stream_policy") == "raw-transparent":
            return True
    return False


def entry_label(index: int, entry: Any) -> str:
    if isinstance(entry, dict) and isinstance(entry.get("command"), str):
        return f"commands[{index}] {entry['command']!r}"
    return f"commands[{index}]"


def collect_rust_test_names(repo_root: Path) -> set[str]:
    names: set[str] = set()
    for base in (repo_root / "src", repo_root / "tests"):
        if not base.exists():
            continue
        for path in base.rglob("*.rs"):
            try:
                text = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            names.update(re.findall(r"(?m)^\s*(?:pub\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(", text))
    return names


def collect_ci_target_names(repo_root: Path) -> set[str]:
    names: set[str] = set()
    workflow_dir = repo_root / ".github" / "workflows"
    if not workflow_dir.exists():
        return names
    for path in workflow_dir.glob("*.yml"):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        in_jobs = False
        for line in text.splitlines():
            if re.match(r"^jobs:\s*$", line):
                in_jobs = True
                continue
            if in_jobs:
                match = re.match(r"^  ([A-Za-z0-9_.-]+):\s*$", line)
                if match:
                    names.add(match.group(1))
                name_match = re.match(r"^    name:\s*(.+?)\s*$", line)
                if name_match:
                    names.add(name_match.group(1).strip('\"\''))
    for path in workflow_dir.glob("*.yaml"):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        in_jobs = False
        for line in text.splitlines():
            if re.match(r"^jobs:\s*$", line):
                in_jobs = True
                continue
            if in_jobs:
                match = re.match(r"^  ([A-Za-z0-9_.-]+):\s*$", line)
                if match:
                    names.add(match.group(1))
                name_match = re.match(r"^    name:\s*(.+?)\s*$", line)
                if name_match:
                    names.add(name_match.group(1).strip('\"\''))
    return names


def test_target_exists(target: str, repo_root: Path, rust_tests: set[str], ci_targets: set[str]) -> bool:
    target = target.strip()
    if not target:
        return False
    target_path = (repo_root / target).resolve()
    try:
        target_path.relative_to(repo_root.resolve())
    except ValueError:
        target_path = Path(target)
    if target_path.exists() or (repo_root / target).exists():
        return True

    if target in ci_targets:
        return True

    if target.startswith("cargo "):
        try:
            tokens = shlex.split(target)
        except ValueError:
            return False
        if len(tokens) >= 2 and tokens[0] == "cargo" and tokens[1] == "test":
            if "--test" in tokens:
                idx = tokens.index("--test")
                if idx + 1 < len(tokens):
                    return (repo_root / "tests" / f"{tokens[idx + 1]}.rs").exists()
                return False
            return True

    normalized = target.split("::")[-1]
    if normalized in rust_tests:
        return True
    if (repo_root / "tests" / f"{normalized}.rs").exists():
        return True
    if (repo_root / "src" / f"{normalized}.rs").exists():
        return True
    return False


if __name__ == "__main__":
    raise SystemExit(main())
