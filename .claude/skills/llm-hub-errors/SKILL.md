---
name: llm-hub-errors
description: >
  Read every error llm-hub recorded (the /api/errors feed, the profile health flags, the
  per-profile logs and the launchd logs), group them by root cause, fix the ones the code can
  fix in the llm-hub repo, rebuild and reinstall the service, and report what is left and why.
  Use when llm-hub shows failing requests or unhealthy profiles, or to clear the error list.
  Triggers: "fix llm-hub errors", "solve the hub errors", "failed upstream profiles", "why is
  profile X unhealthy", "clear the errors in llm hub".
allowed-tools: Bash, Read, Grep, Glob, Edit, Write, AskUserQuestion
---

# llm-hub-errors

Run from the repo root. Hub: `http://127.0.0.1:8888`. Read the repo `CLAUDE.md` first: it sets the
latency rules and the rebuild steps. Read `local.md` in this folder if it exists. It holds
machine-specific paths and overrides the defaults below.

Read-only collection never needs approval. Editing source, tests, and the rebuild do not need
approval either. Ask (AskUserQuestion) only before a profile `DELETE`, a `.env` change, a
publish, or marking rows solved.

## Steps

1. Collect. Run in parallel:
   - `curl -s "http://127.0.0.1:8888/api/errors?range=7d"`. The open set is the `recent[]` rows with
     `resolved_ms` null: `... | jq '[.recent[] | select(.resolved_ms == null)]'`.
   - `curl -s http://127.0.0.1:8888/api/profiles`
   - `curl -s http://127.0.0.1:8888/healthz`
   - `launchctl print gui/$(id -u)/com.barnuri.llm-hub | head -40`
2. Test each enabled profile: `curl -s -X POST http://127.0.0.1:8888/api/profiles/<name>/test`.
   Managed profiles start on demand. The test starts them first.
3. Read the DB for the full picture (read-only). Only rows in the open set from step 1 are current:
   `sqlite3 -readonly llm-hub.db "select datetime(ts_ms/1000,'unixepoch','localtime'), profile, model, status, substr(error,1,160) from requests where status>=400 order by ts_ms desc limit 200;"`
4. Read the process logs: `logs/<profile>.log` and `logs/auto-reinstall.log`. Take the last 50
   lines of each. Strip ANSI codes first. Also tail the launchd stderr log named in the plist.
5. Group only the open errors (`resolved_ms` null) by signature (profile + status + the first 90 chars of the message). For
   each group, state the cause and mark it:
   - `code`: the source can fix it. Find the path with Grep and fix it.
   - `config`: a `.env` value or the upstream account is wrong. Report the fix. Do not edit `.env`
     without asking.
   - `upstream`: a rate limit, outage, or timeout the hub cannot change. Report it. Check whether
     the last 24 hours still show it. Old rows are history.
6. Fix each `code` group with a test next to the code. Run the targeted test first:
   `cargo test --bins <test_name>`. Then run `cargo test` in full.
7. Rebuild and reinstall (required after Rust or UI changes, per the repo `CLAUDE.md`):
   `npm run build:ui` (only when `ui-src/` changed), `cargo build --release`,
   `./target/release/llm-hub service install`, then `curl -s http://127.0.0.1:8888/healthz` must print `ok`.
8. Verify. Re-run steps 1 and 2. Every profile that was failing must show `healthy: true`, or a
   stated reason in step 5. Count the open rows from the last 24 hours in the feed. Expect zero from fixed causes.
9. Mark solved. After verify passes, ask with AskUserQuestion which fixed groups to mark. Then for
   the open rows of those groups with a non-null `id`, run
   `curl -s -X POST http://127.0.0.1:8888/api/errors/resolve -H 'Content-Type: application/json' -d '{"ids":[...],"solved":true}'`
   (max 500 ids per call). Marking a reason also covers its older occurrences. A newer occurrence
   of a solved reason shows as open again. Use `"solved":false` only to reopen rows the user names.

## Known signatures (check these first)

| Signature | Cause | Status |
|---|---|---|
| `profile <managed> healthy: false`, test returns 404 on `/v1/models` | whisper-server has no models route | fixed: static-model profiles count a 404 as up |
| `profile <managed> healthy: false`, `connection refused` on test | managed process was stopped by the idle reaper | fixed: the test starts the process first |
| `"thinking.type.enabled" is not supported` (400, copilot native) | Haiku 5.5 wants adaptive thinking | fixed: native body rewrites enabled to adaptive |
| `The use of the web search tool is not supported` (400) | server tools sent to Copilot native | fixed: `drop_server_tools` |
| `404 page not found` on `copilot/gpt-*-terra` | model is Responses-only | check bridged route still serves it |
| `429 too many requests` bursts | Copilot rate limit | upstream, report only |
| `599 connect error ... githubcopilot.com` | network or Copilot outage | upstream, report only |
| `408 Timed out reading request body` | very large request on Copilot | upstream or client, report only |

## Report format

Lead with the result line: `N profiles healthy of M, K error groups fixed, J left (upstream)`.
Then one line per group: code, status, test name. Use absolute paths for any file you wrote.
End with one next action.
