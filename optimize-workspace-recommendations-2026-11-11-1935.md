# Workspace Optimization Recommendations

**Date:** 2026-11-11 19:35 (+02:00), run 2 of the day (first run: `findings-2026-11-11.md`, preserved)
**Harness:** Copilot CLI 1.0.110-1 (primary, this run) and Claude Code (via llm-hub on `:8888`) · **Model setting:** Copilot `auto`, plan model `claude-opus-5.5`; Claude Code `copilot/claude-opus-5-5[1m]` · **Plan:** Copilot Business, quota-metered
**Audited by:** `optimize-workspace` skill, run by a delegated agent as the assigned Sonnet 5.5 run. Read-only. No settings, proxy config or skills were changed.

Legend: **M** = measured, **E** = estimate. Scope is stated per number; Copilot CLI session logs, llm-hub requests and Claude Code transcripts are different populations and are not compared.

## Verdict

F1. Copilot credits are the dominant cost: **$987.73 of $1,527 used (35.2% left) on 2026-11-11** (M, quota). Previous run read $820.82 on 2026-11-10, so +$166.91 in about one day. At the 11-day average of $89.8/day (E), the remaining $539 lasts about 6 days (E; overage is permitted, so this is a bill risk, not a stop).
F2. Opus 5.5 ($453) and Sonnet 5.5 ($294) carry 76% of the $985 hub estimate (M). Haiku 5.5 is $192, of which $183 is the long-context tier (M).
F3. The Copilot CLI 128-tool error is independently confirmed (see Copilot CLI section). `toolSearch` is `true` in the current settings, yet 206 and 231 tools were sent. Deferral is not shown to fix it.

---

## Baseline

| Metric | Measured | Notes |
|---|---|---|
| Copilot quota | $987.73 / $1,527.00, resets 2026-12-01 | M, `measure_copilot_cost.py`; hub estimate $985.03, within 0.3% |
| Cost by component (since 11-01) | cache read $452 (46%) · cache write $364 (37%) · output $131 (13%) · input $38 (4%) | M |
| Cache write by cause | no session id $221 · idle over 5 min $55 (89 req, $0.62 each) · warm $44 · model switch $42 (741 req) | M; 18,116 requests carry no session id, so their cause is unattributed |
| Claude Code turns above 200k | 81.9% of 15,136 turns (12 largest transcripts) | M, sample biased to big sessions; above 600k 25.9%; peak 930,350; 5 of 12 never compacted |
| Median startup context (Claude Code) | 49,986 tok (40 newest sessions, range 25,301-65,896) | M; heaviest ever 205,102 |
| Transcript corpus | 2.6 GB / 2,385 JSONL, `cleanupPeriodDays` 36500 | M |
| Reasoning effort | settings `medium`, shell unset in this process | prior run saw `high` in the shell; not re-checked in a login shell |
| llm-hub (14 d) | 31,321 ok / 231 failed; 6.05B input tokens; cache hit 95.8% | M; switched dispatches hit 20.2% (134) vs 95.7% stayed (10,472) |
| llm-hub TTFB median | not re-measured this run | no request-path change here |

`ANTHROPIC_BASE_URL`: not set in settings or this process. `~/.zshrc:165` sets it to a gateway only inside a launcher. The previous run recorded `http://localhost:8888` for the audited Claude Code session. Whether tool search is on therefore depends on the launcher, and the startup number above is for sessions as launched. Not re-verified with a `claude -p` probe.

---

## Copilot CLI

**Coverage.** Copilot CLI 1.0.110-1 (`~/.copilot`). Docs for this build were read first (`toolSearch`, `statusLine.refreshInterval`, `contextManagementTools`, hooks). Sources: `/Users/barnu/.copilot/settings.json`, `mcp-config.json`, `session-state/*/events.jsonl` (203 sessions, 2026-09-30 to 2026-11-11, versions 1.0.89 to 1.0.110-1), `logs/`, `session-store.db`. Prompts, assistant text and credentials were not read or copied. `config.json` is machine-managed (comment header, not JSON), only its header was seen.

### Effective settings (M, read from file)

| Setting | Value | Source / note |
|---|---|---|
| `toolSearch` | `true` | `~/.copilot/settings.json`, explicit. Docs: "deferred tool loading" |
| `contextManagementTools` | `true` | explicit (default false). Adds `session_artifacts`, `get_context_remaining`, `session_history`, `new_context` tools |
| `contextTier` / `planContextTier` | `long_context` / `long_context` | explicit. Claude Haiku 5.5 and GPT-6 bill a higher rate above their tier threshold |
| `model` / `planModel` | `auto` / `claude-opus-5.5` | explicit |
| `defaultMode` / `defaultPermissionMode` / `stayInAutopilot` | `autopilot` / `allow-all` / `true` | explicit |
| `statusLine.refreshInterval` | `2` (s) | explicit; renderer is `bun run .../harness-status-line/src/index.ts`. Not timed this run (ruled out as a turn blocker, see Ruled out) |
| `experimental`, `memory`, `keepAlive` | `true`, `true`, `on` | explicit |
| Hooks (user scope) | one `SessionStart` (`herdr-agent-state.sh`, timeout 10 s) | no `PreToolUse` or prompt hooks in Copilot user settings |
| `disabledMcpServers` | `claude-design` | the other 6 servers in `mcp-config.json` are enabled: my-custom-jira, docs-langchain, agents-sdk, mon, context7, playwright (all `tools: ["*"]`) |
| Plugins | 8 enabled, 9 disabled | `enabledPlugins` |
| `disabledSkills` / `disabledHooks` | missing (no value set) | effective default = nothing disabled |

### Observed runtime (M, `events.jsonl`)

| Metric | Value | Scope |
|---|---|---|
| Sessions / tool calls / failed calls | 203 / 10,278 / 1,455 | all sessions |
| Failed calls excluding `read_agent` | 168 (view 42, apply_patch 35, web_fetch 23, read_bash 13, bash 13) | all sessions |
| `read_agent` | 1,287 of 1,312 calls failed, all `not_found`; 16 sessions; 1.0.90-7 (719/719) and 1.0.91-1 (567/567); newest loops 2026-10-01/02; none seen since 1.0.105 | historical loop (one session had 408 consecutive failures in 10 min) |
| Model-call latency (261 calls with `model.model_call_success`) | TTFT median 1,337 ms, p95 3,380 ms; call duration median 1,679 ms, p95 5,854 ms | sample only: 261 calls |
| Tool definitions in prompt (`toolDefinitionsTokens` at shutdown, 177 sessions) | median 40,354; p95 46,118; max 56,794 tok. System prompt median 9,736 | field is the tool-schema size sent, per session |
| Same field by version and model class | 1.0.89-1.0.90-7: 7.4k-14.3k median. 1.0.91-1: 46,118 for 85 BYOK sessions, 13.6k for 11 native. 1.0.102-1 onward: 36.9k-55.5k | confounded by version and class; the cause of the jump is not established |
| Hooks (218 start/end pairs) | 209 `sessionStart`, 4 `agentStop`, 4 `postToolUse`, 1 `userPromptSubmitted`; 0 failed; duration median 45 ms, p95 222 ms, max 1.95 s | all sessions |
| MCP tool calls | 9 of 10,278 (0.09%), matched by name prefix for 7 servers | six MCP servers are enabled; per-server counts not broken out |
| Result size (chars, sum / max) | view 8.64M / 92.5k (1,940 calls) · bash 5.98M / 20.3k (3,002) · rg 1.38M / 19.8k (406) · web_fetch 0.72M / 20.2k | M; characters, not tokens |
| Session `errors` | `quota_exceeded` 402 x36 (2026-09-30, 10-01); model-provider unreachable at `http://127.0.0.1:1/v1` x11 (10-04..11-01); 4 gpt-4.1 tool-limit 400s (11-11) | all sessions |
| `toolCount` field on model calls | recorded as null on all 261 calls | unavailable; do not use it as a tool count |

### F3. 128-tool limit: confirmed independently, cause not proven

- Confirmed in two independent records. Copilot CLI session `782bd74a-4bd7-4959-b7e6-cf3543e4f6eb` (`1.0.110-1`, model auto-switched to `llmgw/gpt-4.1` at 17:23:00Z) logged four `session.error` 400s. llm-hub's `requests` table and `logs/llm-hub.log` show the same four `request_id`s on `/v1/chat/completions` (Azure via LLM Gateway, client `OpenAI/JS 5.20.1`). Messages: `Invalid 'tools': array too long. Expected ... maximum length 128, but got ... 206` (2 requests) and `... 231` (2 requests).
- Observed count 206 then 231 against a limit of 128. The count grew by 25 between the first two and last two requests (cause not recorded; the session logged 2 `skill.invoked_ref` events).
- Session shutdown recorded 46,382 tokens of tool definitions and 12,022 system tokens.
- **Setting vs observation.** `toolSearch` is `true` in the file as read at 19:3x. The value at 17:23Z is not recorded: `settings.json` is rewritten by the CLI continuously, so its mtime does not date the setting. If `true` was already set, the 206/231 arrays show deferral did not apply to this gpt-4.1 BYOK path. If it was not, the setting is unrelated. This run cannot distinguish the two.
- **Not claimed:** that deferred loading fixes it. A fix needs a before and after request on the same provider (`llmgw/gpt-4.1`, same session shape) with `toolSearch` on and off, which this audit may not run (read-only, no setting changes). Ask: owner runs the A/B and records the tool array length.
- Related and also unconfirmed as a cause: 6 MCP servers all expose `tools: ["*"]`, `mon` alone exposes about 120 tools in this harness's deferred list (counted from this session's tool listing, E).

### Recommendations (Copilot CLI)

C1. **Run the toolSearch A/B before changing anything** (owner action, 2 requests). Same provider `llmgw/gpt-4.1`, flip `toolSearch` once, record `tools` length from llm-hub's log line. Evidence: F3. Expected effect: unknown until measured.
C2. **Review the six always-enabled MCP servers** (owner action). 9 MCP calls in 10,278 tool calls (0.09%, M) while tool schemas are 40,354 tok median per BYOK session (M). The two numbers are not proven to be related. Disable-by-session (`/mcp disable <name>` or `disabledMcpServers`) before the A/B in C1 would also show how much of the 206 is MCP. Saving: not measured.
C3. **Do not read `toolCount` for tool counts.** It is null on all 261 calls. Use llm-hub logs or `toolDefinitionsTokens`.
C4. **Check why Copilot CLI called `https://api.individual.githubcopilot.com/models`.** Logged in 1.0.89, 12 `session.error` rows on 2026-09-30 (`Error loading model list` x3, model-provider attribution x9). This is the endpoint the organization forbids. This audit made no such request. Owner: confirm which configuration sends it and that current sessions (1.0.110-1) do not; no such error occurred in any session after 2026-09-30.

---

## Recommendations — ranked by measured impact

### 1. Cut the cache-write spend: 37% of Copilot cost

- **Evidence (M, 2026-11-01..11-11):** cache write $364 = $221 (18,116 requests with no session id) + $55 idle over 5 min (89 req) + $44 warm + $42 model switch (741 req). Model switches: stayed 95.7% cache hit vs switched 20.2% (134 dispatches, 10.06M write tokens).
- **Change:** habit and llm-hub work. (a) Stay on one model per task: no mid-task `/model` change. (b) Attribute the 18,116 requests with no session id (client or `X-` header, a hub gap): add as data gap G8. (c) Idle rewrites: `/compact` or `/clear` before a break over 5 min (E5).
- **Expected effect:** the idle share is bounded at $55 and the switch share at $42 (M, upper bounds, not savings). The $221 unattributed pool is the largest and cannot be acted on until attributed.
- **Risk:** none for (a), (b).

### 2. Opus and Sonnet carry 76% of the estimate

- **Evidence (M):** Opus $453 over 4,566 requests ($0.099 each); Sonnet $294 over 5,432 ($0.054 each). Haiku $192 over 13,618 requests ($0.014 each) with $183 long-tier. 40% of Opus and 54% of Sonnet requests exceed 200k context (M).
- **Change:** habit, owner decision. The default model `copilot/claude-opus-5-5[1m]` is the user's choice. Previous run: about 98% of Opus traffic bypasses Smart Auto; this run's selection_events show 19 Opus Smart Auto picks (M) and 1,671 `pinned` dispatches (M). Do not change the model without asking.
- **Expected effect:** not estimated. A counterfactual from the previous run (Opus tokens at Sonnet rates: $181.3 saved over 10 days) is a ceiling (E), not re-run here.

### 3. Tool results kept in Claude Code context (carry cost)

- **Evidence (E; 14 days, 211 transcripts, 38,105 calls; `$0.50/1M` read price is an assumption, not a measured price):** total tool-result carry about $1,116. Top: `Read` $122, `mcp__mon` $90, `mcp__playwright` $64, `Bash:sed` $55, skill `code-gen` $54, shell loops $46, `Bash:cat` $46.
- **Change:** ranged `Read` instead of `sed`/`cat` of whole files; filter `mon` queries; small playwright snapshots.
- **Expected effect:** the 62 MCP results above 5k tokens carry $62.83 (E) and the 48 Bash outputs above 5k carry $27.94 (E). Upper bounds only.
- **Risk:** none.

Below the noise floor and not ranked: status-line refresh, plugin descriptions, stale plugin dirs.

---

## Habits, not settings

H1. One model per task. Switched dispatches hit 20.2% cache vs 95.7% (M).
H2. `/compact` or `/clear` before a break over 5 min: 89 idle rewrites cost $55, $0.62 each (M).
H3. `/clear` between unrelated tasks. 81.9% of sampled Claude Code turns exceed 200k (M, biased sample).
H4. Stop polling dead agents. 1,287 `read_agent` `not_found` calls in 16 sessions (M, last 2026-10-02). Not seen in 1.0.105 and later.
H5. Use ranged reads. 2 whole-file reads above 10k tokens (M); Read $122 and Bash sed/cat $101 of carry (E).

---

## Hooks and tool-loop health

**Coverage:** Claude Code (current project `llm-hub`, `~/.claude/settings.json`, project `.claude/settings.json` and `settings.local.json` exist and were not opened), Copilot CLI 1.0.110-1 (`~/.copilot/settings.json`, 218 hook records). Managed policy fragments and plugin hooks were not inspected. No hook was executed.

| Source, event, matcher, handler | Activation | Calls / failures / timeouts / repeats | Duration samples, median / p95 | Evidence and owner action |
|---|---|---|---|---|
| `~/.copilot/settings.json` `SessionStart` `herdr-agent-state.sh` (command, timeout 10 s) | observed (209 runs) | 209 / 0 / 0 / unknown | 218 pairs (all hook types), 45 ms / 222 ms, max 1.95 s | `hook.start`/`hook.end` in session events. No action |
| `agentStop` (4), `postToolUse` (4), `userPromptSubmitted` (1) | observed in session events | 9 runs, 0 failed | included above | not in `~/.copilot/settings.json`; source unknown (repo-level `.claude`/`.github` hooks are also read by Copilot CLI per docs) |
| `~/.claude/settings.json` `PreToolUse` `.*` `/usr/local/bin/checkpoint-hooks-client claude` (command, 60 s, synchronous) | configured only | unknown | unknown | managed security client. Do not bypass. No timings recorded in Claude Code hook logs read here |
| `~/.claude/settings.json` `UserPromptSubmit` `.*` same client (60 s) | configured only | unknown | unknown | same |
| `~/.claude/settings.json` `PreToolUse` `*` `python3 -I .../web-terminal/scripts/reapply-claude-hook-noop.py` | configured only | unknown | unknown | runs on every tool call. Its name matches the incident's "no-op replacement" workaround. Owner: confirm it is still intended |
| `~/.claude/settings.json` `PreToolUse` `Workflow` `workflow-active-marker.sh`, `Stop` `render-overview-stop.sh` (15 s), `SessionStart` `herdr-agent-state.sh` (10 s) | configured only | unknown | unknown | scoped matchers, low risk |

**Configuration churn (M).** `/Users/barnu/.claude/settings.json` is a regular file (not a symlink), mtime 19:34:46, changed again by 19:35:48 in a 75 s sample (1 change in 62 s; none in the last 25 s). 174 `settings.json.bak*` files exist; per hour on 2026-11-11: 15h 18, 16h 5, 17h 75, 18h 46, 19h 30. The launchd job `com.barnuri.claude-settings-reset` (`StartInterval` 30 s, `RunAtLoad`) runs `reset-claude-settings.py`; its log has 302 "settings.json rewritten from source" lines in 365. The live file differs from `/Users/barnu/sandbox/private/skills/claude-user-scope/settings.json` by the `checkpoint-hooks-client` groups (live has them, source does not). The reset job is the writer evidenced by its own log; who adds the security groups was not observed.
**Validation:** `statusLine` is at the settings root, not under `hooks`. No malformed `PreToolUse` entry (every handler has `type`) in the live file. No harness validation diagnostics were read.
**Retries:** 174 backups and 302 resets per day are churn, not proven retries. No correlated hook-attributed tool retry was found; generic tool errors are not attributed to hooks.
**Verdict:** configuration risk without runtime timing (Claude Code). The Copilot hook is measured and fast. Do not rank. Owner investigation: why 302 resets are needed (30 s interval, rewrite each time even if unchanged). Do not copy any no-op replacement for the security hook.

---

## Cost habits, skills and MCP servers

Source `measure_cost_habits.py`, 14 days, **all dollars are estimates** at an assumed `$0.50/1M` cache read.

| Item | Kind | Carry $ (E) | Evidence | Recommendation |
|---|---|---|---|---|
| `mcp__mon` | MCP | 90.48 | 581 calls, 674k result tokens, max 11,999, 30 errors | filter queries, smaller page size |
| `mcp__playwright` + `plugin_playwright` | MCP | 63.57 + 13.32 | 1,064 + 234 calls, 59 + 15 errors (highest error count) | `browser_snapshot` smaller, fewer full-page snapshots; two playwright servers loaded (`mcp-config` `playwright` via `npx` plus the plugin) |
| `mcp__my-custom-jira` | MCP | 27.56 | 91 calls, max 9,163 | field filter |
| `agents-sdk`, `docs-langchain`, `claude-design` | MCP, no calls in the window | 0 | not in top families | `agents-sdk` HTTP initialize took 10.79 s (M, one sample); does not block startup (ruled out) |
| `code-gen` | skill to improve | 53.65 | 59 loads, 409,967 body tokens | E3 trimmed body on 2026-11-10; still the top skill, judge after 2 working days |
| `session-slug` | skill to improve | 18.00 | 68 loads, 141,931 tokens | same |
| `cr` | skill to improve | 14.94 | 19 loads | none yet |
| commit and push (11 sessions, 6 projects), `git add -> push -> ghx pr` (3 sessions, 3 projects) | skill to create | not priced | prompt opening `commit and push`; plain git, but ends in `ghx pr` | `pr-creator` already owns it; check why it is not triggered by "commit and push" |

Tool errors by family: playwright 59+15, `mcp__mon` 30, shell loop 29, `Read` 24.
Recurring prompt `reply with exactly: ok` (7 sessions, 2 projects) is the tool-search probe, not a habit.
Agent memory: 624 markdown files under `~/.claude/agent-memory` were counted but not read this run. Unmeasured.

---

## Experiment status (`local/cost-experiments.md`)

E1, E2, E3, E5: **not judged**. They started 2026-11-10 and need 2 working days of data; today is day 2. E4 and E6 stay `done`. Data-gap candidates this run: **G8** per-request client/session id for the 18,116 unattributed requests ($221 cache write); **G9** the Copilot CLI `toolCount` per model call (null today) or the count of `tools` sent, recorded by the hub on each request.

---

## Ruled out

| Suspect | Verdict | Evidence |
|---|---|---|
| Copilot hooks as a tool-loop delay | cleared for Copilot user scope | 218 hook pairs, 0 failed, median 45 ms, p95 222 ms |
| Status-line renderer blocks turns | cleared (previous baseline) | no new contradicting measurement |
| MCP servers block startup | cleared (previous baseline) | none sets `alwaysLoad`; `agents-sdk` 10.79 s only delays its first tool call |
| Plugin and skill descriptions as the bloat | cleared (previous baseline) | skill descriptions 5,021 tok (44 skills); startup median 49,986 tok |
| Copilot CLI model latency | not a problem in the sample | TTFT median 1,337 ms (261 calls) |

---

## Not performance, but found while measuring

N1. Copilot CLI 1.0.89 called `api.individual.githubcopilot.com` on 2026-09-30 (12 errors). See C4.
N2. llm-hub: 55 `404 page not found` (47 on `copilot/auto`), 42 `429`, 18 `599` connect errors to `api.business.githubcopilot.com`, 22 image-input 500s on `qwen3-coder-next` (M, 14 days). Prior items F10 (image and limit gates) may not cover non-chat paths; unverified.
N3. llm-hub: `llmgw/azure/gpt-5.4-mini` 33 failures (5 `405 Method Not Allowed`, rest unseen), `thinking.type.enabled` 3, `output_config` 6, web-search tool 12 rejected 400s on Haiku/Sonnet.
N4. `ivrit.log` and `whisper.log` have 14 error lines each, last modified 2026-11-08; `auto-reinstall.log` 116 error lines (M, counted not read).
N5. `~/.claude/mod-errors/summarize.jsonl`: 1 error, `git state failed for .../skills`, 2026-11-11T15:56Z. Fix lives in the mod repo `register.tsx`; not edited.
N6. Policy: no credentials were printed. `~/.zshrc:165` sets a gateway URL inline; not reproduced.

## Blocked or not measured

- Copilot CLI `toolSearch` before/after A/B: needs a setting change, out of scope.
- `toolSearch` value at 17:23Z: not recorded.
- Claude Code hook durations for `checkpoint-hooks-client`, `reapply-claude-hook-noop.py`: no hook logs read, `unknown`.
- Copilot CLI session usage in dollars: `session.shutdown.modelMetrics` carries tokens only here; dollars come from llm-hub and cover traffic through the hub, not direct CLI traffic.
- llm-hub TTFB vs direct upstream: not re-measured.
- Applying proxy config: skipped. No config-fixable proxy finding was identified this run.
- `~/.claude/knowledge/optimize-workspace-cadence.md`, `ruled-out.md` and `local/cost-experiments.md` not updated (parent-owned).

## How to re-run

```
/optimize-workspace
```
