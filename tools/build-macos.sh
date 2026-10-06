#!/bin/bash
set -euo pipefail
[[ $(uname -s) == Darwin ]] || { echo 'Build on a macOS host with Xcode Command Line Tools.' >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
case "${1:-$(uname -m)}" in
  arm64) target=aarch64-apple-darwin ;;
  x64|x86_64) target=x86_64-apple-darwin ;;
  *) echo 'Choose arm64 or x64.' >&2; exit 1 ;;
esac
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-13.0}
cargo build --locked --release --package adapter-app --target "$target"
printf 'Built %s in target/%s/release\n' "$target" "$target"
