# Parent provenance and safe rollback release handoff

## Release artifacts

The release provenance reviewed by this handoff is:

| Component | Version | Reviewed source | Artifact SHA-256 |
|---|---:|---|---|
| `light-terminal` / `@ictechgy/lterm` | `1.0.33` | last code-bearing commit `e3c8892831e2559c15f63acf70793056bd957dbb` | Deferred: no authorized `v1.0.33` tag archive exists yet; generate and verify this value only after tagging. |
| `oh-my-codex` | `0.20.3` | reviewed upstream HEAD `49073f8ae5a65f4d5036427993f3cc81e8ac525a`; last production-code fix `1010dda0f56a92dae7c163bc0684ad4e13a25143` | A locally generated pre-release artifact `oh-my-codex-0.20.3.tgz` (`5071529` bytes), SHA-256 `e27babaa7e0dd44a73e991364b6095e10b9b8e8218d766d582a76524617f823e`; not published or deployed. |

The local package evidence is recorded in
`.omx/ultragoal/evidence/g007/package-1784686497/package-evidence.json` with
`published=false`, `deployed=false`, and `production_contact=false`. This package
is a pre-release handoff artifact only; it does not authorize publication,
deployment, or a live restart.

The prior OMX artifact from `923cc7869e9e676121807abf238f61a4c158efca`
with SHA-256
`377476c6fec1c32a44873cbcf0cbe623a192843ff4a87b3526e9f36ec8acf0b1`
is superseded for this fix. It does not contain the G007 successful-startup
commit-boundary change and must not be represented or deployed as the fixed G007
artifact.

The reviewed `light-terminal` code-bearing release head before this metadata
edit was `e3c8892831e2559c15f63acf70793056bd957dbb`.

The Homebrew formula intentionally remains a complete, valid `v1.0.32`
formula in this pre-tag metadata commit. The
`packaging/homebrew/PENDING_RELEASE` marker records the intentional `1.0.33`
deferral. Its URL, version assertion, and SHA-256 must be updated together only
after an authorized `v1.0.33` tag exists and the exact downloaded archive's
SHA-256 has been verified; remove the marker in that same follow-up commit.

## Final verification evidence

Verification ran on 2026-07-23 with Node `v26.5.0` and npm `11.17.0`.

### OMX

The unrestricted OMX final gates all passed:

These results apply to reviewed source HEAD
`49073f8ae5a65f4d5036427993f3cc81e8ac525a`, not the superseded package
artifact from `923cc7869e9e676121807abf238f61a4c158efca`.

| Gate | Result |
|---|---|
| Full `tmux-session` suite | PASS: `225/225` |
| Full `runtime` suite | PASS: `155/155` |
| Build | PASS |
| No-unused static check | PASS |
| Lint | PASS |
| Native agent assets | PASS |
| Plugin bundle | PASS |
| Catalog consistency | PASS |

The rollback-specific assertions prove that:

- drifted pane ownership fails closed when startup evidence is missing;
- multi-row or mismatched pane/PID output is rejected;
- only an exact attempt-owned pane is killed;
- protected, reused, missing, and unreadable pane targets are preserved; and
- shared startup rollback has no PID or destructive process-signal fallback.

### light-terminal

The final serial full suite passed:

| Gate | Result |
|---|---|
| Unit tests | PASS: `664` |
| CLI smoke tests | PASS: `268` |
| Lifecycle tests | PASS: `11` |
| Short soak | PASS: `1` |
| Formatting | PASS |
| Clippy | PASS |
| Release build | PASS |
| Audit | PASS: zero vulnerabilities; allowed warning `RUSTSEC-2026-0190` for `anyhow 1.0.102` |
| Pre-tag release version alignment | EXPECTED BLOCK: Cargo/npm/docs are `1.0.33`, while the complete Homebrew formula remains `1.0.32` until the tagged archive SHA-256 can be measured. |

### `RUSTSEC-2026-0190` assessment

The advisory affects `anyhow::Error::downcast_mut` before `anyhow 1.0.103` when
mutable downcasting follows `Error::context`; that sequence can violate Rust's
borrow rules. The reviewed lockfile contains `anyhow 1.0.102` both as a direct
`light-terminal` dependency and through `portable-pty 0.9.0`.

The affected API is not reachable from the reviewed source paths. A repository
scan found no `downcast_mut` call; the only downcasts in `light-terminal` are
immutable `downcast_ref::<std::io::Error>` checks in managed-launch, server, and
client error classification. A source scan of the locked `portable-pty 0.9.0`
crate likewise found no `downcast_mut` call. Error creation, context attachment,
PTY setup, session launch, and error rendering therefore use the affected crate
version but do not invoke the advisory's vulnerable operation.

The current mitigation is to keep `downcast_mut` absent and treat any future
introduction as release-blocking while `1.0.102` remains locked. The upgrade plan
is to update the lockfile to patched `anyhow >=1.0.103` in a separately reviewed
artifact revision, then rerun the locked macOS and Linux format, clippy, test,
release-build, and `cargo audit` gates and regenerate artifact hashes before
release. A local offline resolution check confirmed `1.0.103` is compatible with
both the direct dependency and `portable-pty`; it was intentionally not retained
in this handoff revision because changing the reviewed dependency graph would
invalidate the exact release artifact provenance above.

### Linux successor-boundary evidence

The G011 Linux gate ran against clean source HEAD
`4299c1bfa6febcea288b9cfcefdcc141d88c314e`, archived with SHA-256
`7d489bde5469c8cb1bcb2411f7001bd63d240a6394c6bcb7abffb9e23afd38bb`.
It used Ubuntu on `aarch64`, Rust/Cargo `1.97.0`, `CARGO_NET_OFFLINE=true`,
and a new network namespace whose only interface was loopback.

The certified run passed formatting, all-target clippy with warnings denied,
`747` unit tests, `268` CLI smoke tests, `11` lifecycle tests, `18` managed Linux
tests with the delegated-cgroup gate explicitly ignored, the short soak,
`53` speculation Linux tests, and the locked release build. The run then exited
only because the VM did not install the optional `file` evidence utility; that
command ran after the release build completed and was not a Rust gate. The
machine-checked certification and complete log are retained at
`.omx/ultragoal/evidence/g011/linux-gates-exact-4299c1bf-certified.json` and
`.omx/ultragoal/evidence/g011/linux-gates-exact-4299c1bf-attempt1-file-tool.log`.

The original `7f434440` Linux run exposed a test-isolation race: one test counted
every process FD while the Rust harness ran other FD tests concurrently. Commit
`3580c50f` narrowed the assertion to descriptors for a unique temporary inode;
the affected test also passed `20/20` focused pre-fix runs, confirming the
production descriptor cleanup itself was not the failing behavior. Two later
full retries transparently retained in G011 evidence each observed a different
PTY-output assertion flake, while the certified run had zero test failures.

Parallel CLI smoke runs observed the existing shared-resource timing failure
`agent_mobile_status_request_stays_on_transcript_surface`. The exact test passed
`3/3` in isolation and the entire serial suite passed, so this is a non-blocking
test-isolation observation, not a product failure.

The incident matrix unit suite passed `8/8`, and the disposable real tmux matrix
also passed. The matrix recorded `production_contact=false` and
`destructive_signal_targets=[]`.

## OMX-first rollout

1. The release owner verifies that the current local pre-release package
   `oh-my-codex-0.20.3.tgz` matches reviewed source HEAD
   `49073f8ae5a65f4d5036427993f3cc81e8ac525a` and SHA-256
   `e27babaa7e0dd44a73e991364b6095e10b9b8e8218d766d582a76524617f823e`, then
   obtains separate authority for publication, deployment, and any live restart.
   Do not use the superseded `923cc786...` artifact for the G007 fix.
2. After that verification and authority, publish and roll out the current fixed
   OMX package first.
3. Confirm the installed OMX artifact matches the reviewed source and repeat the
   rollback-specific gate on a disposable private tmux server.
4. Only after fixed OMX is present, roll out fixed lterm (`1.0.33`, with last
   code-bearing commit `e3c8892831e2...`). After the authorized release tag is
   created, download that exact archive, verify its SHA-256, and update the
   Homebrew formula URL, version assertion, and SHA-256 together in a separate
   follow-up commit before publishing it.
5. Run the independent fixed-lterm/fixed-OMX incident matrix with isolated lterm
   state and an explicit private tmux socket. Do not contact default or live
   sockets.

This order avoids introducing fixed lterm while an old OMX may still use unsafe
startup rollback behavior.

## Rollback order

- Prefer rolling lterm back first while keeping fixed OMX. The old-lterm / fixed-OMX
  state retains leader-safe rollback, although a fully stripped nested launch may
  remain a root.
- Roll OMX back only after lterm is no longer fixed, unless the environment is a
  disposable laboratory with every destructive signal intercepted.
- Never use broad `pkill`, inferred process targets, process-group cleanup, a
  default tmux socket, or a live lterm socket as part of this rollback.
- If owner provenance is absent, stale, reused, mismatched, or unreadable, fail
  closed and preserve the pane for manual diagnosis.

## Unsupported destructive cell

The **fixed-lterm | old-OMX** destructive startup-failure cell is unsupported for
release, production, developer sessions, or any non-disposable leader. It is
policy-only in normal verification. If historical behavior must be investigated,
use a disposable leader, an isolated private tmux server, isolated lterm state,
and intercepted process signals; do not represent that exercise as a supported
rollout state.

## Authority boundary

This verifier is not authorized to publish npm or crate artifacts, merge or tag
the release, deploy either component, restart a live daemon, mutate production or
developer sessions, contact existing sockets, or execute the unsupported
destructive cell. Those actions require the release owner. This handoff supplies
verification evidence and rollout/rollback ordering only.
