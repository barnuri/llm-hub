#!/bin/bash
# Rebuilds llm-hub and reloads the LaunchAgent when sources are newer than the release binary.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$REPO/target/release/llm-hub"
LOCK="$REPO/target/.auto-reinstall.lock"
LOG="$REPO/logs/auto-reinstall.log"
HEALTH_URL="http://127.0.0.1:8888/healthz"

newer_than_binary() {
  [ ! -e "$BINARY" ] && return 0
  [ -n "$(find "$@" -newer "$BINARY" -type f -print -quit 2>/dev/null)" ]
}

mkdir -p "$REPO/logs" "$REPO/target"
if ! mkdir "$LOCK" 2>/dev/null; then
  exit 0
fi
trap 'rmdir "$LOCK"' EXIT

cd "$REPO" || exit 1
if ! newer_than_binary src ui-src/src Cargo.toml Cargo.lock; then
  exit 0
fi
if [ -n "${AUTO_REINSTALL_DRY_RUN:-}" ]; then
  echo "would rebuild: sources are newer than $BINARY"
  exit 0
fi

{
  echo "== $(date '+%Y-%m-%d %H:%M:%S') sources changed, rebuilding"
  if newer_than_binary ui-src/src && ! npm run build:ui; then
    echo "ui build failed, keeping the running service"
    exit 1
  fi
  if ! cargo build --release; then
    echo "cargo build failed, keeping the running service"
    exit 1
  fi
  "$BINARY" service install
  for _ in $(seq 1 30); do
    if curl -fsS -o /dev/null --max-time 1 "$HEALTH_URL"; then
      echo "healthz 200"
      exit 0
    fi
    sleep 0.5
  done
  echo "healthz did not return 200 within 15 s"
  exit 1
} >>"$LOG" 2>&1
