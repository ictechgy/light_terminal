# Quad-Brainstorming Adoption Guide

`quad-brainstorming` is a privacy-first four-track ideation workflow for decisions that benefit from independent perspectives. It can combine Claude CLI, Codex, Gemini CLI, and ForgeCode when they are locally available, but it remains useful with partial quorum.

This guide is intentionally adoption-focused: it explains how to make the workflow easy to trust, easy to repeat, and safe to share.

## Safety promise

Default runs are read-only:

- no repository writes;
- no issue, ticket, or pull-request posting;
- no production or external service contact beyond the explicitly selected local provider CLIs;
- no raw prompt, raw context, provider stdout/stderr, or transcript persistence;
- redaction before prompt construction and before any external provider invocation.

The workflow prints a Read-Only Trust Envelope before external tracks run. The envelope states context sources, redaction status, provider readiness, expected usable track count, persistence mode, skipped-provider reasons, and no-posting/no-write boundaries.

## Quickstart

```text
quad-brainstorming --doctor
quad-brainstorming --preflight "Which onboarding path should we test first?"
quad-brainstorming --preset decision-record "Choose the first user-facing preset to document"
```

Use `--save-artifacts` only when you want a redacted `adr.md` plus `manifest.json`. Raw artifact saving is intentionally not part of the normal path.

## Presets

| Preset | Use when | Output habit |
| --- | --- | --- |
| `architecture-review` | a design has multiple viable implementation paths | ADR with tradeoffs and migration risk |
| `risk-scan` | a release or rollout needs pre-mortem thinking | risk register with mitigations and stop conditions |
| `decision-record` | the team needs a concise reusable decision | compact ADR under 100 lines |
| `product-strategy` | adoption, packaging, or positioning is the question | strategy ADR with validation metrics |

Presets change lenses and output emphasis only. They do not change redaction, provider classification, quorum math, persistence defaults, or no-posting behavior.

## Quorum messaging

`quad-brainstorming` should be honest about provider availability:

- **Full quorum**: four usable tracks; high confidence only if disagreement is low.
- **Strong quorum**: three usable tracks; high confidence may be downgraded for material disagreement.
- **Partial quorum**: two usable tracks; confidence is capped at medium.
- **Solo degraded mode**: one usable track; confidence is capped at low and the report must say this is not true multi-model consensus.
- **No-quorum dry result**: no usable external tracks; report doctor/preflight status or stop.

Provider failures are not hidden. The manifest records `missing`, `auth-required`, `unsafe-mode`, `timeout`, `empty-output`, `schema-invalid`, and `off-target` classes without storing raw provider output.

## Shareable artifacts

A saved redacted artifact directory contains:

- `adr.md` — compact ADR generated from redacted context and validated track summaries.
- `manifest.json` — local run metadata: provider statuses, quorum, confidence cap, preset, redaction status, context summary, persistence policy, artifact policy, and telemetry status.

The manifest follows [`docs/schemas/quad-brainstorming-manifest.schema.json`](../schemas/quad-brainstorming-manifest.schema.json). It is safe to share only after confirming that context summaries and details do not reveal sensitive project names or paths.

## Sample reports

- [Full/strong architecture-review ADR](samples/architecture-review-adr.md)
- [Solo degraded ADR with low-confidence caveat](samples/solo-degraded-adr.md)

Use the solo sample when explaining why `quad-brainstorming` is still useful with one provider but should not be marketed as consensus.

## P2 team playbooks

### Architecture review

- Preset: `architecture-review`
- Context: PRD, design doc, or narrow diff
- Output: redacted ADR
- Stop condition: one recommended path plus a smallest next experiment

### Release risk scan

- Preset: `risk-scan`
- Context: release notes, public contract changes, or staged diff
- Output: risk register and rollback checklist
- Stop condition: every high-risk item has a mitigation or owner type

### Planning meeting

- Preset: `decision-record`
- Context: product brief and constraints
- Output: options, rejected alternatives, assumptions, next experiment
- Stop condition: one experiment can be run before the next meeting

### Incident pre-mortem

- Preset: `risk-scan`
- Context: rollout plan, operational assumptions, observability notes
- Output: failure modes, detection gaps, rollback gaps
- Stop condition: rollout gates are explicit

## Broader surfaces

Future CLI, MCP/plugin, and GitHub Action surfaces must pass the same Core Contract tests before release. The first GitHub Action shape should be manual-dispatch dry-run with read-only permissions and no automatic comments or issue creation. Any surface that can write externally belongs behind a separate explicit execution mode, not the brainstorming default.

## Local metrics, no telemetry

Adoption metrics are local-only by default and can be computed from redacted manifests:

- run count by preset;
- usable-track distribution;
- skipped-provider reasons;
- saved ADR count;
- repeat-use intervals.

Use the local helper for a no-telemetry summary:

```bash
python3 scripts/quad_brainstorming_manifest_summary.py --json .codex/artifacts/quad-brainstorming
```

No telemetry is sent by default. Exporting metrics must be explicit and local-first.
