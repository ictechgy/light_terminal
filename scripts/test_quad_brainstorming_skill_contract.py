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
DEFAULT_SKILL = Path.home() / ".codex" / "skills" / "quad-brainstorming" / "SKILL.md"
SKILL_PATH = Path(os.environ.get(SKILL_PATH_ENV, DEFAULT_SKILL)).expanduser()
CI_ENV_VARS = ("CI", "GITHUB_ACTIONS", "BUILDKITE", "CIRCLECI", "GITLAB_CI")
FENCE_PREFIXES = ("```", "~~~")
HEADING_RE = re.compile(r"^ {0,3}(#{1,6})\s+(.*?)\s*$")
FORBIDDEN_EXTERNAL_POSTING_PATTERNS = [
    r"\bgh\s+pr\s+comment\b",
    r"\bgh\s+pr\s+review\b",
    r"\bgh\s+issue\s+(?:create|comment)\b",
    r"\bgh\s+api\b[^\n]*(?:comments|issues|pulls|reviews)",
    r"\bcurl\b[^\n]*(?:(?:-X|--request)\s*['\"]?POST\b|--data(?:-raw|-binary|-urlencode)?\b|--form(?:-string)?\b)",
    r"\bforge\s+(?:pr|issue)\s+comment\b",
    r"\brequests\.post\s*\(",
    r"\bfetch\s*\((?:(?!\n\s*\n).){0,400}\bmethod\s*:\s*['\"]POST['\"]",
    r"\burllib\.request\.Request\s*\((?:(?!\n\s*\n).){0,400}\bmethod\s*=\s*['\"]POST['\"]",
    r"\bhttp\.request\s*\((?:(?!\n\s*\n).){0,400}\bmethod\s*:\s*['\"]POST['\"]",
]


def running_in_ci() -> bool:
    return any(os.environ.get(var) for var in CI_ENV_VARS)


def update_fence_state(line: str, current: str | None) -> str | None:
    stripped = line.lstrip()
    fence = next((prefix for prefix in FENCE_PREFIXES if stripped.startswith(prefix)), None)
    if fence is None:
        return current
    if current is None:
        return fence
    if current == fence:
        return None
    return current


def level_2_heading_matches(line: str, heading: str) -> bool:
    match = HEADING_RE.match(line)
    return bool(match and match.group(1) == "##" and match.group(2) == heading)


def is_outer_section_boundary(line: str) -> bool:
    match = HEADING_RE.match(line)
    return bool(match and len(match.group(1)) <= 2)


def normalize_shell_continuations(text: str) -> str:
    return re.sub(r"\\\r?\n[ \t]*", " ", text)


def first_forbidden_external_posting(text: str) -> tuple[str, str] | None:
    normalized = normalize_shell_continuations(text)
    for pattern in FORBIDDEN_EXTERNAL_POSTING_PATTERNS:
        match = re.search(pattern, normalized, flags=re.IGNORECASE | re.DOTALL)
        if match:
            excerpt = " ".join(match.group(0).split())
            return pattern, excerpt[:160]
    return None


def markdown_section(text: str, heading: str) -> str:
    """Return a level-2 Markdown section while ignoring headings inside fences."""
    lines = text.splitlines(keepends=True)
    start = None
    target = f"## {heading}"
    in_fence: str | None = None
    for index, line in enumerate(lines):
        if in_fence is None and level_2_heading_matches(line, heading):
            start = index + 1
            break
        in_fence = update_fence_state(line, in_fence)
    if start is None:
        raise AssertionError(f"Missing Markdown section: {target}")

    end = len(lines)
    in_fence = None
    for index in range(start, len(lines)):
        if in_fence is None and is_outer_section_boundary(lines[index]):
            end = index
            break
        in_fence = update_fence_state(lines[index], in_fence)
    return "".join(lines[start:end])


class MarkdownSectionHelperTests(unittest.TestCase):
    def test_markdown_section_ignores_backtick_and_tilde_fenced_headings(self) -> None:
        text = (
            "## Intro\n"
            "```markdown\n"
            "## Target\n"
            "ignored backtick fence\n"
            "```\n"
            "~~~markdown\n"
            "## Target\n"
            "ignored tilde fence\n"
            "~~~\n"
            "## Target\n"
            "body\n"
            "# Next\n"
            "not part of target\n"
        )
        self.assertEqual(markdown_section(text, "Target"), "body\n")


class ContractHelperTests(unittest.TestCase):
    def test_default_skill_path_is_portable(self) -> None:
        self.assertEqual(
            DEFAULT_SKILL,
            Path.home() / ".codex" / "skills" / "quad-brainstorming" / "SKILL.md",
        )
        self.assertEqual(
            DEFAULT_SKILL.parts[-4:],
            (".codex", "skills", "quad-brainstorming", "SKILL.md"),
        )

    def test_forbidden_external_posting_patterns_cover_common_posting_paths(self) -> None:
        forbidden_samples = [
            "gh pr comment 89 --body reviewed",
            "gh pr review 89 --approve",
            "gh pr review 89 --request-changes --body fail",
            "gh issue create --title bug",
            "gh api repos/o/r/issues/1/comments -f body=review",
            "curl \\\n  -X POST \\\n  --data '{\"body\":\"x\"}' https://api.github.com/repos/o/r/issues/1/comments",
            "forge pr comment 89 --body reviewed",
            "requests.post('https://example.invalid')",
            "fetch('https://example.invalid', {method: 'POST', body: 'x'})",
            "urllib.request.Request(url, data=b'x', method='POST')",
            "http.request(url, {method: 'POST'})",
        ]
        for sample in forbidden_samples:
            with self.subTest(sample=sample):
                self.assertIsNotNone(first_forbidden_external_posting(sample))


class QuadBrainstormingSkillContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.skill_path = SKILL_PATH
        if not cls.skill_path.is_file():
            message = (
                f"quad-brainstorming skill not found at {cls.skill_path}. "
                f"Set {SKILL_PATH_ENV}=path/to/SKILL.md to run these local contract tests."
            )
            if SKILL_PATH_ENV in os.environ or running_in_ci():
                raise AssertionError(message)
            raise unittest.SkipTest(message)
        cls.skill_text = cls.skill_path.read_text(encoding="utf-8")

    def section(self, heading: str) -> str:
        return markdown_section(self.skill_text, heading)

    def assertContainsAll(self, text: str, snippets: list[str]) -> None:
        for snippet in snippets:
            with self.subTest(snippet=snippet):
                if snippet not in text:
                    self.fail(f"Missing required snippet: {snippet!r}")

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
        provider_markers = [
            "CLAUDE_PROMPT=",
            "GEMINI_PROMPT=",
            "FORGE_PROMPT=",
            "claude -p",
            "gemini -p",
            "forge --agent",
        ]
        self.assertContainsAll(track_commands, [*provider_markers, "redacted context"])
        redaction_contract = self.skill_text.index("**Redaction before prompts**")
        redaction_failure_guard = self.skill_text.index("If redaction fails, set expected external track count to zero")
        for marker in provider_markers:
            with self.subTest(marker=marker):
                marker_index = self.skill_text.index(marker)
                self.assertLess(redaction_contract, marker_index)
                self.assertLess(redaction_failure_guard, marker_index)

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
        forbidden = first_forbidden_external_posting(self.skill_text)
        self.assertIsNone(
            forbidden,
            msg=(
                "Forbidden external posting command introduced: "
                f"pattern={forbidden[0]!r} excerpt={forbidden[1]!r}"
                if forbidden
                else ""
            ),
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
