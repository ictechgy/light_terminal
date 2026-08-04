# ADR 0005: daemon-owned styled-screen RPC

## Decision

Expose a versioned, local same-user daemon RPC that returns full styled terminal
snapshots from the existing `vt100` parser. The raw attach PTY stream remains
byte-transparent and is never reconstructed from this RPC.

The v1 payload uses a deduplicated style table and per-row runs. Logical text
cells and explicit wide continuations make the physical terminal grid
self-contained. The cursor column is the parser's raw `0..=cols` position; v1
deliberately does not infer delayed-wrap state.

Each session has an immutable UUID incarnation and a non-wrapping decimal screen
revision. A matching pair returns `not_modified`; a different incarnation,
malformed revision, or future revision returns `styled_screen_stale`.

## Safety limits

The daemon admits at most 32 MiB of concurrent snapshot work, reserving a
conservative per-request estimate before cloning the parser screen. Individual
responses are limited to 8 MiB. Geometry and style counts are bounded. An
absolute write deadline prevents a stalled or slowly draining peer from
indefinitely holding a reservation.

Parser failure or revision exhaustion quarantines the session-local terminal
screen-state subsystem, including styled snapshots and attach-screen
reconstruction. The raw PTY reader and attached live streams continue
operating.
