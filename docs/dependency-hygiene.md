# Dependency Hygiene

`lterm` is a small same-user daemon, so dependency changes are treated as
security- and release-sensitive even when they are routine patch/minor updates.

## Policy

- Keep `Cargo.lock` committed and use `--locked` for release builds.
- Do not add runtime dependencies casually; prefer existing standard-library or
  already-approved crate surfaces when practical.
- Review dependency updates in small batches. Patch/minor updates are acceptable
  only after the standard test gate passes and any advisory impact is recorded.
- Treat major-version updates, new crates, build-script changes, and crates that
  touch PTYs, sockets, shell commands, credentials, or terminal escape parsing as
  design-review items.
- Before tagging a release, run `cargo audit` when available. If an advisory is
  intentionally deferred, record the affected crate, reachable code path, impact,
  mitigation, and follow-up owner in release evidence.
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

`scripts/release-preflight.sh` runs the normal build/test/contract gates and will
run `cargo audit` automatically when `cargo-audit` is installed. Use
`--require-audit` for release evidence when an audit result is mandatory. Record
the audit result, dependency dry-run diff, and any deferred advisory decision in
`docs/release-evidence-template.md`.
