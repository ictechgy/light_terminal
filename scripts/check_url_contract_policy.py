#!/usr/bin/env python3
"""Guard lterm urls policy against code/schema/docs drift.

The URL extractor intentionally exposes a stable public contract: runtime caps live
in Rust, the JSON shape lives in schema, and user-facing guarantees live in the
manifest/docs. This script keeps those surfaces synchronized so future URL policy
edits fail loudly instead of drifting silently.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
EXPECTED_SCHEMA_PATTERN = r"^[Hh][Tt][Tt][Pp][Ss]?://[!-~]+$"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def rust_usize_const(source: str, name: str) -> int:
    pattern = re.compile(rf"^\s*(?:pub\s+)?const\s+{re.escape(name)}\s*:\s*usize\s*=\s*(\d+)\s*;", re.MULTILINE)
    match = pattern.search(source)
    if not match:
        raise ValueError(f"missing Rust usize const {name}")
    return int(match.group(1))


def require(condition: bool, message: str, errors: list[str]) -> None:
    if not condition:
        errors.append(message)


def normalized(value: str) -> str:
    return " ".join(value.split())


def require_text(text: str, needle: str, label: str, errors: list[str]) -> None:
    require(normalized(needle) in normalized(text), f"{label}: missing {needle!r}", errors)


def find_manifest_entry(manifest: dict, command: str) -> dict:
    for entry in manifest.get("entries", []):
        if entry.get("command") == command:
            return entry
    raise ValueError(f"manifest entry not found for {command!r}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check lterm urls policy drift")
    parser.add_argument("--client", default="src/client.rs")
    parser.add_argument("--schema", default="docs/schemas/urls.schema.json")
    parser.add_argument("--manifest", default="docs/contract-manifest.json")
    parser.add_argument("--public-contract", default="docs/public-contract.md")
    parser.add_argument("--readme", default="README.md")
    parser.add_argument("--readme-ko", default="README.ko.md")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="accepted by contract-manifest validation; runs the same repo policy check",
    )
    args = parser.parse_args(argv)

    client_path = REPO_ROOT / args.client
    schema_path = REPO_ROOT / args.schema
    manifest_path = REPO_ROOT / args.manifest
    public_contract_path = REPO_ROOT / args.public_contract
    readme_path = REPO_ROOT / args.readme
    readme_ko_path = REPO_ROOT / args.readme_ko

    errors: list[str] = []

    try:
        client = read(client_path)
        max_urls = rust_usize_const(client, "MAX_EXTRACTED_URLS")
        max_bytes = rust_usize_const(client, "MAX_EXTRACTED_URL_BYTES")
    except Exception as exc:  # noqa: BLE001 - report all drift findings uniformly
        errors.append(f"{client_path}: {exc}")
        max_urls = -1
        max_bytes = -1

    try:
        schema = json.loads(read(schema_path))
        items = schema.get("items", {})
        require(schema.get("type") == "array", f"{schema_path}: type must be array", errors)
        require(schema.get("maxItems") == max_urls, f"{schema_path}: maxItems {schema.get('maxItems')!r} != MAX_EXTRACTED_URLS {max_urls}", errors)
        require(schema.get("uniqueItems") is True, f"{schema_path}: uniqueItems must be true", errors)
        require(items.get("type") == "string", f"{schema_path}: items.type must be string", errors)
        require(items.get("maxLength") == max_bytes, f"{schema_path}: items.maxLength {items.get('maxLength')!r} != MAX_EXTRACTED_URL_BYTES {max_bytes}", errors)
        require(items.get("pattern") == EXPECTED_SCHEMA_PATTERN, f"{schema_path}: items.pattern changed from expected ASCII http(s) pattern", errors)
    except Exception as exc:  # noqa: BLE001
        errors.append(f"{schema_path}: {exc}")

    try:
        manifest = json.loads(read(manifest_path))
        entry = find_manifest_entry(manifest, "lterm urls")
        scope = entry.get("stability_scope", "")
        tests = entry.get("tests", [])
        require(entry.get("schema_path") == args.schema, f"{manifest_path}: lterm urls schema_path must be {args.schema}", errors)
        require(
            "python3 scripts/check_url_contract_policy.py --self-test" in tests,
            f"{manifest_path}: lterm urls tests must include check_url_contract_policy.py --self-test",
            errors,
        )
        for needle in [
            f"capped at {max_urls} rows",
            f"longer than {max_bytes} bytes",
            "unique ASCII URL tokens",
            "ASCII-case-insensitively",
            "--last emits the newest valid",
            "short-lived authentication secrets",
        ]:
            require_text(scope, needle, f"{manifest_path}: lterm urls stability_scope", errors)
    except Exception as exc:  # noqa: BLE001
        errors.append(f"{manifest_path}: {exc}")

    text_checks = [
        (public_contract_path, [f"capped at {max_urls} rows", f"longer than {max_bytes} bytes", "ASCII-case-insensitively", "newest valid within-length URL occurrence"]),
        (readme_path, [f"{max_urls} unique ASCII URL tokens", f"{max_bytes} bytes", "Treat extracted links as untrusted terminal output"]),
        (readme_ko_path, [f"unique ASCII URL token {max_urls}개", f"{max_bytes} byte", "신뢰할 수 없는 terminal output"]),
    ]
    for path, needles in text_checks:
        try:
            text = read(path)
            for needle in needles:
                require_text(text, needle, str(path), errors)
        except Exception as exc:  # noqa: BLE001
            errors.append(f"{path}: {exc}")

    if errors:
        print("FAIL URL contract policy drift:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    print(
        f"PASS URL contract policy: MAX_EXTRACTED_URLS={max_urls}, "
        f"MAX_EXTRACTED_URL_BYTES={max_bytes}, schema/docs/manifest aligned"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
