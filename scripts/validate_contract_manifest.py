#!/usr/bin/env python3
"""Validate docs/contract-manifest.json for the lterm 1.0 contract gate."""
from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
import tempfile
from copy import deepcopy
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python < 3.11 fallback.
    tomllib = None  # type: ignore[assignment]

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
OPTIONAL_ENTRY_FIELDS = {
    "surface_contracts",
    "stability_scope",
    "json_samples",
    "examples",
    "example_timeout_seconds",
    "expected_exit_code",
}
ALLOWED_ENTRY_FIELDS = REQUIRED_FIELDS | OPTIONAL_ENTRY_FIELDS
SURFACE_REQUIRED_FIELDS = {
    "name",
    "classification",
    "raw_stream_policy",
    "text_output_stability",
    "json_output_stability",
    "schema_path",
}
ATTACH_NAMES = {"resume", "attach", "a", "open", "attach-or-new"}
RAW_SURFACE_NAMES = {"raw-pty-stream", "raw_pty_stream", "attached-pty", "attach-pty"}
CARGO_TEST_FLAGS_WITH_VALUE = {
    "--package",
    "-p",
    "--exclude",
    "--test",
    "--bin",
    "--bench",
    "--example",
    "--features",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--profile",
    "--jobs",
    "-j",
    "--color",
    "--message-format",
    "--config",
    "-Z",
}
RUST_FN_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, nargs="?")
    parser.add_argument("--self-test", action="store_true", help="Run validator regression tests and exit")
    args = parser.parse_args()

    if args.self_test:
        return run_self_tests()
    if args.manifest is None:
        parser.error("manifest is required unless --self-test is used")

    repo_root = args.manifest.resolve().parent.parent if args.manifest.parent.name == "docs" else Path.cwd()
    errors = validate_manifest_file(args.manifest, repo_root)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"PASS: {args.manifest} contract manifest is valid")
    return 0


def validate_manifest_file(path: Path, repo_root: Path) -> list[str]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return [f"manifest not found: {path}"]
    except json.JSONDecodeError as exc:
        return [f"manifest is not valid JSON: {exc}"]
    return validate_manifest(manifest, repo_root)


def validate_manifest(manifest: Any, repo_root: Path) -> list[str]:
    errors: list[str] = []
    try:
        entries = manifest_entries(manifest)
    except ValueError as exc:
        return [str(exc)]

    if not entries:
        return ["manifest must contain at least one command entry"]

    rust_tests = collect_rust_test_names(repo_root)
    ci_targets = collect_ci_target_names(repo_root)
    seen_commands: set[str] = set()
    for index, entry in enumerate(entries):
        label = entry_label(index, entry)
        if not isinstance(entry, dict):
            errors.append(f"{label}: entry must be an object")
            continue

        unknown = sorted(set(entry) - ALLOWED_ENTRY_FIELDS)
        if unknown:
            errors.append(f"{label}: unknown field(s): {', '.join(unknown)}")

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

        stability_scope = entry.get("stability_scope")
        if stability_scope is not None and (not isinstance(stability_scope, str) or not stability_scope.strip()):
            errors.append(f"{label}: stability_scope must be a non-empty string when present")

        tests = entry.get("tests")
        if not isinstance(tests, list) or not tests or not all(isinstance(test, str) and test.strip() for test in tests):
            errors.append(f"{label}: tests must be a non-empty array of strings")
        else:
            missing_tests = [test for test in tests if not test_target_exists(test, repo_root, rust_tests, ci_targets)]
            if missing_tests:
                errors.append(f"{label}: tests name no real target(s): {missing_tests!r}")

        surfaces = entry.get("surface_contracts", [])
        if surfaces is not None and not isinstance(surfaces, list):
            errors.append(f"{label}: surface_contracts must be an array when present")
            surfaces = []
        if isinstance(surfaces, list):
            if surfaces and not (isinstance(stability_scope, str) and stability_scope.strip()):
                errors.append(f"{label}: surface_contracts requires a non-empty stability_scope")
            errors.extend(validate_surface_contracts(label, entry, surfaces))

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


def validate_surface_contracts(label: str, entry: dict[str, Any], surfaces: list[Any]) -> list[str]:
    errors: list[str] = []
    seen_names: set[str] = set()
    parent_raw_policy = entry.get("raw_stream_policy")
    has_policy_override = False
    for index, surface in enumerate(surfaces):
        surface_label = f"{label}.surface_contracts[{index}]"
        if not isinstance(surface, dict):
            errors.append(f"{surface_label}: surface contract must be an object")
            continue

        unknown = sorted(set(surface) - SURFACE_REQUIRED_FIELDS)
        if unknown:
            errors.append(f"{surface_label}: unknown field(s): {', '.join(unknown)}")
        missing = sorted(SURFACE_REQUIRED_FIELDS - set(surface))
        if missing:
            errors.append(f"{surface_label}: missing required field(s): {', '.join(missing)}")
            continue

        name = surface.get("name")
        if not isinstance(name, str) or not name.strip():
            errors.append(f"{surface_label}: name must be a non-empty string")
        elif name in seen_names:
            errors.append(f"{surface_label}: duplicate surface contract name {name!r}")
        else:
            seen_names.add(name)

        errors.extend(enum_check(surface_label, surface, "classification", CLASSIFICATIONS))
        errors.extend(enum_check(surface_label, surface, "raw_stream_policy", RAW_POLICIES))
        errors.extend(enum_check(surface_label, surface, "text_output_stability", TEXT_STABILITY))
        errors.extend(enum_check(surface_label, surface, "json_output_stability", JSON_STABILITY))

        schema_path = surface.get("schema_path")
        if schema_path is not None and not isinstance(schema_path, str):
            errors.append(f"{surface_label}: schema_path must be a string or null")
        if surface.get("json_output_stability") == "stable" and not schema_path:
            errors.append(f"{surface_label}: stable JSON output requires a non-null schema_path")
        if surface.get("raw_stream_policy") == "raw-transparent":
            errors.extend(validate_raw_surface_not_schema_stable(surface_label, surface))
        if surface.get("raw_stream_policy") != parent_raw_policy:
            has_policy_override = True

    if has_policy_override and not str(entry.get("stability_scope") or "").strip():
        errors.append(f"{label}: surface raw_stream_policy overrides require stability_scope")
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
        name = surface.get("name")
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
            names.update(collect_rust_test_names_from_file(path))
    return names


def collect_rust_test_names_from_file(path: Path) -> set[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (FileNotFoundError, UnicodeDecodeError):
        return set()

    names: set[str] = set()
    pending_test_attr = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("#["):
            if re.search(r"#\s*\[\s*(?:tokio::|async_std::)?test\b", stripped):
                pending_test_attr = True
            continue
        match = RUST_FN_RE.match(line)
        if match:
            if pending_test_attr:
                names.add(match.group(1))
            pending_test_attr = False
            continue
        if stripped:
            pending_test_attr = False
    return names


def collect_ci_target_names(repo_root: Path) -> set[str]:
    names: set[str] = set()
    workflow_dir = repo_root / ".github" / "workflows"
    if not workflow_dir.exists():
        return names
    for path in list(workflow_dir.glob("*.yml")) + list(workflow_dir.glob("*.yaml")):
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
                    names.add(name_match.group(1).strip('"\''))
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
            return cargo_test_target_exists(tokens, repo_root, rust_tests)
    if target.startswith(("python ", "python3 ")):
        try:
            tokens = shlex.split(target)
        except ValueError:
            return False
        if len(tokens) >= 2:
            script_path = (repo_root / tokens[1]).resolve()
            try:
                script_path.relative_to(repo_root.resolve())
            except ValueError:
                return False
            return script_path.exists()

    normalized = target.split("::")[-1]
    if normalized in rust_tests:
        return True
    if (repo_root / "tests" / f"{normalized}.rs").exists():
        return True
    if (repo_root / "src" / f"{normalized}.rs").exists():
        return True
    return False


def cargo_test_target_exists(tokens: list[str], repo_root: Path, rust_tests: set[str]) -> bool:
    test_path: Path | None = None
    bin_name: str | None = None
    lib_scope = False
    filters: list[str] = []
    index = 2
    while index < len(tokens):
        token = tokens[index]
        if token == "--":
            break
        if token == "--test":
            if index + 1 >= len(tokens):
                return False
            test_path = repo_root / "tests" / f"{tokens[index + 1]}.rs"
            index += 2
            continue
        if token == "--lib":
            lib_scope = True
            index += 1
            continue
        if token == "--bin":
            if index + 1 >= len(tokens):
                return False
            bin_name = tokens[index + 1]
            index += 2
            continue
        if token in CARGO_TEST_FLAGS_WITH_VALUE:
            index += 2
            continue
        if token.startswith("--") and "=" in token:
            index += 1
            continue
        if token.startswith("-"):
            index += 1
            continue
        filters.append(token)
        index += 1

    if test_path is not None:
        if not test_path.exists():
            return False
        return rust_filters_exist(filters, collect_rust_test_names_from_file(test_path))

    if bin_name is not None and not cargo_bin_exists(repo_root, bin_name):
        return False

    if lib_scope or bin_name is not None:
        src_tests: set[str] = set()
        for path in (repo_root / "src").rglob("*.rs"):
            src_tests.update(collect_rust_test_names_from_file(path))
        return rust_filters_exist(filters, src_tests)

    return rust_filters_exist(filters, rust_tests) if filters else True


def rust_filters_exist(filters: list[str], names: set[str]) -> bool:
    if not filters:
        return True
    return all(any(test_filter in name for name in names) for test_filter in filters)


def cargo_bin_exists(repo_root: Path, bin_name: str) -> bool:
    if (repo_root / "src" / "bin" / f"{bin_name}.rs").exists():
        return True
    cargo_toml = repo_root / "Cargo.toml"
    if not cargo_toml.exists():
        return False
    if tomllib is not None:
        try:
            cargo = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
        except Exception:
            cargo = {}
        for entry in cargo.get("bin", []):
            if isinstance(entry, dict) and entry.get("name") == bin_name:
                path = entry.get("path")
                return bool(path and (repo_root / str(path)).exists())
        package_name = cargo.get("package", {}).get("name") if isinstance(cargo.get("package"), dict) else None
        return package_name == bin_name and (repo_root / "src" / "main.rs").exists()

    text = cargo_toml.read_text(encoding="utf-8")
    return bool(re.search(rf"(?ms)^\[\[bin\]\].*?^name\s*=\s*['\"]{re.escape(bin_name)}['\"]", text))


def run_self_tests() -> int:
    failures: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "src").mkdir()
        (root / "tests").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname = "light-terminal"\nversion = "0.0.0"\nedition = "2024"\n\n[[bin]]\nname = "lterm"\npath = "src/main.rs"\n',
            encoding="utf-8",
        )
        (root / "src" / "main.rs").write_text(
            '#[test]\nfn exact_contract_test() {}\nfn exact_contract_test_typo() {}\n#[test]\nasync fn async_contract_test() {}\n#[test]\npub(crate) fn visible_contract_test() {}\n',
            encoding="utf-8",
        )
        (root / "tests" / "cli_smoke.rs").write_text(
            '#[test]\nfn integration_contract_test() {}\nfn helper_only() {}\n',
            encoding="utf-8",
        )

        valid_surface = {
            "name": "stable-output",
            "classification": "stable",
            "raw_stream_policy": "sanitized-output-only",
            "text_output_stability": "stable",
            "json_output_stability": "none",
            "schema_path": None,
        }
        valid_entry = {
            "command": "lterm valid",
            "aliases": [],
            "text_output_stability": "best-effort",
            "json_output_stability": "none",
            "schema_path": None,
            "tests": [
                "cargo test --bin lterm exact_contract_test",
                "cargo test --bin lterm async_contract_test",
                "cargo test --bin lterm visible_contract_test",
                "cargo test --test cli_smoke integration_contract_test",
            ],
            "docs_owner": "docs/public-contract.md",
            "classification": "stable",
            "raw_stream_policy": "sanitized-output-only",
            "surface_contracts": [valid_surface],
            "stability_scope": "stable-output owns sanitized output",
        }

        def expect_errors(name: str, entry: dict[str, Any], needles: list[str]) -> None:
            errors = validate_manifest({"entries": [entry]}, root)
            joined = "\n".join(errors)
            if not errors or not all(needle in joined for needle in needles):
                failures.append(f"{name}: expected {needles!r}, got {errors!r}")

        errors = validate_manifest({"entries": [valid_entry]}, root)
        if errors:
            failures.append(f"valid manifest unexpectedly failed: {errors!r}")

        invalid_surface = deepcopy(valid_entry)
        invalid_surface["surface_contracts"] = [{"surface": "not-schema-valid"}]
        expect_errors(invalid_surface["command"], invalid_surface, ["unknown field(s): surface", "missing required field(s)"])

        empty_scope = deepcopy(valid_entry)
        empty_scope["stability_scope"] = ""
        expect_errors("empty stability_scope", empty_scope, ["stability_scope must be a non-empty string"])

        missing_test = deepcopy(valid_entry)
        missing_test["tests"] = ["cargo test --bin lterm exact_contract_test_missing"]
        expect_errors("missing exact test", missing_test, ["tests name no real target"])

        helper_test = deepcopy(valid_entry)
        helper_test["tests"] = ["cargo test --test cli_smoke helper_only"]
        expect_errors("helper-only test", helper_test, ["tests name no real target"])

    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        return 1
    print("PASS: validate_contract_manifest.py self-tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
