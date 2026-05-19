#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/lterm-dep-dry-run.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

usage() {
  cat <<'EOF'
Usage: scripts/dependency-minor-dry-run.sh [--package NAME ...]

Run a dependency update dry-run in a temporary copy and print the Cargo.lock diff.
The repository checkout is not modified. Cargo updates remain constrained by
Cargo.toml semver requirements, so this is suitable for patch/minor review prep.
EOF
}

packages=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --package|-p)
      [[ $# -ge 2 ]] || { echo "--package requires a value" >&2; exit 64; }
      packages+=("$2")
      shift 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 64 ;;
  esac
done

copy="$TMP/repo"
mkdir -p "$copy"
git -C "$ROOT" archive --format=tar HEAD | tar -xf - -C "$copy"

cmd=(cargo update --manifest-path "$copy/Cargo.toml")
if [[ ${#packages[@]} -gt 0 ]]; then
  for package in "${packages[@]}"; do
    cmd+=(--package "$package")
  done
fi
printf '==>'
printf ' %q' "${cmd[@]}"
printf '\n'
"${cmd[@]}"

if cmp -s "$ROOT/Cargo.lock" "$copy/Cargo.lock"; then
  echo "No Cargo.lock changes from dry-run update."
else
  diff -u "$ROOT/Cargo.lock" "$copy/Cargo.lock" || true
fi
