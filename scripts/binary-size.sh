#!/bin/sh
# Compares a release binary with the same platform's asset in the latest GitHub release.
# Usage: scripts/binary-size.sh [BINARY]. Exits 1 when it grew more than MAX_GROWTH_PCT (default 2).
set -eu

binary="${1:-target/release/llm-hub}"
repo="${GITHUB_REPOSITORY:-barnuri/llm-hub}"
max_growth_pct="${MAX_GROWTH_PCT:-2}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) asset=llm-hub-linux-x86_64 ;;
  Linux-aarch64 | Linux-arm64) asset=llm-hub-linux-aarch64 ;;
  Darwin-arm64) asset=llm-hub-macos-aarch64 ;;
  Darwin-x86_64) asset=llm-hub-macos-x86_64 ;;
  *) asset=llm-hub-windows-x86_64.exe ;;
esac

size=$(wc -c <"$binary" | tr -d ' ')
release=$(curl -fsSL ${GITHUB_TOKEN:+-H "Authorization: Bearer $GITHUB_TOKEN"} \
  "https://api.github.com/repos/$repo/releases/latest")
tag=$(printf '%s' "$release" | jq -r .tag_name)
base=$(printf '%s' "$release" | jq -r --arg asset "$asset" '.assets[] | select(.name == $asset) | .size')
if [ -z "$base" ]; then
  echo "binary size: $size bytes. Release $tag has no $asset asset to compare with."
  exit 0
fi

awk -v size="$size" -v base="$base" -v tag="$tag" -v asset="$asset" -v max="$max_growth_pct" 'BEGIN {
  change = size - base
  pct = change * 100 / base
  printf "| Binary | Size |\n|---|---|\n"
  printf "| %s in %s | %.2f MiB |\n", asset, tag, base / 1048576
  printf "| this build | %.2f MiB |\n\n", size / 1048576
  printf "Change: %+.2f MiB (%+.1f%%). Allowed without a reason: %s%%.\n", change / 1048576, pct, max
  exit (pct > max) ? 1 : 0
}'
