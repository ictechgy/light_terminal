#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/prepare-npm-platform-package.sh <platform-arch> [path-to-lterm]

Examples:
  scripts/prepare-npm-platform-package.sh darwin-arm64 target/release/lterm
  scripts/prepare-npm-platform-package.sh linux-x64 ./dist/lterm

The script copies a prebuilt lterm binary into npm/platforms/lterm-<platform-arch>/bin/lterm
and runs npm pack --dry-run for that platform package. Build and verify the binary before publishing.
USAGE
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 64
fi

target="$1"
case "$target" in
  darwin-arm64|darwin-x64|linux-arm64|linux-x64) ;;
  *)
    echo "unsupported npm platform target: $target" >&2
    usage
    exit 64
    ;;
esac

binary="${2:-target/release/lterm}"
if [[ ! -x "$binary" ]]; then
  echo "lterm binary is missing or not executable: $binary" >&2
  exit 66
fi

package_dir="npm/platforms/lterm-$target"
install -d "$package_dir/bin"
install -m 0755 "$binary" "$package_dir/bin/lterm"
(
  cd "$package_dir"
  npm pack --dry-run
)
