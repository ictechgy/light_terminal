# Quad-Brainstorming ADR: Architecture Review Sample

## Decision
- Proposed decision: start with a skill-first adapter contract and schema validation before building a standalone runtime surface.
- Quorum: strong (3/4 usable tracks)
- Confidence: high — three usable tracks agreed and one provider was unavailable.

## Options
| Option | Upside | Downside | Cost | Risk | Best next test |
| --- | --- | --- | --- | --- | --- |
| Skill-first contract | fastest trust improvement | less discoverable outside Codex users | low | low | contract test over the skill source |
| Standalone CLI first | easier install story | may fork behavior before schemas stabilize | medium | medium | thin wrapper spike that imports the same schemas |
| GitHub Action first | visible team workflow | introduces posting/permission risk too early | high | high | manual dry-run only proof |

## Recommendation
Ship the skill-first contract and schema assets first, then evaluate a thin CLI wrapper once adapter outputs and manifest schemas are stable.

## Risks
- Users may confuse partial quorum with full consensus.
- Provider CLI flags may drift.
- Saved artifacts may be over-trusted without redaction review.

## Assumptions
- Users value a concise ADR more than raw provider transcripts.
- At least one local provider is usually available.
- Broader surfaces can reuse the same adapter contract.

## Rejected alternatives
- GitHub Action first — too much permission and posting risk before schema confidence.
- Raw transcript archive — conflicts with no-raw-default trust positioning.

## Next experiment
Run `architecture-review` on one real design decision and confirm a teammate can act from the ADR without reading raw transcripts.

## Track status
| Track | Status | Failure class / caveat | Used in quorum |
| --- | --- | --- | --- |
| Claude | usable | systems tradeoffs | yes |
| Codex | usable | implementation feasibility | yes |
| Gemini | auth-required | local login missing | no |
| Forge | usable | maintainability and tests | yes |

## Evidence and disagreement
- Strong themes: stabilize the contract before broad distribution.
- Split/disputed points: how soon to build a standalone CLI wrapper.
- Contrarian ideas: use public sample ADRs as marketing before runtime expansion.
