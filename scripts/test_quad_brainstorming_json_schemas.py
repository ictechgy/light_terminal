#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from types import ModuleType


REPO_ROOT = Path(__file__).resolve().parents[1]
SCHEMA_DIR = REPO_ROOT / "docs" / "schemas"
VALIDATOR_PATH = REPO_ROOT / "scripts" / "validate_json_schemas.py"


def load_validator() -> ModuleType:
    spec = importlib.util.spec_from_file_location("validate_json_schemas", VALIDATOR_PATH)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


VALIDATOR = load_validator()


def base_track(status: str = "usable") -> dict[str, object]:
    return {
        "provider": "codex",
        "lens": "local synthesis",
        "status": status,
        "top_ideas": [],
        "wild_cards": [],
        "risks": [],
        "assumptions_to_validate": [],
        "recommended_next_experiments": [],
        "decision_matrix": [],
        "final_stance": "Proceed locally.",
    }


class QuadBrainstormingJsonSchemaValidatorTests(unittest.TestCase):
    def test_ref_fragments_allof_conditionals_and_additional_properties_schema(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-schema-validator-test-") as tmp:
            schema_dir = Path(tmp)
            (schema_dir / "defs.schema.json").write_text(
                json.dumps(
                    {
                        "$id": "https://example.invalid/defs.schema.json",
                        "definitions": {
                            "non_empty": {"type": "string", "minLength": 1},
                        },
                    }
                ),
                encoding="utf-8",
            )
            store = VALIDATOR.build_schema_store(schema_dir)
            schema = {
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": {"enum": ["ok", "failed"]},
                    "name": {"$ref": "defs.schema.json#/definitions/non_empty"},
                    "failure_class": {"enum": ["timeout"]},
                },
                "additionalProperties": {"type": "string"},
                "allOf": [
                    {
                        "if": {"properties": {"kind": {"const": "failed"}}, "required": ["kind"]},
                        "then": {"required": ["failure_class"]},
                    }
                ],
            }

            self.assertEqual(
                VALIDATOR.validate_value(
                    {"kind": "ok", "name": "ready", "note": "extra"},
                    schema,
                    schema_dir,
                    store,
                ),
                [],
            )
            self.assertTrue(
                any(
                    "missing required property 'failure_class'" in error
                    for error in VALIDATOR.validate_value(
                        {"kind": "failed", "name": "ready"},
                        schema,
                        schema_dir,
                        store,
                    )
                )
            )
            self.assertTrue(
                any(
                    "expected type 'string'" in error
                    for error in VALIDATOR.validate_value(
                        {"kind": "ok", "name": "ready", "note": 3},
                        schema,
                        schema_dir,
                        store,
                    )
                )
            )

    def test_ref_traversal_outside_schema_dir_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quad-schema-validator-test-") as tmp:
            schema_dir = Path(tmp) / "schemas"
            schema_dir.mkdir()
            (schema_dir / "outside.schema.json").write_text("{}", encoding="utf-8")
            (Path(tmp) / "outside.schema.json").write_text("{}", encoding="utf-8")
            store = VALIDATOR.build_schema_store(schema_dir)
            with self.assertRaisesRegex(VALIDATOR.SchemaError, "escapes schema_dir"):
                VALIDATOR.resolve_ref("../outside.schema.json", schema_dir, store)
            with self.assertRaisesRegex(VALIDATOR.SchemaError, "unresolved"):
                VALIDATOR.resolve_ref("missing/outside.schema.json", schema_dir, store)

    def test_track_output_status_failure_class_consistency(self) -> None:
        store = VALIDATOR.build_schema_store(SCHEMA_DIR)
        schema = json.loads((SCHEMA_DIR / "quad-brainstorming-track-output.schema.json").read_text(encoding="utf-8"))

        usable = base_track("usable")
        self.assertEqual(VALIDATOR.validate_value(usable, schema, SCHEMA_DIR, store), [])

        usable_with_failure = dict(usable, failure_class="timeout")
        self.assertTrue(
            any(
                "not in enum []" in error
                for error in VALIDATOR.validate_value(usable_with_failure, schema, SCHEMA_DIR, store)
            )
        )

        timeout_without_failure = base_track("timeout")
        self.assertTrue(
            any(
                "missing required property 'failure_class'" in error
                for error in VALIDATOR.validate_value(timeout_without_failure, schema, SCHEMA_DIR, store)
            )
        )

        timeout_with_mismatch = dict(timeout_without_failure, failure_class="auth-required")
        self.assertTrue(
            any(
                "expected const 'timeout'" in error
                for error in VALIDATOR.validate_value(timeout_with_mismatch, schema, SCHEMA_DIR, store)
            )
        )

        timeout_with_match = dict(timeout_without_failure, failure_class="timeout")
        self.assertEqual(VALIDATOR.validate_value(timeout_with_match, schema, SCHEMA_DIR, store), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
