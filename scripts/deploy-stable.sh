#!/bin/bash
# Deploys a merged ref of llm-hub as the always-on service, from a dedicated clone.
#   scripts/deploy-stable.sh --clone DIR [--ref origin/master] [--releases DIR]
#                            [--keep 3] [--skip-ci] [--dry-run]
# Releases live in <releases>/<sha>/llm-hub; <releases>/current links to the live one.
# The service runs in the parent of <releases> (put .env and logs there).
set -uo pipefail

CLONE=""
REF="origin/master"
RELEASES="${LLM_HUB_RELEASES:-$HOME/.local/share/llm-hub/releases}"
KEEP=3
SKIP_CI=false
DRY=false
HEALTH_URL="${LLM_HUB_HEALTH_URL:-http://127.0.0.1:8888/healthz}"
HEALTH_WAIT="${LLM_HUB_HEALTH_WAIT:-20}"

while [ $# -gt 0 ]; do
  case "$1" in
    --clone) CLONE="${2:-}"; shift 2 ;;
    --ref) REF="${2:-}"; shift 2 ;;
    --releases) RELEASES="${2:-}"; shift 2 ;;
    --keep) KEEP="${2:-}"; shift 2 ;;
    --skip-ci) SKIP_CI=true; shift ;;
    --dry-run) DRY=true; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [ -z "$CLONE" ] || [ ! -d "$CLONE/.git" ]; then
  echo "usage: deploy-stable.sh --clone <dedicated clone dir> [--ref REF] [--releases DIR] [--keep N] [--skip-ci] [--dry-run]" >&2
  exit 2
fi
CLONE="$(cd "$CLONE" && pwd)"
RUN_DIR="$(dirname "$RELEASES")"
LOCK="$RELEASES/.deploy.lock"

say() { echo "deploy-stable: $*"; }
do_or_say() { if $DRY; then say "would run: $*"; else "$@"; fi; }

discard() {
  if command -v trash >/dev/null; then trash "$1"; else mv "$1" "$RELEASES/.old-$(basename "$1")"; fi
}

healthy() {
  for _ in $(seq 1 "$HEALTH_WAIT"); do
    curl -fsS -o /dev/null --max-time 1 "$HEALTH_URL" && return 0
    sleep 1
  done
  return 1
}

install_service() { (cd "$RUN_DIR" && "$RELEASES/current/llm-hub" service install); }

mkdir -p "$RELEASES"
if ! mkdir "$LOCK" 2>/dev/null; then
  say "another deploy holds $LOCK"
  exit 0
fi
trap 'rmdir "$LOCK"' EXIT

git -C "$CLONE" fetch --quiet origin || { say "fetch failed"; exit 1; }
SHA="$(git -C "$CLONE" rev-parse "$REF^{commit}")" || { say "cannot resolve $REF"; exit 1; }
CURRENT_DIR=""
[ -L "$RELEASES/current" ] && CURRENT_DIR="$(cd "$RELEASES/current" 2>/dev/null && pwd -P)"
PREVIOUS_SHA="$(basename "${CURRENT_DIR:-none}")"
say "ref $REF is $SHA, deployed ${PREVIOUS_SHA}"
if [ "$PREVIOUS_SHA" = "$SHA" ]; then
  say "already deployed"
  exit 0
fi

if $SKIP_CI; then
  say "skipping the CI check"
else
  slug="$(git -C "$CLONE" remote get-url origin | sed -E 's#^.*[:/]([^/:]+/[^/]+)$#\1#; s#\.git$##')"
  states="$("${GH:-gh}" api "repos/$slug/commits/$SHA/check-runs?per_page=100" \
    --jq '[.check_runs[] | (.conclusion // .status)] | unique | join(",")' 2>/dev/null)" \
    || { say "cannot read the checks of $SHA"; exit 1; }
  if [ -z "$states" ] || echo "$states" | tr ',' '\n' | grep -qvE '^(success|skipped|neutral)$'; then
    say "checks of $SHA are not green ($states)"
    exit 1
  fi
  say "checks are green"
fi

changed_ui=true
if [ -n "$CURRENT_DIR" ] && git -C "$CLONE" cat-file -e "$PREVIOUS_SHA^{commit}" 2>/dev/null; then
  git -C "$CLONE" diff --quiet "$PREVIOUS_SHA" "$SHA" -- ui-src && changed_ui=false
fi

if $DRY; then
  say "would check out $SHA in $CLONE"
  $changed_ui && say "would run: npm run build:ui"
  say "would run: GIT_SHA=$SHA cargo build --release --locked"
  say "would copy the binary to $RELEASES/$SHA/llm-hub"
  say "would point $RELEASES/current at $SHA and run: current/llm-hub service install"
  say "would wait up to ${HEALTH_WAIT}s for $HEALTH_URL, roll back to $PREVIOUS_SHA on failure"
  say "would keep the last $KEEP releases"
  exit 0
fi

git -C "$CLONE" checkout --quiet --detach "$SHA" || exit 1
cd "$CLONE" || exit 1
if $changed_ui; then
  npm ci --silent && npm run build:ui || { say "ui build failed, keeping the running service"; exit 1; }
fi
GIT_SHA="$SHA" cargo build --release --locked || { say "cargo build failed, keeping the running service"; exit 1; }

mkdir -p "$RELEASES/$SHA"
cp "${CARGO_TARGET_DIR:-$CLONE/target}/release/llm-hub" "$RELEASES/$SHA/llm-hub" || exit 1
ln -sfn "$RELEASES/$SHA" "$RELEASES/current"

if install_service && healthy; then
  say "deployed $SHA, healthz 200"
  ls -1dt "$RELEASES"/*/ 2>/dev/null | tail -n +$((KEEP + 1)) | while read -r old; do
    name="$(basename "$old")"
    if [ "$name" != "$SHA" ] && [ "$name" != "$PREVIOUS_SHA" ]; then
      discard "$old"
    fi
  done
  exit 0
fi

say "deploy of $SHA failed"
discard "$RELEASES/$SHA"
if [ -n "$CURRENT_DIR" ] && [ -d "$CURRENT_DIR" ]; then
  ln -sfn "$CURRENT_DIR" "$RELEASES/current"
  install_service && say "rolled back to $PREVIOUS_SHA"
fi
exit 1
