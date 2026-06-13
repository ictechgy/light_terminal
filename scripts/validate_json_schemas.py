#!/usr/bin/env python3
"""Validate lterm JSON schemas and sample stable JSON command outputs.

The implementation intentionally supports the JSON Schema subset used by the
repository schemas so it can run in CI with only the Python standard library.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

Json = Any

DEFAULT_SAMPLES: dict[str, list[dict[str, Any]]] = {
    "lterm sessions": [
        {
            "setup": [["start", "-d", "-n", "schema-sessions", "--", "sh", "-lc", "printf READY; sleep 5"]],
            "argv": ["sessions", "--json"],
        }
    ],
    "lterm processes": [
        {
            "setup": [["start", "-d", "-n", "schema-processes", "--", "sh", "-lc", "sleep 5"]],
            "argv": ["processes", "schema-processes", "--json"],
        }
    ],
    "lterm wait": [
        {
            "setup": [["start", "-d", "-n", "schema-wait", "--", "sh", "-lc", "printf READY; sleep 5"]],
            "argv": ["wait", "schema-wait", "--contains", "READY", "--timeout", "5s", "--json"],
        }
    ],
    "lterm watch": [
        {
            "setup": [["start", "-d", "-n", "schema-watch", "--", "sh", "-lc", "printf READY; sleep 5"]],
            "argv": ["watch", "schema-watch", "--contains", "READY", "--timeout", "5s", "--json"],
        }
    ],
    "lterm doctor": [{"argv": ["doctor", "--json"]}],
    "lterm agents": [{"argv": ["agents", "--json"]}],
    "lterm tmux-compat list-commands": [{"argv": ["tmux-compat", "list-commands", "--json"]}],
}


class SchemaError(ValueError):
    pass


def load_json(path: Path) -> Json:
    try:
        with path.open(encoding="utf-8") as fh:
            return json.load(fh)
    except json.JSONDecodeError as exc:
        raise SchemaError(f"{path}: invalid JSON: {exc}") from exc


def build_schema_store(schema_dir: Path) -> dict[str, Json]:
    store: dict[str, Json] = {}
    for path in sorted(schema_dir.glob("*.schema.json")):
        schema = load_json(path)
        if not isinstance(schema, dict):
            raise SchemaError(f"{path}: schema root must be an object")
        store[path.name] = schema
        if isinstance(schema.get("$id"), str):
            store[schema["$id"]] = schema
    return store


def type_matches(value: Json, expected: str) -> bool:
    if expected == "object":
        return isinstance(value, dict)
    if expected == "array":
        return isinstance(value, list)
    if expected == "string":
        return isinstance(value, str)
    if expected == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if expected == "number":
        return (isinstance(value, int) or isinstance(value, float)) and not isinstance(value, bool)
    if expected == "boolean":
        return isinstance(value, bool)
    if expected == "null":
        return value is None
    raise SchemaError(f"unsupported schema type {expected!r}")


def split_ref(ref: str) -> tuple[str, str]:
    if not isinstance(ref, str) or not ref:
        raise SchemaError(f"unsupported $ref {ref!r}")
    base, marker, fragment = ref.partition("#")
    return base, f"#{fragment}" if marker else ""


def pointer_token(token: str) -> str:
    return token.replace("~1", "/").replace("~0", "~")


def resolve_pointer(document: Json, fragment: str, ref: str) -> Json:
    if fragment in ("", "#"):
        return document
    if not fragment.startswith("#/"):
        raise SchemaError(f"unsupported $ref fragment {fragment!r} in {ref!r}")
    target = document
    for raw_token in fragment[2:].split("/"):
        token = pointer_token(raw_token)
        if isinstance(target, dict) and token in target:
            target = target[token]
        elif isinstance(target, list) and token.isdecimal() and int(token) < len(target):
            target = target[int(token)]
        else:
            raise SchemaError(f"unresolved $ref fragment {fragment!r} in {ref!r}")
    return target


def is_url_ref(base: str) -> bool:
    parsed = urlsplit(base)
    return bool(parsed.scheme or parsed.netloc)


def load_ref_base(base: str, schema_dir: Path, store: dict[str, Json], current_root: Json | None) -> Json:
    if base == "":
        if current_root is None:
            raise SchemaError("internal $ref requires a current schema root")
        return current_root

    if is_url_ref(base):
        if base in store:
            return store[base]
        raise SchemaError(f"unresolved $ref {base!r}")

    raw_path = Path(base)
    if raw_path.is_absolute():
        raise SchemaError(f"$ref {base!r} must be relative to schema_dir")
    schema_root = schema_dir.resolve()
    path = (schema_root / raw_path).resolve()
    try:
        path.relative_to(schema_root)
    except ValueError as exc:
        raise SchemaError(f"$ref {base!r} escapes schema_dir") from exc
    if path.is_file():
        schema = load_json(path)
        store[base] = schema
        store[path.name] = schema
        if isinstance(schema, dict) and isinstance(schema.get("$id"), str):
            store[schema["$id"]] = schema
        return schema
    if not any(sep in base for sep in ("/", "\\")) and ".." not in raw_path.parts and base in store:
        return store[base]
    raise SchemaError(f"unresolved $ref {base!r}")


def resolve_ref(
    ref: str, schema_dir: Path, store: dict[str, Json], current_root: Json | None = None
) -> tuple[Json, Json]:
    base, fragment = split_ref(ref)
    root = load_ref_base(base, schema_dir, store, current_root)
    return resolve_pointer(root, fragment, ref), root


def validate_value(
    value: Json,
    schema: Json,
    schema_dir: Path,
    store: dict[str, Json],
    path: str = "$",
    root_schema: Json | None = None,
) -> list[str]:
    errors: list[str] = []
    if not isinstance(schema, dict):
        return [f"{path}: schema must be an object"]
    if root_schema is None:
        root_schema = schema
    if "$ref" in schema:
        resolved_schema, resolved_root = resolve_ref(schema["$ref"], schema_dir, store, root_schema)
        return validate_value(value, resolved_schema, schema_dir, store, path, resolved_root)
    if "allOf" in schema:
        branches = schema["allOf"]
        if not isinstance(branches, list):
            errors.append(f"{path}: schema allOf must be an array")
        else:
            for branch in branches:
                errors.extend(validate_value(value, branch, schema_dir, store, path, root_schema))
    if "if" in schema:
        condition_errors = validate_value(value, schema["if"], schema_dir, store, path, root_schema)
        if not condition_errors and "then" in schema:
            errors.extend(validate_value(value, schema["then"], schema_dir, store, path, root_schema))
    if "not" in schema:
        disallowed = schema["not"]
        if not isinstance(disallowed, dict):
            errors.append(f"{path}: schema not must be an object")
        else:
            disallowed_errors = validate_value(value, disallowed, schema_dir, store, path, root_schema)
            if not disallowed_errors:
                errors.append(f"{path}: matched disallowed schema in not")
    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: expected const {schema['const']!r}, got {value!r}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: {value!r} not in enum {schema['enum']!r}")
    if "oneOf" in schema:
        variants = schema["oneOf"]
        if not isinstance(variants, list) or not variants:
            errors.append(f"{path}: schema oneOf must be a non-empty array")
        else:
            matches = 0
            variant_summaries: list[str] = []
            for variant_index, variant in enumerate(variants):
                variant_errors = validate_value(value, variant, schema_dir, store, path, root_schema)
                if variant_errors:
                    variant_summaries.append(f"branch {variant_index}: {'; '.join(variant_errors)}")
                else:
                    matches += 1
            if matches != 1:
                detail = "; ".join(variant_summaries[:3])
                errors.append(f"{path}: expected exactly one oneOf branch to match, matched {matches}" + (f"; {detail}" if detail else ""))
    if "type" in schema:
        expected = schema["type"]
        expected_types = expected if isinstance(expected, list) else [expected]
        if not any(type_matches(value, item) for item in expected_types):
            errors.append(f"{path}: expected type {expected!r}, got {type(value).__name__}")
            return errors
    if isinstance(value, dict):
        required = schema.get("required", [])
        if not isinstance(required, list):
            errors.append(f"{path}: schema required must be an array")
        else:
            for key in required:
                if key not in value:
                    errors.append(f"{path}: missing required property {key!r}")
        properties = schema.get("properties", {})
        if not isinstance(properties, dict):
            errors.append(f"{path}: schema properties must be an object")
            properties = {}
        for key, subvalue in value.items():
            if key in properties:
                errors.extend(validate_value(subvalue, properties[key], schema_dir, store, f"{path}.{key}", root_schema))
            else:
                additional = schema.get("additionalProperties", True)
                if additional is False:
                    errors.append(f"{path}: unexpected property {key!r}")
                elif isinstance(additional, dict):
                    errors.extend(validate_value(subvalue, additional, schema_dir, store, f"{path}.{key}", root_schema))
                elif additional is not True:
                    errors.append(f"{path}: schema additionalProperties must be boolean or object")
    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            errors.append(f"{path}: expected at least {schema['minItems']} items, got {len(value)}")
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            errors.append(f"{path}: expected at most {schema['maxItems']} items, got {len(value)}")
        if schema.get("uniqueItems") is True:
            seen_items: set[str] = set()
            for idx, item in enumerate(value):
                try:
                    fingerprint = json.dumps(item, sort_keys=True, separators=(",", ":"))
                except TypeError:
                    fingerprint = repr(item)
                if fingerprint in seen_items:
                    errors.append(f"{path}[{idx}]: duplicate item violates uniqueItems")
                    break
                seen_items.add(fingerprint)
        items = schema.get("items")
        if items is not None:
            for idx, item in enumerate(value):
                errors.extend(validate_value(item, items, schema_dir, store, f"{path}[{idx}]", root_schema))
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            errors.append(f"{path}: expected length at least {schema['minLength']}, got {len(value)}")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            errors.append(f"{path}: expected length at most {schema['maxLength']}, got {len(value)}")
        if "pattern" in schema:
            pattern = schema["pattern"]
            if not isinstance(pattern, str):
                errors.append(f"{path}: schema pattern must be a string")
            else:
                try:
                    matches = re.search(pattern, value) is not None
                except re.error as exc:
                    errors.append(f"{path}: invalid schema pattern {pattern!r}: {exc}")
                else:
                    if not matches:
                        errors.append(f"{path}: {value!r} does not match pattern {pattern!r}")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            errors.append(f"{path}: {value!r} is below minimum {schema['minimum']!r}")
    return errors


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


def run_lterm(prefix: list[str], argv: list[str], env: dict[str, str], cwd: Path, timeout: int = 15) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [*prefix, *argv],
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )


def sample_specs(entry: dict[str, Any]) -> list[dict[str, Any]]:
    configured = entry.get("json_samples")
    if configured is not None:
        if not isinstance(configured, list):
            raise SchemaError(f"{entry.get('command')}: json_samples must be an array")
        return configured
    return DEFAULT_SAMPLES.get(entry["command"], [])


def validate_sample(entry: dict[str, Any], schema: Json, schema_dir: Path, store: dict[str, Json], prefix: list[str], repo_root: Path) -> list[str]:
    errors: list[str] = []
    specs = sample_specs(entry)
    if not specs:
        return [f"{entry['command']}: no json sample configured for stable JSON output"]
    for idx, spec in enumerate(specs):
        if not isinstance(spec, dict) or not isinstance(spec.get("argv"), list):
            errors.append(f"{entry['command']}: json sample {idx} must have argv array")
            continue
        with tempfile.TemporaryDirectory(prefix="lterm-schema-") as tmp:
            env = os.environ.copy()
            env["LTERM_RUNTIME_DIR"] = str(Path(tmp) / "run")
            env["LTERM_DATA_DIR"] = str(Path(tmp) / "data")
            env["TMPDIR"] = tmp
            scrub_lterm_session_env(env)
            try:
                for setup in spec.get("setup", []):
                    proc = run_lterm(prefix, [str(part) for part in setup], env, repo_root)
                    if proc.returncode != 0:
                        errors.append(f"{entry['command']}: setup {setup!r} failed: {proc.stderr.strip()}")
                        break
                else:
                    proc = run_lterm(prefix, [str(part) for part in spec["argv"]], env, repo_root)
                    if proc.returncode != 0:
                        errors.append(f"{entry['command']}: sample argv {spec['argv']!r} exited {proc.returncode}: {proc.stderr.strip()}")
                    else:
                        try:
                            output = json.loads(proc.stdout)
                        except json.JSONDecodeError as exc:
                            errors.append(f"{entry['command']}: sample output is not JSON: {exc}; stdout={proc.stdout[:200]!r}")
                        else:
                            errors.extend(validate_value(output, schema, schema_dir, store, f"{entry['command']} sample {idx}"))
            finally:
                run_lterm(prefix, ["shutdown"], env, repo_root, timeout=5)
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True, help="Path to docs/contract-manifest.json")
    parser.add_argument("--schema-dir", type=Path, required=True, help="Directory containing *.schema.json files")
    parser.add_argument("--lterm-bin", help="Path to a prebuilt lterm binary; defaults to LTERM_BIN, target/debug, target/release, then cargo run")
    args = parser.parse_args()

    errors: list[str] = []
    manifest = load_json(args.manifest)
    if not isinstance(manifest, dict) or not isinstance(manifest.get("entries"), list):
        print("ERROR: manifest entries must be an array", file=sys.stderr)
        return 1
    store = build_schema_store(args.schema_dir)
    repo_root = repo_root_for(args.manifest)
    prefix = lterm_prefix(repo_root, args.lterm_bin)

    stable_entries = [entry for entry in manifest["entries"] if entry.get("json_output_stability") == "stable"]
    for entry in stable_entries:
        schema_path = entry.get("schema_path")
        if not isinstance(schema_path, str):
            errors.append(f"{entry.get('command')}: stable JSON output requires string schema_path")
            continue
        schema_file = repo_root / schema_path
        if not schema_file.is_file():
            errors.append(f"{entry.get('command')}: schema file missing: {schema_path}")
            continue
        try:
            schema = load_json(schema_file)
            # Validate the schema file against the subset validator using a harmless impossible value
            # only enough to ensure references are resolvable during sample validation.
            errors.extend(validate_sample(entry, schema, args.schema_dir, store, prefix, repo_root))
        except (SchemaError, subprocess.TimeoutExpired) as exc:
            errors.append(f"{entry.get('command')}: {exc}")

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print(f"PASS JSON schemas: {len(stable_entries)} stable JSON command(s) sampled")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
