#!/bin/sh
# Median and p90 time to first byte of llm-hub binaries against the mock upstream, interleaved.
# Usage: bench.sh <label=binary> [<label=binary> ...]. Env: BODY_KB (default 1), RUNS (300),
# RULES (a rules file to load), ROUNDS (2).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
upstream_port=18901
hub_port=18902
runs="${RUNS:-300}"
rounds="${ROUNDS:-2}"

python3 -I "$here/mock_upstream.py" "$upstream_port" &
upstream_pid=$!
trap 'kill "$upstream_pid" 2>/dev/null; wait "$upstream_pid" 2>/dev/null; rm -rf "$work"' EXIT
sleep 1

python3 -I -c "
import json, sys
kb = int(sys.argv[1])
body = {'model': 'mock/m', 'stream': True, 'temperature': 1,
        'messages': [{'role': 'user', 'content': 'x' * (kb * 1024)}]}
open(sys.argv[2], 'w').write(json.dumps(body))
" "${BODY_KB:-1}" "$work/body.json"

ttfb() {
  curl -s -o /dev/null -w '%{time_starttransfer}\n' -X POST "localhost:$hub_port/v1/chat/completions" \
    -H 'content-type: application/json' --data-binary "@$work/body.json"
}

run() {
  label=$1
  binary=$2
  rm -rf "$work/run" && mkdir "$work/run"
  if [ -n "${RULES:-}" ]; then
    cp "$RULES" "$work/run/extensions.yaml"
  fi
  (cd "$work/run" && LLM_HUB_PORT=$hub_port LLM_HUB_AUTO_UPDATE=0 LLM_HUB_PROFILES=mock \
    LLM_HUB_MOCK_BASE_URL="http://127.0.0.1:$upstream_port/v1" exec "$binary" >hub.log 2>&1) &
  hub_pid=$!
  sleep 2.5
  for _ in $(seq 1 20); do ttfb >/dev/null; done
  for _ in $(seq 1 "$runs"); do ttfb; done | sort -n | awk -v l="$label" \
    '{a[NR]=$1} END {printf "%-24s median %.3f ms  p90 %.3f ms\n", l, a[int(NR/2)]*1000, a[int(NR*0.9)]*1000}'
  kill "$hub_pid" 2>/dev/null
  wait "$hub_pid" 2>/dev/null || true
}

for round in $(seq 1 "$rounds"); do
  echo "round $round"
  for pair in "$@"; do
    run "${pair%%=*}" "${pair#*=}"
  done
done
