# Parent provenance and safe rollback fallback inventory

This inventory covers the files changed from `origin/main` at merge base
`a924d5e45a2916377ff40048ecd6d1826dbe0b94`. It distinguishes deliberate
compatibility behavior from fail-closed evidence handling and test-harness-only
cleanup. A fallback-like spelling is not, by itself, a rollback fallback.

## Classification

| Surface | Trigger | Result | Classification | Disposition |
| --- | --- | --- | --- | --- |
| `src/server.rs:2139-2174,2230-2238,2322-2349` — `parent_request`, `resolve_implicit_parent`, and the `create_session`/`create_session_with_inspector` flow | All explicit parent fields are absent | Search the authenticated peer's bounded process ancestry for the nearest exact live session leader | Deliberate compatibility fallback | Keep. This restores parent provenance across wrappers without overriding explicit provenance. |
| `src/server.rs:2139-2174,2322-2349` — `parent_request` and the `create_session`/`create_session_with_inspector` flow | Any explicit parent field is present but the explicit tuple is incomplete or invalid | Reject session creation | Fail closed, not a fallback | Keep. Invalid explicit provenance must never enable ancestry inference. |
| `src/server.rs` — `resolve_peer_ancestry` | No exact leader, an unreadable hop, a cycle, excessive depth, or PID/birth-marker mismatch | Return no implicit parent | Evidence unavailable; safe no-parent result | Keep. The request may remain a root only when no parent was selected. No guessed parent is accepted. |
| `src/server.rs` — `validate_implicit_parent_chain` and `resolve_implicit_parent_locked` | A selected ancestry link or selected live-session identity changes before commit | Abort creation and clean up the spawned child/reservation | Fail closed, not a downgrade fallback | Keep. A selected child is never silently converted into an unparented root. |
| `src/process_identity.rs` — `SystemProcessInspector::snapshot`, Darwin `proc_pidinfo`, and Linux `/proc/<pid>/stat` parsing | The platform record is unavailable, malformed, truncated, or no longer describes the requested PID | Return `None` | Optional evidence probe | Keep. Callers treat missing identity as insufficient proof; no weak identity is synthesized. |
| `src/tmux_compat.rs` — `process_start_identity` | tmux compatibility needs a process birth marker | Delegate to `crate::process_identity::process_start_identity` | Reuse, not a fallback | Keep. This removes duplicate platform parsing and preserves one identity contract. |
| `scripts/verify_parent_provenance_rollback_matrix.py` — compatibility cells | Old/fixed lterm and OMX combinations differ | Report the explicit compatibility policy; execute the destructive matrix only for fixed/fixed | Release-policy classification | Keep. The old/fixed cells are reported, not emulated by an unsafe runtime fallback. |
| `scripts/verify_parent_provenance_rollback_matrix.py` — cleanup `try`/`finally` blocks | A disposable private-socket fixture exits or fails | Best-effort removal of fixture panes, sessions, processes, and temporary directories | Harness-only cleanup fallback | Keep. Scope is restricted to unique private matrix resources and never the default/live tmux server. |
| `scripts/test_verify_parent_provenance_rollback_matrix.py` — `skipUnless(shutil.which("tmux"))` | Real tmux is unavailable in the test environment | Skip only the real-tmux fixture test | Test capability gate | Keep. Unit coverage for isolation and policy remains active. |
| `scripts/verify_parent_provenance_rollback_matrix.py:673-682,712-717` — optional `--tmux-bin` default | `tmux` is absent from `PATH` | Represent the binary as unavailable and fail when a real-tmux run is requested | CLI availability handling | Keep. No default socket or alternate destructive command is substituted. |
| Documentation and tests | Text contains “fallback”, “rollback”, “missing”, or “default” | Describe or assert the contracts above | Evidence, not executable fallback | Keep. These occurrences make the safety boundary explicit. |

## Safety boundary audit

- Explicit provenance has precedence and malformed explicit provenance fails.
- Implicit ancestry is bounded, exact, and authenticated from the peer process.
- Process identity requires both PID and birth/start identity; PID-only matching is
  not accepted.
- A proof is revalidated before commit and again against the locked live-session
  identity.
- Missing ancestry evidence may produce a root only before a parent is selected.
  Stale selected evidence aborts and cleans up instead of downgrading.
- Rollback evidence targets only an attempt-owned pane after exact owner
  validation.
- No changed production code adds `pkill`, process-group signaling, PID-derived
  rollback targets, default-socket tmux commands, or best-effort destructive
  rollback.

## Simplification verdict

No fallback-like production branch should be deleted or merged. The apparent
fallbacks have different contracts: compatibility inference, evidence failure,
and transaction abort. Combining them would obscure the distinction between “no
parent was proven” and “a proven parent became stale.” The only duplication in
the changed production surface was process-start identity parsing, and the branch
already centralizes it in `src/process_identity.rs`.

## Residual risk

The supported compatibility policy intentionally allows a fully stripped nested
launch to remain a root on systems where peer ancestry cannot be inspected. That
is a loss of hierarchy, not an unsafe inferred relationship or destructive
rollback. Release evidence should continue to call out this degraded-but-safe
cell explicitly.
