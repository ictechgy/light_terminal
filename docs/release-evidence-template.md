# Release Evidence Template

Use this checklist for every reviewed release candidate before tagging or
publishing `lterm`. Keep the filled copy with the release notes, PR, or tag
artifact.

## Release candidate

- Version:
- Commit SHA:
- Release PR:
- Reviewer:
- Date / timezone:
- Host OS and architecture:
- Rust toolchain:
- Node version:

## Required gate

```bash
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked -- --test-threads=1
cargo build --release --locked
scripts/release-preflight.sh --require-audit
```

Evidence:

- `cargo fmt -- --check`:
- `cargo clippy --locked --all-targets -- -D warnings`:
- `cargo test --locked -- --test-threads=1`:
- `cargo build --release --locked`:
- `scripts/release-preflight.sh --require-audit`:

## Contract gate

For fast contract-only reruns after doc/schema edits:

```bash
scripts/release-preflight.sh --contract-only
python3 scripts/validate_contract_manifest.py --self-test
python3 scripts/check_contract_drift.py --self-test
```

Evidence:

- Contract manifest/schema/examples/drift:
- Manifest validator self-test:
- Drift checker self-test:
- Public contract doc owner/table drift:

## Security and dependency evidence

```bash
cargo audit
scripts/dependency-minor-dry-run.sh
```

Evidence:

- `cargo audit` result:
- Deferred advisories, if any:
- Dependency dry-run diff summary:
- Dependency updates applied in this release:
- Dependency updates deferred and owner:

## Manual release soak

Run only when the release owner intentionally accepts the time cost:

```bash
LTERM_SOAK_DURATION=900 LTERM_SOAK_SESSIONS=16 \
  scripts/release-preflight.sh --run-soak --require-audit
```

Evidence:

- Soak run status:
- Duration / sessions:
- Failures or flakes:
- Rationale if skipped:

## Publish / rollback notes

- Artifacts built:
- Checksums:
- npm packages / platform packages:
- Git tag:
- Rollback plan:
- Follow-up issues:
