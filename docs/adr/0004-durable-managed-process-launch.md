# ADR 0004: Durable managed-process launch

- Status: Accepted
- Date: 2026-07-14

## Context

A future speculative-session feature must recover a managed root process after
daemon death without confusing PID reuse for the original process. Spawning the
target before durably recording its PID and birth identity leaves an
unresolvable crash window. PID, PGID, argv, environment, process name, age, or a
best-effort process scan cannot close that window.

This ADR accepts only the prerequisite launch and root-process reconciliation
substrate. It does not approve or implement full G003.

## Decision

Managed launch is Linux-only and uses a trusted two-phase gate. The daemon
first persists and reads back `IntentDurable`, then takes an exclusive OFD lock
on the slot's fixed, immutable guard inode and spawns a hidden `/proc/self/exe`
gate inheriting that open file description and a bounded `SOCK_SEQPACKET`
control channel. The gate may validate and wait, but may not execute target
code. It self-registers by atomically temp-writing, fsyncing, renaming,
directory-fsyncing, reopening, and verifying a mutable registration sidecar
bound to the slot, generation, nonce, boot, PID namespace, PID, and start ticks.

The inherited guard proves pre-identity absence, not identity: acquiring the
verified guard through a new open file description proves that no pre-exec
child or gate still owns an identity-less intent and, by the phase invariant,
that no target was released. A busy guard plus a valid durable registration
sidecar lets recovery locate the exact holder; a missing, corrupt, or
unavailable sidecar preserves explicit unknown evidence and remains
unevictable. The parent verifies self-registration through pidfd and procfs
before authoritatively recording `{boot_uuid, pid_namespace_inode, pid,
start_ticks}` as `IdentityDurable`. Only after that record is read back may the
daemon send one atomic `COMMIT`, carrying the pinned executable descriptor with
`SCM_RIGHTS`. The gate revalidates its identity, marks control and guard
descriptors `CLOEXEC`, and executes the pinned target with
`execveat(AT_EMPTY_PATH)` without changing PID or start ticks. EOF, a malformed
or stale commit, validation failure, or failed exec exits without releasing
unrecorded target code.

The bounded registry uses 1,024 fixed slots, immutable guards, mutable
registration sidecars, and mutable durable records. Records move through
`Vacant -> IntentDurable -> IdentityDurable -> CleanupPending ->
ResolvedTombstone -> Vacant`; generations never wrap. Every transition writes
a same-directory exclusive no-follow temp, fsyncs and revalidates it, atomically
renames it, fsyncs the directory, then reopens, parses, and verifies the expected
generation and state. Registry genesis also fsyncs every file and directory
before a no-replace root rename and parent fsync. No spawn, commit, signal, or
reuse may precede its durable readback.

Process evidence is typed as `Present(identity)`, `Absent`, or
`Unavailable(reason)`; missing, corrupt, ambiguous, or unsupported evidence
never collapses to absence. Restart cleanup durably records and reads back
`CleanupPending`, opens a pidfd, then rereads the birth identity. It signals
only an exact matching incarnation through `pidfd_send_signal`, never raw
`kill(pid)` or a stored PGID. PID reuse proves the recorded incarnation absent
but must not signal the current process. A tombstone becomes durable only after
exact-process absence is positively proved.

Unresolved or invalid records produce `unknown_orphan_risk`, remain unevictable,
and consume capacity. Age and expiry never convert unknown into absent. Only
positively resolved tombstones older than retention may be reclaimed.
Structural registry risk blocks new managed launches, and saturation fails
closed before intent creation or spawn. Ordinary sessions, Attach, Send, G002
capabilities, and raw PTY bytes remain outside this registry and unchanged.

## Consequences

The substrate requires Linux procfs birth evidence, boot and PID-namespace
identity, OFD locks, pidfds with `pidfd_send_signal`, `SOCK_SEQPACKET`,
`SCM_RIGHTS`, `/proc/self/exe`, and `execveat`. If a required primitive is
unavailable, managed launch is unsupported and fails closed; there is no
portable or PID-only fallback.

This is a safety mechanism within lterm's cooperative same-UID trust model, not
isolation from a malicious same-UID host process. Stronger adversaries require
an external sandbox or privilege boundary.

The contract proves only managed root-process launch and recovery. Full G003
tournament RPCs, workspaces, candidate policy, bwrap `--sync-fd` integration,
and descendant containment and quiescence proof remain excluded. They require
a fresh consensus plan after this prerequisite is implemented, crash-tested,
and architect-approved.
