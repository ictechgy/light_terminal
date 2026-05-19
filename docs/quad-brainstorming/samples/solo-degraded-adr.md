# Quad-Brainstorming ADR: Solo Degraded Sample

## Decision
- Proposed decision: run a small docs/demo experiment before investing in a new surface.
- Quorum: solo degraded (1/4 usable tracks)
- Confidence: low — this is not true multi-model consensus.

## Options
| Option | Upside | Downside | Cost | Risk | Best next test |
| --- | --- | --- | --- | --- | --- |
| Publish sample ADR | easy to share | does not fix provider setup | low | low | ask two users whether the sample is understandable |
| Build CLI wrapper | better onboarding | duplicates behavior if contract is unstable | medium | medium | wrapper dry-run spike |
| Defer adoption work | preserves focus | loses momentum | low | medium | revisit after provider setup improves |

## Recommendation
Use the sample ADR as a low-risk messaging test, but do not describe the run as consensus until at least partial quorum is available.

## Risks
- The single usable provider may miss important alternatives.
- A low-confidence result may still look authoritative if copied without caveat.

## Assumptions
- The user explicitly accepted degraded output.
- Redacted context was sufficient for a first-pass recommendation.

## Rejected alternatives
- Claiming consensus — one usable track cannot support that label.
- Saving raw provider output — unnecessary for the messaging test.

## Next experiment
Fix one provider setup issue, rerun with partial quorum, and compare whether the recommendation changes.

## Track status
| Track | Status | Failure class / caveat | Used in quorum |
| --- | --- | --- | --- |
| Claude | auth-required | local login missing | no |
| Codex | usable | local synthesis only | yes |
| Gemini | unsafe-mode | no verified read-only boundary | no |
| Forge | missing | CLI unavailable | no |

## Evidence and disagreement
- Strong themes: none; solo mode cannot establish cross-provider agreement.
- Split/disputed points: needs another provider before deciding.
- Contrarian ideas: use the run only to generate questions for the next quorum run.
