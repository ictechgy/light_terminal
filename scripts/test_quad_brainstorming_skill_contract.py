#!/usr/bin/env python3
"""Regression checks for the installed quad-brainstorming P0 contract.

The quad-brainstorming workflow currently lives as a Codex skill outside this
repository. These tests validate the installed skill text as the execution
surface until the skill has a repo-native source package. They are intentionally
local/read-only: no provider CLIs are invoked and no external posting is done.
"""

from __future__ import annotations

import os
import re
import unittest
from pathlib import Path


DEFAULT_SKILL = Path("/Users/jinhongan/.codex/skills/quad-brainstorming/SKILL.md")
SKILL_PATH = Path(os.environ.get("QUAD_BRAINSTORMING_SKILL", DEFAULT_SKILL))


class QuadBrainstormingSkillContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.skill_text = SKILL_PATH.read_text(encoding="utf-8")

    def assertContainsAll(self, snippets: list[str]) -> None:
        for snippet in snippets:
            with self.subTest(snippet=snippet):
                self.assertIn(snippet, self.skill_text)

    def test_core_contract_sections_are_present(self) -> None:
        self.assertContainsAll(
            [
                "## P0 Core Contract",
                "## Track Availability and Doctor/Preflight",
                "## Read-Only Trust Envelope",
                "## Compact ADR Report",
                "## Artifact Policy and Cleanup",
            ]
        )

    def test_core_contract_preserves_privacy_first_defaults(self) -> None:
        self.assertContainsAll(
            [
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
            ]
        )

    def test_provider_state_taxonomy_keeps_configured_runnable_usable_separate(self) -> None:
        self.assertContainsAll(
            [
                "Provider status fields:",
                "`configured` | CLI/surface exists and has a detectable adapter",
                "`runnable` | configured plus auth/safety/read-only boundary is acceptable",
                "Treat each track output as usable only when it:",
                "Exclude all failed classes from usable-track counts",
                "make the denominator the number of usable tracks",
            ]
        )
        configured_index = self.skill_text.index("`configured` | CLI/surface exists")
        runnable_index = self.skill_text.index("`runnable` | configured plus auth/safety")
        usable_index = self.skill_text.index("Treat each track output as usable only when it:")
        self.assertLess(configured_index, usable_index)
        self.assertLess(runnable_index, usable_index)

    def test_doctor_and_preflight_never_invoke_providers(self) -> None:
        self.assertContainsAll(
            [
                "`--doctor` | Check local provider availability/auth/safety boundaries",
                "`--dry-run` / `--preflight` | Collect allowed context, redact it, print the Read-Only Trust Envelope",
                "Treat `--doctor`, `--dry-run`, and `--preflight` as non-invocation modes",
                "must not send prompts or context to providers",
                "Doctor output should be actionable and successful even when every external provider is `missing` or `unsafe-mode`",
            ]
        )
        doctor_row = re.search(r"\| `--doctor` .* \| Never \| None \|", self.skill_text)
        preflight_row = re.search(r"\| `--dry-run` / `--preflight` .* \| Never \|", self.skill_text)
        self.assertIsNotNone(doctor_row)
        self.assertIsNotNone(preflight_row)

    def test_redaction_precedes_prompt_construction_and_external_calls(self) -> None:
        self.assertContainsAll(
            [
                "Redaction must happen before prompt construction, prompt-file creation, logs, metrics, artifacts, or external adapter/provider calls",
                "collect context into a private temp file, run redaction, then build prompt files only from redacted context",
                "Feed `$CONTEXT_REDACTED`, not `$CONTEXT`, to every external track",
                "If redaction fails, set expected external track count to zero, do not create prompt files",
            ]
        )
        redaction_contract = self.skill_text.index("**Redaction before prompts**")
        first_prompt_file = self.skill_text.index("CLAUDE_PROMPT=")
        self.assertLess(redaction_contract, first_prompt_file)

    def test_failure_classes_and_confidence_caps_are_normative(self) -> None:
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
                self.assertIn(f"`{failure_class}`", self.skill_text)

        expected_rows = [
            r"\| 4 \| Full quorum \| high \|",
            r"\| 3 \| Strong quorum \| high \|",
            r"\| 2 \| Partial quorum \| medium \|",
            r"\| 1 \| Solo degraded mode \| low \|",
            r"\| 0 \| No-quorum dry result \| n/a \|",
        ]
        for pattern in expected_rows:
            with self.subTest(pattern=pattern):
                self.assertRegex(self.skill_text, pattern)

        self.assertContainsAll(
            [
                "Never label solo degraded mode above `low`",
                "never label partial quorum above `medium` without a separate validation result",
                "If there are zero usable tracks, do not present consensus",
            ]
        )

    def test_compact_adr_schema_and_artifact_policy_are_explicit(self) -> None:
        self.assertContainsAll(
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
                "`adr.md`: compact ADR report",
                "`manifest.json`: provider statuses, quorum label, confidence cap",
                "Do not persist raw prompts, raw context, provider stdout/stderr, or transcripts by default",
                "`--save-raw-artifacts-i-understand-risk`",
                "No P0 path posts externally, opens issues, comments on PRs, or contacts production systems",
            ]
        )

    def test_no_external_posting_commands_are_introduced(self) -> None:
        forbidden_patterns = [
            r"\bgh\s+pr\s+comment\b",
            r"\bgh\s+issue\s+create\b",
            r"\bcurl\s+-X\s+POST\b",
            r"\bforge\s+pr\s+comment\b",
        ]
        for pattern in forbidden_patterns:
            with self.subTest(pattern=pattern):
                self.assertIsNone(re.search(pattern, self.skill_text))


if __name__ == "__main__":
    unittest.main(verbosity=2)
