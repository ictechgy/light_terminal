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
import tempfile
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
EXPECTED_SCHEMA_PATTERN = r"^[Hh][Tt][Tt][Pp][Ss]?://[!-~]+$"
MANIFEST_SELF_TEST = "python3 scripts/check_url_contract_policy.py --self-test"


@dataclass(frozen=True)
class PolicyInputs:
    root: Path
    client: str
    schema: str
    manifest: str
    public_contract: str
    readme: str
    readme_ko: str

    def path(self, value: str) -> Path:
        path = Path(value)
        return path if path.is_absolute() else self.root / path


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


def check_policy(inputs: PolicyInputs) -> tuple[list[str], int, int]:
    client_path = inputs.path(inputs.client)
    schema_path = inputs.path(inputs.schema)
    manifest_path = inputs.path(inputs.manifest)
    public_contract_path = inputs.path(inputs.public_contract)
    readme_path = inputs.path(inputs.readme)
    readme_ko_path = inputs.path(inputs.readme_ko)

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
        require(
            schema.get("maxItems") == max_urls,
            f"{schema_path}: maxItems {schema.get('maxItems')!r} != MAX_EXTRACTED_URLS {max_urls}",
            errors,
        )
        require(schema.get("uniqueItems") is True, f"{schema_path}: uniqueItems must be true", errors)
        require(items.get("type") == "string", f"{schema_path}: items.type must be string", errors)
        require(
            items.get("maxLength") == max_bytes,
            f"{schema_path}: items.maxLength {items.get('maxLength')!r} != MAX_EXTRACTED_URL_BYTES {max_bytes}",
            errors,
        )
        require(
            items.get("pattern") == EXPECTED_SCHEMA_PATTERN,
            f"{schema_path}: items.pattern changed from expected ASCII http(s) pattern",
            errors,
        )
    except Exception as exc:  # noqa: BLE001
        errors.append(f"{schema_path}: {exc}")

    try:
        manifest = json.loads(read(manifest_path))
        entry = find_manifest_entry(manifest, "lterm urls")
        scope = entry.get("stability_scope", "")
        tests = entry.get("tests", [])
        require(entry.get("schema_path") == inputs.schema, f"{manifest_path}: lterm urls schema_path must be {inputs.schema}", errors)
        require(
            MANIFEST_SELF_TEST in tests,
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
        (
            public_contract_path,
            [
                f"capped at {max_urls} rows",
                f"longer than {max_bytes} bytes",
                "ASCII-case-insensitively",
                "newest valid within-length URL occurrence",
            ],
        ),
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

    return errors, max_urls, max_bytes


def write_fixture(root: Path, *, max_urls: int = 17, max_bytes: int = 99) -> None:
    (root / "src").mkdir(parents=True)
    (root / "docs" / "schemas").mkdir(parents=True)
    (root / "scripts").mkdir(parents=True)
    (root / "src" / "client.rs").write_text(
        f"const MAX_EXTRACTED_URLS: usize = {max_urls};\nconst MAX_EXTRACTED_URL_BYTES: usize = {max_bytes};\n",
        encoding="utf-8",
    )
    (root / "docs" / "schemas" / "urls.schema.json").write_text(
        json.dumps(
            {
                "type": "array",
                "maxItems": max_urls,
                "uniqueItems": True,
                "items": {"type": "string", "maxLength": max_bytes, "pattern": EXPECTED_SCHEMA_PATTERN},
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    (root / "docs" / "contract-manifest.json").write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "command": "lterm urls",
                        "schema_path": "docs/schemas/urls.schema.json",
                        "tests": [MANIFEST_SELF_TEST],
                        "stability_scope": (
                            f"unique ASCII URL tokens capped at {max_urls} rows; complete raw candidates "
                            f"longer than {max_bytes} bytes are skipped; extraction matches schemes "
                            "ASCII-case-insensitively; --last emits the newest valid URL; extracted URLs "
                            "may include short-lived authentication secrets."
                        ),
                    }
                ]
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    (root / "docs" / "public-contract.md").write_text(
        f"Urls are capped at {max_urls} rows. Tokens longer than {max_bytes} bytes are skipped. "
        "Matching is ASCII-case-insensitively. --last returns the newest valid within-length URL occurrence.\n",
        encoding="utf-8",
    )
    (root / "README.md").write_text(
        f"`lterm urls` returns {max_urls} unique ASCII URL tokens and skips tokens over {max_bytes} bytes. "
        "Treat extracted links as untrusted terminal output.\n",
        encoding="utf-8",
    )
    (root / "README.ko.md").write_text(
        f"`lterm urls`는 unique ASCII URL token {max_urls}개와 {max_bytes} byte 제한을 둡니다. "
        "신뢰할 수 없는 terminal output 으로 취급하세요.\n",
        encoding="utf-8",
    )


def fixture_inputs(root: Path) -> PolicyInputs:
    return PolicyInputs(
        root=root,
        client="src/client.rs",
        schema="docs/schemas/urls.schema.json",
        manifest="docs/contract-manifest.json",
        public_contract="docs/public-contract.md",
        readme="README.md",
        readme_ko="README.ko.md",
    )


def assert_self_test_case(name: str, root: Path, expect_error: str | None = None) -> list[str]:
    errors, _, _ = check_policy(fixture_inputs(root))
    if expect_error is None:
        return [] if not errors else [f"{name}: expected pass, got {errors!r}"]
    if any(expect_error in error for error in errors):
        return []
    return [f"{name}: expected error containing {expect_error!r}, got {errors!r}"]


def run_self_test() -> int:
    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="lterm-url-policy-self-test-") as tmp:
        root = Path(tmp)

        passing = root / "passing"
        write_fixture(passing)
        failures.extend(assert_self_test_case("passing fixture", passing))

        schema_mismatch = root / "schema-mismatch"
        write_fixture(schema_mismatch)
        schema_path = schema_mismatch / "docs" / "schemas" / "urls.schema.json"
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        schema["maxItems"] = 18
        schema_path.write_text(json.dumps(schema), encoding="utf-8")
        failures.extend(assert_self_test_case("schema cap mismatch", schema_mismatch, "maxItems"))

        missing_manifest_test = root / "missing-manifest-test"
        write_fixture(missing_manifest_test)
        manifest_path = missing_manifest_test / "docs" / "contract-manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["entries"][0]["tests"] = []
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        failures.extend(assert_self_test_case("missing manifest self-test", missing_manifest_test, "tests must include"))

        missing_readme_warning = root / "missing-readme-warning"
        write_fixture(missing_readme_warning)
        (missing_readme_warning / "README.md").write_text("No warning here.\n", encoding="utf-8")
        failures.extend(assert_self_test_case("missing README warning", missing_readme_warning, "untrusted terminal output"))

    if failures:
        print("FAIL URL contract policy self-test:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("PASS URL contract policy self-test: positive and negative fixtures behaved as expected")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check lterm urls policy drift")
    parser.add_argument("--repo-root", default=str(REPO_ROOT))
    parser.add_argument("--client", default="src/client.rs")
    parser.add_argument("--schema", default="docs/schemas/urls.schema.json")
    parser.add_argument("--manifest", default="docs/contract-manifest.json")
    parser.add_argument("--public-contract", default="docs/public-contract.md")
    parser.add_argument("--readme", default="README.md")
    parser.add_argument("--readme-ko", default="README.ko.md")
    parser.add_argument("--self-test", action="store_true", help="run fixture-based regression tests for this checker")
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_test()

    inputs = PolicyInputs(
        root=Path(args.repo_root).resolve(),
        client=args.client,
        schema=args.schema,
        manifest=args.manifest,
        public_contract=args.public_contract,
        readme=args.readme,
        readme_ko=args.readme_ko,
    )
    errors, max_urls, max_bytes = check_policy(inputs)

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
