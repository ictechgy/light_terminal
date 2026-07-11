# ADR 0002: Cooperative input capabilities

- Status: Accepted
- Date: 2026-07-11

## Context

lterm's owner-only Unix socket and peer-UID check intentionally give every
same-UID client ambient authority. Agent integrations need a narrower opt-in
surface without changing raw PTY attach semantics, adding a dependency, or
claiming isolation that the existing socket boundary cannot provide.

## Decision

Protocol v5 adds daemon-generated, in-memory input capability grants. Each grant
binds a UUIDv4 token to one immutable session UUID through `Weak<Session>` and a
finite attempted-byte budget. The registry has global and per-session caps and
uses the lock order `sessions -> input_capabilities`. Reservation subtracts the
whole payload under the registry lock, removes an exhausted grant, releases the
lock, and performs one `write_all`; failures are not refunded.

Tokens are persisted only in exclusive, no-follow, exact-0600, owner-owned,
regular, single-link files. Input and revoke use a two-stage same-connection
protocol: a nonsecret `CapabilityChannel` hello and ready response precede one
bounded sensitive frame. Token-bearing responses and frames use no-preview
parsers. Before unlink, the client reopens and revalidates the file, compares
device/inode identity plus token, and performs one final leaf identity check.
Detected replacements are retained; POSIX cannot atomically unlink an already
open fd, so a malicious same-UID race after the final check remains outside the
cooperative boundary. Session teardown purges grants; revoke is idempotent and
non-oracular.

Legacy `Send` and raw `Attach` are intentionally untouched.

## Consequences

Cooperative or externally sandboxed integrations can delegate only finite PTY
input without sharing target names or daemon-global credentials. The primitive
is memory-only and rollback requires no migration; old private files become
inert after daemon restart or feature removal.

This does not constrain a malicious same-UID process, which can still reach the
ambient socket and use legacy Send or Attach. Strong isolation remains the job
of an external OS sandbox or a future capability-only session mode. A daemon
crash or response loss can orphan an in-memory grant until restart; table caps
bound that risk.
