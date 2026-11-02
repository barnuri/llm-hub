#!/bin/sh
# Prints median time to first byte through the hub and against the direct upstream, and fails if the hub adds more than MAX_ADDED_MS.
# Set UPSTREAM_URL and UPSTREAM_MODEL (the model the hub resolves for Efficiency); HUB_URL, HUB_MODEL, HUB_AUTH, UPSTREAM_AUTH and RUNS are optional.
set -eu

HUB_URL="${HUB_URL:-http://127.0.0.1:8888/v1/chat/completions}"
HUB_MODEL="${HUB_MODEL:-hub/smart-auto-efficiency}"
HUB_AUTH="${HUB_AUTH:-}"
UPSTREAM_URL="${UPSTREAM_URL:?set UPSTREAM_URL to the direct upstream chat completions URL}"
UPSTREAM_MODEL="${UPSTREAM_MODEL:?set UPSTREAM_MODEL to the model the hub resolves for Efficiency}"
UPSTREAM_AUTH="${UPSTREAM_AUTH:-}"
RUNS="${RUNS:-10}"
MAX_ADDED_MS="${MAX_ADDED_MS:-50}"

case "$RUNS" in
  ''|*[!0-9]*|0) echo "RUNS must be a positive integer" >&2; exit 2 ;;
esac

# Sends one streaming request, prints its time to first byte in milliseconds, and fails on a non-2xx status.
ttfb_ms() {
  url=$1
  model=$2
  auth=$3
  body=$(printf '{"model":"%s","stream":true,"max_tokens":8,"messages":[{"role":"user","content":"ping"}]}' "$model")
  if [ -n "$auth" ]; then
    set -- -H "$auth"
  else
    set --
  fi
  result=$(curl -sS -o /dev/null --max-time 120 -X POST \
    -H 'Content-Type: application/json' "$@" --data "$body" \
    -w '%{time_starttransfer} %{http_code}' "$url") || return 1
  code=${result##* }
  case "$code" in
    2??) ;;
    *) echo "request to $url returned HTTP $code" >&2; return 1 ;;
  esac
  seconds=${result% *}
  awk -v s="$seconds" 'BEGIN { printf "%.3f\n", s * 1000 }'
}

# Reads one value per line on stdin and prints the median.
median() {
  sort -n | awk '{ v[NR] = $1 } END { if (NR == 0) exit 1; printf "%.3f\n", v[int((NR + 1) / 2)] }'
}

# One unmeasured request per target opens the connections and warms the hub.
ttfb_ms "$HUB_URL" "$HUB_MODEL" "$HUB_AUTH" >/dev/null
ttfb_ms "$UPSTREAM_URL" "$UPSTREAM_MODEL" "$UPSTREAM_AUTH" >/dev/null

hub_samples=""
upstream_samples=""
run=1
while [ "$run" -le "$RUNS" ]; do
  hub_ms=$(ttfb_ms "$HUB_URL" "$HUB_MODEL" "$HUB_AUTH")
  upstream_ms=$(ttfb_ms "$UPSTREAM_URL" "$UPSTREAM_MODEL" "$UPSTREAM_AUTH")
  echo "run $run: hub ${hub_ms} ms, direct upstream ${upstream_ms} ms"
  hub_samples="${hub_samples}${hub_ms}
"
  upstream_samples="${upstream_samples}${upstream_ms}
"
  run=$((run + 1))
done

hub_median=$(printf '%s' "$hub_samples" | median)
upstream_median=$(printf '%s' "$upstream_samples" | median)
added=$(awk -v h="$hub_median" -v u="$upstream_median" 'BEGIN { printf "%.3f", h - u }')

echo "hub TTFB median: ${hub_median} ms"
echo "direct upstream TTFB median: ${upstream_median} ms"
echo "hub added: ${added} ms (limit ${MAX_ADDED_MS} ms)"

if awk -v a="$added" -v m="$MAX_ADDED_MS" 'BEGIN { exit !(a > m) }'; then
  echo "FAIL: the hub adds more than ${MAX_ADDED_MS} ms" >&2
  exit 1
fi
echo "PASS"
