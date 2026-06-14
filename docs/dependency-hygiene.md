# Dependency Hygiene

`lterm` is a small same-user daemon, so dependency changes are treated as
security- and release-sensitive even when they are routine patch/minor updates.

## Policy

- Keep `Cargo.lock` committed and use `--locked` for CI/release Clippy, test,
  compatibility, soak, and build gates.
- Do not add runtime dependencies casually; prefer existing standard-library or
  already-approved crate surfaces when practical.
- Review dependency updates in small batches. Patch/minor updates are acceptable
  only after the standard test gate passes and any advisory impact is recorded.
- Treat major-version updates, new crates, build-script changes, and crates that
  touch PTYs, sockets, shell commands, credentials, or terminal escape parsing as
  design-review items.
- PR CI runs a version-pinned `cargo audit` against the locked dependency graph
  and caches the audit binary to keep the gate reproducible without adding a
  large source-build cost to every run. Before tagging a release, also run
  `scripts/release-preflight.sh --require-audit` so the audit tool path/version
  and result are captured with the release evidence. If an advisory is
  intentionally deferred, record the affected crate, reachable code path,
  impact, mitigation, and follow-up owner in release evidence.
- npm wrapper/platform package versions must match `Cargo.toml`; platform
  binaries should be built from the same reviewed source revision.

## Dry-run update workflow

Use the repository-safe dry-run helper to inspect what Cargo would update without
modifying the checkout:

```bash
scripts/dependency-minor-dry-run.sh
scripts/dependency-minor-dry-run.sh --package serde_json
```

The helper copies the current `HEAD` to a temporary directory, runs
`cargo update` there, and prints the resulting `Cargo.lock` diff. Review the diff
before deciding whether to apply it in the real checkout.

## Release preflight connection

`scripts/release-preflight.sh` runs the normal build/test/contract gates with
locked Cargo resolution and will run `cargo audit` automatically when
`cargo-audit` is installed. Use `--require-audit` for release evidence when an
audit result is mandatory. The preflight prints Rust, Cargo, rustfmt, Clippy, and
Node diagnostics before the expensive gates so release evidence can identify the
exact toolchain used. Record the toolchain provenance, audit result, dependency
dry-run diff, npm package graph provenance, and any deferred advisory decision in
`docs/release-evidence-template.md`.
