#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/release-preflight.sh [OPTIONS]

Run the local release preflight gate from one reviewed checkout.

Options:
  --contract-only          Run only manifest/schema/example/drift checks.
  --allow-occupied-skip    Set LTERM_TEST_ALLOW_OCCUPIED_SKIP=1 for hosts with a live daemon.
  --run-soak              Run the ignored release-gate soak profile (15 minutes by default).
  --require-audit         Fail if cargo-audit is unavailable or reports advisories.
  --skip-audit            Do not run cargo audit even if installed.
  -h, --help              Show this help.

Environment:
  LTERM_SOAK_DURATION     Override soak duration seconds when --run-soak is used.
  LTERM_SOAK_SESSIONS     Override soak session count when --run-soak is used.
EOF
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT_ONLY=0
ALLOW_OCCUPIED_SKIP=0
RUN_SOAK=0
AUDIT_MODE=auto

while [[ $# -gt 0 ]]; do
  case "$1" in
    --contract-only) CONTRACT_ONLY=1 ;;
    --allow-occupied-skip) ALLOW_OCCUPIED_SKIP=1 ;;
    --run-soak) RUN_SOAK=1 ;;
    --require-audit) AUDIT_MODE=require ;;
    --skip-audit) AUDIT_MODE=skip ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 64 ;;
  esac
  shift
done

if [[ "$CONTRACT_ONLY" == 1 ]]; then
  if [[ "$RUN_SOAK" == 1 ]]; then
    echo "--contract-only cannot be combined with --run-soak" >&2
    exit 64
  fi
  if [[ "$AUDIT_MODE" == "require" ]]; then
    echo "--contract-only cannot be combined with --require-audit" >&2
    exit 64
  fi
  if [[ "$ALLOW_OCCUPIED_SKIP" == 1 ]]; then
    echo "note: --allow-occupied-skip has no effect with --contract-only" >&2
  fi
  AUDIT_MODE=skip
fi

cd "$ROOT"
if [[ "$ALLOW_OCCUPIED_SKIP" == 1 ]]; then
  export LTERM_TEST_ALLOW_OCCUPIED_SKIP=1
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [[ "$TARGET_DIR" != /* ]]; then
  TARGET_DIR="$ROOT/$TARGET_DIR"
fi

step() { printf '\n==> %s\n' "$*"; }
run() { step "$*"; "$@"; }

cargo_package_version() {
  python3 - <<'PY'
from pathlib import Path

section = None
for raw_line in Path("Cargo.toml").read_text(encoding="utf-8").splitlines():
    line = raw_line.split("#", 1)[0].strip()
    if not line:
        continue
    if line.startswith("[") and line.endswith("]"):
        section = line
        continue
    if section == "[package]" and line.startswith("version"):
        key, _, value = line.partition("=")
        if key.strip() == "version":
            print(value.strip().strip('"'))
            break
else:
    raise SystemExit("Cargo.toml [package].version not found")
PY
}

version_from_package_json() {
  node -e 'const fs = require("fs"); const path = process.argv[1]; console.log(JSON.parse(fs.readFileSync(path, "utf8")).version)' "$1"
}

step "Validate release metadata versions"
cargo_version=$(cargo_package_version)
if command -v node >/dev/null 2>&1; then
  npm_version=$(version_from_package_json package.json)
  [[ "$cargo_version" == "$npm_version" ]] || {
    echo "Cargo.toml version $cargo_version != package.json version $npm_version" >&2
    exit 65
  }
  shopt -s nullglob
  platform_package_jsons=(npm/platforms/*/package.json)
  shopt -u nullglob
  for package_json in "${platform_package_jsons[@]}"; do
    platform_version=$(version_from_package_json "$package_json")
    [[ "$platform_version" == "$cargo_version" ]] || {
      echo "$package_json version $platform_version != $cargo_version" >&2
      exit 65
    }
  done
else
  echo "node not found; skipping npm package version cross-check" >&2
fi
export LTERM_CARGO_VERSION="$cargo_version"
manifest_release=$(python3 - <<'PY'
import json
from pathlib import Path
manifest = json.loads(Path('docs/contract-manifest.json').read_text())
print(manifest.get('release'))
PY
)
expected_release="lterm-$cargo_version"
[[ "$manifest_release" == "$expected_release" ]] || {
  echo "contract manifest release '$manifest_release' != '$expected_release'" >&2
  exit 65
}

if [[ "$CONTRACT_ONLY" == 0 ]]; then
  run cargo fmt -- --check
  run cargo clippy --locked --all-targets -- -D warnings
  run cargo test --locked -- --test-threads=1
  run cargo build --release --locked
  LTERM_BIN="${LTERM_BIN:-$TARGET_DIR/release/lterm}"
else
  run cargo build --locked
  LTERM_BIN="${LTERM_BIN:-$TARGET_DIR/debug/lterm}"
fi

run python3 scripts/validate_contract_manifest.py docs/contract-manifest.json
run python3 scripts/validate_json_schemas.py --manifest docs/contract-manifest.json --schema-dir docs/schemas --lterm-bin "$LTERM_BIN"
LTERM_BIN="$LTERM_BIN" run python3 scripts/check_contract_examples.py --manifest docs/contract-manifest.json README.md docs/public-contract.md
run python3 scripts/check_contract_drift.py --self-test
run python3 scripts/check_contract_drift.py --manifest docs/contract-manifest.json --lterm-bin "$LTERM_BIN"

if [[ "$CONTRACT_ONLY" == 0 ]]; then
  run cargo test --locked --test upgrade_matrix -- --nocapture --test-threads=1
fi

case "$AUDIT_MODE" in
  skip) echo "Skipping cargo audit." ;;
  auto)
    if command -v cargo-audit >/dev/null 2>&1; then
      run cargo audit
    else
      echo "audit: skipped (cargo-audit not installed)"
      echo "cargo-audit not found; install with 'cargo install cargo-audit' or rerun with --require-audit in release evidence." >&2
    fi
    ;;
  require)
    command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit required but not found" >&2; exit 66; }
    run cargo audit
    ;;
esac

if [[ "$RUN_SOAK" == 1 ]]; then
  export LTERM_SOAK_DURATION="${LTERM_SOAK_DURATION:-900}"
  export LTERM_SOAK_SESSIONS="${LTERM_SOAK_SESSIONS:-16}"
  run cargo test --locked --test soak -- --ignored --nocapture --test-threads=1
else
  echo "Skipping release soak; pass --run-soak for the manual 15-minute gate."
fi

step "Release preflight completed"
