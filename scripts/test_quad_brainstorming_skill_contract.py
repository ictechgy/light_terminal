#!/usr/bin/env python3
"""Regression checks for the installed quad-brainstorming P0 contract.

The quad-brainstorming workflow currently lives as a Codex skill outside this
repository. These tests validate an explicitly installed skill surface until the
skill has a repo-native source package. They are intentionally local/read-only:
no provider CLIs are invoked and no external posting is done.

Set QUAD_BRAINSTORMING_SKILL to test a specific SKILL.md. Without that variable,
the default is the current user's Codex skill install under ~/.codex/skills.
"""

from __future__ import annotations

import os
import re
import unittest
from pathlib import Path


SKILL_PATH_ENV = "QUAD_BRAINSTORMING_SKILL"
DEFAULT_SKILL = Path.home() / ".codex/skills/quad-brainstorming/SKILL.md"
SKILL_PATH = Path(os.environ.get(SKILL_PATH_ENV, DEFAULT_SKILL)).expanduser()


def markdown_section(text: str, heading: str) -> str:
    """Return a level-2 Markdown section while ignoring headings inside fences."""
    lines = text.splitlines(keepends=True)
    start = None
    target = f"## {heading}"
    for index, line in enumerate(lines):
        if line.strip() == target:
            start = index + 1
            break
    if start is None:
        raise AssertionError(f"Missing Markdown section: {target}")

    end = len(lines)
    in_fence = False
    for index in range(start, len(lines)):
        stripped = lines[index].lstrip()
        if stripped.startswith("```"):
            in_fence = not in_fence
        if not in_fence and lines[index].startswith("## "):
            end = index
            break
    return "".join(lines[start:end])


class QuadBrainstormingSkillContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.skill_path = SKILL_PATH
        if not cls.skill_path.is_file():
            message = (
                f"quad-brainstorming skill not found at {cls.skill_path}. "
                f"Set {SKILL_PATH_ENV}=path/to/SKILL.md to run these local contract tests."
            )
            if SKILL_PATH_ENV in os.environ:
                raise AssertionError(message)
            raise unittest.SkipTest(message)
        cls.skill_text = cls.skill_path.read_text(encoding="utf-8")

    def section(self, heading: str) -> str:
        return markdown_section(self.skill_text, heading)

    def assertContainsAll(self, text: str, snippets: list[str]) -> None:
        for snippet in snippets:
            with self.subTest(snippet=snippet):
                self.assertIn(snippet, text)

    def test_default_skill_path_is_portable(self) -> None:
        self.assertEqual(DEFAULT_SKILL, Path.home() / ".codex/skills/quad-brainstorming/SKILL.md")
        self.assertTrue(str(DEFAULT_SKILL).endswith("/.codex/skills/quad-brainstorming/SKILL.md"))

    def test_core_contract_sections_are_present(self) -> None:
        self.assertContainsAll(
            self.skill_text,
            [
                "## P0 Core Contract",
                "## Track Availability and Doctor/Preflight",
                "## Read-Only Trust Envelope",
                "## Compact ADR Report",
                "## Artifact Policy and Cleanup",
            ],
        )

    def test_core_contract_preserves_privacy_first_defaults(self) -> None:
        core_contract = self.section("P0 Core Contract")
        self.assertContainsAll(
            core_contract,
            [
                "Every `quad-brainstorming` surface MUST preserve this contract",
                "**Provider/context disclosure**",
                "**Read-only default**",
                "**No raw persistence by default**",
                "**Explicit artifact consent**",
                "**Redaction before prompts**",
                "**Graceful quorum**",
                "**No external posting**",
                "**Compact ADR output**",
                "Future CLI, MCP, plugin, or GitHub Action surfaces",
                "MUST NOT weaken consent, no-posting, no-raw-persistence, redaction, or read-only defaults",
            ],
        )

    def test_provider_state_taxonomy_keeps_configured_runnable_usable_separate(self) -> None:
        track_availability = self.section("Track Availability and Doctor/Preflight")
        self.assertContainsAll(
            track_availability,
            [
                "Provider status fields:",
                "`configured` | CLI/surface exists and has a detectable adapter",
                "`runnable` | configured plus auth/safety/read-only boundary is acceptable",
                "expected_track_count()",
            ],
        )
        core_contract = self.section("P0 Core Contract")
        self.assertContainsAll(
            core_contract,
            [
                "Provider failure classes are:",
                "Exclude all failed classes from usable-track counts",
                "usable-track denominator is explicit",
            ],
        )
        configured_index = track_availability.index("`configured` | CLI/surface exists")
        runnable_index = track_availability.index("`runnable` | configured plus auth/safety")
        self.assertIn("Provider failure classes are:", core_contract)
        self.assertIn("usable-track denominator is explicit", core_contract)
        self.assertLess(configured_index, runnable_index)

    def test_doctor_and_preflight_never_invoke_providers(self) -> None:
        invocation = self.section("Invocation")
        track_availability = self.section("Track Availability and Doctor/Preflight")
        self.assertContainsAll(
            invocation,
            [
                "`--doctor` | Check local provider availability/auth/safety boundaries",
                "`--dry-run` / `--preflight` | Collect allowed context, redact it, print the Read-Only Trust Envelope",
            ],
        )
        self.assertContainsAll(
            track_availability,
            [
                "`doctor`/preflight checks are local-only",
                "Do not run model prompts",
                "provider commands that consume the user brief/context",
                "Doctor output should be actionable and successful even when every external provider is `missing` or `unsafe-mode`",
            ],
        )
        doctor_row = re.search(r"\| `--doctor` .* \| Never \| None \|", invocation)
        preflight_row = re.search(r"\| `--dry-run` / `--preflight` .* \| Never \|", invocation)
        self.assertIsNotNone(doctor_row)
        self.assertIsNotNone(preflight_row)

    def test_redaction_precedes_prompt_construction_and_external_calls(self) -> None:
        core_contract = self.section("P0 Core Contract")
        trust_envelope = self.section("Read-Only Trust Envelope")
        track_commands = self.section("Track Commands")
        self.assertContainsAll(
            core_contract,
            [
                "Redaction must happen before prompt construction, prompt-file creation, logs, metrics, artifacts, or external adapter/provider calls",
                "collect context into a private temp file, run redaction, then build prompt files only from redacted context",
            ],
        )
        self.assertContainsAll(
            trust_envelope,
            [
                "If redaction fails, set expected external track count to zero, do not create prompt files",
            ],
        )
        self.assertContainsAll(track_commands, ["CLAUDE_PROMPT=", "redacted context"])
        redaction_contract = self.skill_text.index("**Redaction before prompts**")
        first_prompt_file = self.skill_text.index("CLAUDE_PROMPT=")
        self.assertLess(redaction_contract, first_prompt_file)

    def test_failure_classes_and_confidence_caps_are_normative(self) -> None:
        core_contract = self.section("P0 Core Contract")
        for failure_class in [
            "missing",
            "auth-required",
            "unsafe-mode",
            "timeout",
            "empty-output",
            "schema-invalid",
            "off-target",
        ]:
            with self.subTest(failure_class=failure_class):
                self.assertIn(f"`{failure_class}`", core_contract)

        expected_rows = [
            r"\| 4 \| Full quorum \| high \|",
            r"\| 3 \| Strong quorum \| high \|",
            r"\| 2 \| Partial quorum \| medium \|",
            r"\| 1 \| Solo degraded mode \| low \|",
            r"\| 0 \| No-quorum dry result \| n/a \|",
        ]
        for pattern in expected_rows:
            with self.subTest(pattern=pattern):
                self.assertRegex(core_contract, pattern)

        self.assertContainsAll(
            core_contract,
            [
                "| 2 | Partial quorum | medium | High is disallowed without later independent validation. |",
                "| 1 | Solo degraded mode | low | Must say this is not true multi-model consensus. |",
                "| 0 | No-quorum dry result | n/a | Report only doctor/preflight status or stop unless local synthesis is explicitly useful. |",
            ],
        )

    def test_compact_adr_schema_and_artifact_policy_are_explicit(self) -> None:
        compact_adr = self.section("Compact ADR Report")
        artifact_policy = self.section("Artifact Policy and Cleanup")
        self.assertContainsAll(
            compact_adr,
            [
                "Default to a decision-ready ADR shape",
                "under 120 lines",
                "## Decision",
                "## Options",
                "## Recommendation",
                "## Risks",
                "## Assumptions",
                "## Rejected alternatives",
                "## Next experiment",
                "## Track status",
            ],
        )
        self.assertContainsAll(
            artifact_policy,
            [
                "`adr.md`: compact ADR report",
                "`manifest.json`: provider statuses, quorum label, confidence cap",
                "Do not persist raw prompts, raw context, provider stdout/stderr, or transcripts by default",
                "Raw artifact persistence is off by default",
                "No P0 path posts externally, opens issues, comments on PRs, or contacts production systems",
            ],
        )

    def test_no_external_posting_commands_are_introduced(self) -> None:
        forbidden_patterns = [
            r"\bgh\s+pr\s+comment\b",
            r"\bgh\s+pr\s+review\b[^\n]*(?:--comment|-c|--body|--body-file)",
            r"\bgh\s+issue\s+(?:create|comment)\b",
            r"\bgh\s+api\b[^\n]*(?:comments|issues|pulls|reviews)",
            r"\bcurl\b[^\n]*(?:(?:-X|--request)\s*['\"]?POST\b|--data(?:-raw|-binary)?\b|--form\b)",
            r"\bforge\s+(?:pr|issue)\s+comment\b",
            r"\b(?:python|node|ruby)\b[^\n]*(?:requests\.post|fetch\(|urllib\.request|http\.request)",
        ]
        for pattern in forbidden_patterns:
            with self.subTest(pattern=pattern):
                self.assertIsNone(re.search(pattern, self.skill_text, flags=re.IGNORECASE))


if __name__ == "__main__":
    unittest.main(verbosity=2)
