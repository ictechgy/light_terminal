# ADR 0003: Live session metadata operation algebra

- Status: Accepted
- Date: 2026-07-12

## Context

Session names and status themes are useful mutable side state, but ordinary
mutation offers no deterministic way to reverse an accidental change. PTY
bytes, process effects, filesystem effects, input, and session termination are
not safely invertible and must not be mislabeled as reversible.

## Decision

Protocol v6 adds a per-live-session, in-memory linear journal for `name` and
`status_theme` only. A single `SessionMetadata` mutex holds the current pair,
the entries, cursor, and volatile purge aggregate. The lock order is
`State.sessions -> Session.metadata`; code must not call `Session::info()` while
holding metadata. Existing non-idempotent `rename`, `status-theme`, and
tmux-compatible `rename-session` mutations append one entry. No-ops append
nothing.

Undo requires the complete current metadata pair to equal the entry's `after`
value; redo requires equality with `before`. Name-index conflicts and current
state mismatches reject before any map, value, entry, cursor, or purge evidence
changes. The journal holds at most 1024 entries. A new mutation at the cap or
behind the tip rejects rather than evicting entries or truncating the redo
branch; no-ops still succeed.

`metadata purge-history` is the one deliberately irreversible operation. It
requires `--irreversible`, the exact canonical immutable live session UUID, a
nonempty journal, and checked aggregate counters. It preserves current name and
theme, clears entries and cursor, and updates live-only informational purge
evidence. This is an accident gate, not same-UID authorization or durable
audit.

Only the new metadata history/undo/redo/purge RPCs require protocol v6. Legacy
rename/theme/tmux behavior and raw Attach, Send, Kill, and close paths retain
their mixed-version and byte-stream contracts.

## Consequences

Metadata mutations become deterministically reversible within one live daemon
and session lifetime. The hard capacity and preserved redo branch can make a
rename or theme update unavailable until the user redoes to the tip or
explicitly purges history. All journal and purge evidence disappears on session
close, daemon shutdown, or crash. The design makes no claim about physical
energy, process rollback, filesystem rollback, security authorization, or
durable non-repudiation.
