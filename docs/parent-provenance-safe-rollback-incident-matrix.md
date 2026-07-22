# Parent provenance and safe rollback incident matrix

This matrix is an independent integration gate for the two incident causes. It
does not contact a developer's existing lterm socket or launch a real Team. Its
real-tmux check creates a unique private socket/server inside a mode-0700
temporary directory; it never contacts the default or a live tmux server.

## Matrix

The column order is always **lterm | OMX**.

| lterm | OMX | Safe expected result |
|---|---|---|
| old | old | Historical unsafe baseline. Policy-only; the verifier never executes this cell. |
| old | fixed | Rollback is leader-safe; a fully stripped nested launch may remain a root. |
| fixed | old | Parent provenance recovers, but destructive startup-failure injection is prohibited except with intercepted signals and a disposable leader. |
| fixed | fixed | Parent provenance recovers and stale/reused pane ownership fails closed without a process signal. |

The JSON report always emits these four stable `lterm|OMX` cell IDs. Old-version
cells are `policy_only`; they are not represented as executed without supplied
old artifacts. `fixed|fixed` is marked `executed` only when both fixed halves are
supplied in the same verifier run.

## Independent fixture

Run the verifier against locally built source worktrees:

```bash
python3 scripts/verify_parent_provenance_rollback_matrix.py \
  --lterm-bin target/debug/lterm \
  --omx-repo /path/to/fixed-oh-my-codex-source-worktree \
  --tmux-bin /absolute/path/to/tmux \
  --output /tmp/parent-provenance-safe-rollback-matrix.json
```

The lterm half creates a private temporary `HOME`, runtime directory, data
directory, lterm socket, tmux temporary directory, and daemon. Its descendant
launcher removes all five explicit provenance variables (`LTERM_PANE`,
`LTERM_PARENT_TOKEN`, `LTERM_SOCKET`, `TMUX`, and `TMUX_PANE`). It verifies the
nearest parent link, root/child inventories, and an outside-process control that
must remain a root.

The OMX half first runs pre-built mock-tmux tests. Those tests provide synthetic
pane IDs, stale/reused owner tags, multi-row PID output, forced startup-evidence
failure, and an intercepted process-signal ledger. A passing result requires
owner mismatch to fail closed and the destructive signal target list to remain
empty. It then starts real tmux with `-S` pointing at a unique absolute socket
under a private temporary directory and `-f /dev/null`. The fixture creates a
leader pane, an unrelated control pane, and an exact attempt-owned pane. Forced
rollback validates the owner tag, deletes only that attempt pane, and proves the
leader and control panes survive.

These are complementary claims, not one overextended claim: the pre-built OMX
runtime tests exercise injected `startTeam` rollback, while the real-tmux
fixture exercises only the exact pane primitive boundary. Direct `kill-pane` in
the fixture is not represented as an OMX startup invocation. The JSON keeps
`destructive_signal_targets` empty and lists the protected leader/control panes
as absent from signal targets.

The attempt pane also records two exact process identities. Its foreground
process must terminate with the pane. A descendant deliberately created in a
new session is allowed to remain as documented escaped residue; the fixture
then revalidates its recorded identity and cleans up only that exact PID. Broad
`pkill`, inferred process targets, default-socket commands, and process-group
cleanup are prohibited.

## Safety boundary

- Do not point the fixture at an installed OMX `dist` tree; use a reviewed source
  worktree whose `dist` was built from the source commit under test.
- Do not reuse a live `LTERM_SOCKET`, `LTERM_RUNTIME_DIR`, or `LTERM_DATA_DIR`.
- Every real tmux command must contain the fixture's explicit private `-S`
  socket. Never use the default tmux socket, including during teardown.
- Do not run the fixed-lterm/old-OMX destructive cell against a non-disposable
  leader.
- Publishing, deployment, live daemon restart, and production session mutation
  are outside this verifier.
