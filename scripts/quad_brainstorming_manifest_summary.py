#!/usr/bin/env python3
"""Summarize local quad-brainstorming manifests without telemetry.

The helper reads redacted `manifest.json` files created by the planned
quad-brainstorming artifact policy. It never contacts the network and it ignores
raw prompt/context/provider-output fields if a malformed manifest contains them.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path
from typing import Any, Iterable

DEFAULT_ARTIFACT_DIR = Path(".codex/artifacts/quad-brainstorming")
RAW_FIELD_NAMES = {
    "raw_prompt",
    "raw_prompts",
    "raw_context",
    "raw_provider_output",
    "raw_provider_stdout",
    "raw_provider_stderr",
    "transcript",
    "transcripts",
    "provider_request_dump",
}
PROVIDERS = ("claude", "codex", "gemini", "forge")
PRESETS = ("architecture-review", "risk-scan", "decision-record", "product-strategy", "custom")
QUORUMS = ("full", "strong", "partial", "solo-degraded", "no-quorum-dry-result")
CONFIDENCE_CAPS = ("high", "medium", "low", "n/a")
TELEMETRY_STATUSES = ("disabled", "explicit-export-only")
PROVIDER_STATUSES = (
    "ready",
    "usable",
    "missing",
    "auth-required",
    "unsafe-mode",
    "timeout",
    "empty-output",
    "schema-invalid",
    "off-target",
)
USABLE_PROVIDER_STATUSES = {"ready", "usable"}
REQUIRED_MANIFEST_FIELDS = (
    "schema_version",
    "session_id",
    "created_at",
    "preset",
    "context_summary",
    "redaction",
    "provider_statuses",
    "quorum",
    "confidence_cap",
    "persistence_policy",
    "artifact_policy",
    "telemetry",
)


def safe_enum(value: Any, allowed: Iterable[str], default: str = "unknown") -> str:
    if not isinstance(value, str):
        return default
    allowed_values = set(allowed)
    if value not in allowed_values:
        return default
    return value


def iter_manifest_paths(inputs: Iterable[Path], *, strict: bool = False) -> tuple[list[Path], list[str]]:
    paths: list[Path] = []
    errors: list[str] = []
    for item in inputs:
        if item.is_file():
            paths.append(item)
        elif item.is_dir():
            direct = item / "manifest.json"
            if direct.is_file():
                paths.append(direct)
            paths.extend(sorted(item.glob("*/manifest.json")))
        elif strict:
            errors.append(f"{item}: path does not exist or is not a file/directory")
    return sorted(dict.fromkeys(paths)), errors


def safe_load_manifest(path: Path) -> tuple[dict[str, Any] | None, str | None]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        return None, f"{path}: {exc}"
    if not isinstance(data, dict):
        return None, f"{path}: manifest root must be an object"
    return data, None


def iter_raw_field_paths(value: Any, path: str = "") -> Iterable[str]:
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}" if path else str(key)
            if key in RAW_FIELD_NAMES:
                yield child_path
            yield from iter_raw_field_paths(child, child_path)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            child_path = f"{path}[{index}]" if path else f"[{index}]"
            yield from iter_raw_field_paths(child, child_path)


def validate_manifest_shape(path: Path, manifest: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    for field in REQUIRED_MANIFEST_FIELDS:
        if field not in manifest:
            errors.append(f"{path}: missing required field {field!r}")
    return errors


def summarize(paths: list[Path], input_errors: Iterable[str] = ()) -> dict[str, Any]:
    manifests = []
    errors = list(input_errors)
    invalid_manifest_count = 0
    raw_field_occurrences: Counter[str] = Counter()
    preset_counts: Counter[str] = Counter()
    quorum_counts: Counter[str] = Counter()
    confidence_counts: Counter[str] = Counter()
    telemetry_counts: Counter[str] = Counter()
    provider_status_counts: dict[str, Counter[str]] = {provider: Counter() for provider in PROVIDERS}
    skipped_provider_reasons: Counter[str] = Counter()
    saved_adr_count = 0

    for path in paths:
        manifest, error = safe_load_manifest(path)
        if error:
            errors.append(error)
            invalid_manifest_count += 1
            continue
        assert manifest is not None
        raw_field_occurrences.update(iter_raw_field_paths(manifest))
        shape_errors = validate_manifest_shape(path, manifest)
        if shape_errors:
            errors.extend(shape_errors)
            invalid_manifest_count += 1
            continue
        manifests.append(path)
        preset_counts.update([safe_enum(manifest.get("preset"), PRESETS)])
        quorum_counts.update([safe_enum(manifest.get("quorum"), QUORUMS)])
        confidence_counts.update([safe_enum(manifest.get("confidence_cap"), CONFIDENCE_CAPS)])
        telemetry = manifest.get("telemetry")
        if isinstance(telemetry, dict):
            telemetry_counts.update([safe_enum(telemetry.get("status"), TELEMETRY_STATUSES)])
        else:
            telemetry_counts.update(["unknown"])

        artifact_policy = manifest.get("artifact_policy")
        if isinstance(artifact_policy, dict):
            saved = artifact_policy.get("saved", [])
            if isinstance(saved, list) and "adr.md" in saved:
                saved_adr_count += 1

        statuses = manifest.get("provider_statuses", [])
        if isinstance(statuses, list):
            for status in statuses:
                if not isinstance(status, dict):
                    continue
                provider = safe_enum(status.get("provider"), PROVIDERS)
                state = safe_enum(status.get("status"), PROVIDER_STATUSES)
                provider_status_counts.setdefault(provider, Counter()).update([state])
                if state not in USABLE_PROVIDER_STATUSES:
                    skipped_provider_reasons.update([f"{provider}:{state}"])

    return {
        "manifest_count": len(manifests),
        "invalid_manifest_count": invalid_manifest_count,
        "error_count": len(errors),
        "errors": errors,
        "preset_counts": dict(sorted(preset_counts.items())),
        "quorum_counts": dict(sorted(quorum_counts.items())),
        "confidence_counts": dict(sorted(confidence_counts.items())),
        "provider_status_counts": {
            provider: dict(sorted(counts.items())) for provider, counts in provider_status_counts.items()
        },
        "skipped_provider_reasons": dict(sorted(skipped_provider_reasons.items())),
        "saved_adr_count": saved_adr_count,
        "telemetry_counts": dict(sorted(telemetry_counts.items())),
        "raw_fields_ignored": dict(sorted(raw_field_occurrences.items())),
    }


def print_text(summary: dict[str, Any]) -> None:
    print(f"manifests\t{summary['manifest_count']}")
    print(f"errors\t{summary['error_count']}")
    if summary["errors"]:
        print("[errors]", file=sys.stderr)
        for error in summary["errors"]:
            print(error, file=sys.stderr)
    print(f"saved_adr_count\t{summary['saved_adr_count']}")
    for section in [
        "preset_counts",
        "quorum_counts",
        "confidence_counts",
        "telemetry_counts",
        "skipped_provider_reasons",
        "raw_fields_ignored",
    ]:
        values = summary[section]
        if values:
            print(f"[{section}]")
            for key, value in values.items():
                print(f"{key}\t{value}")
    print("[provider_status_counts]")
    for provider, counts in summary["provider_status_counts"].items():
        if counts:
            rendered = ",".join(f"{status}:{count}" for status, count in counts.items())
            print(f"{provider}\t{rendered}")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        type=Path,
        help="manifest files or artifact directories; defaults to .codex/artifacts/quad-brainstorming",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON summary")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    explicit_paths = bool(args.paths)
    inputs = args.paths or [DEFAULT_ARTIFACT_DIR]
    paths, input_errors = iter_manifest_paths(inputs, strict=explicit_paths)
    summary = summarize(paths, input_errors)
    if args.json:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print_text(summary)
    return 1 if summary["error_count"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
