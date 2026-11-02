# Smart Auto

Smart Auto is a virtual model that llm-hub resolves for each request. The caller names a tier,
and the hub picks a concrete model from the models it serves, adds a reasoning effort, and for
the Balance and Intelligence tiers has other models review the answer before it is returned.

```sh
llm-hub claude hub/smart-auto-balance
```

Smart Auto runs in the hub only. The request path does no network call for the choice: the pool
and the routing state are in memory, refreshed with the model list.

## Selectors and presets

| Selector | Meaning |
|---|---|
| `hub/smart-auto` | The default tier, Balance. |
| `hub/smart-auto-<tier>` | One tier: `efficiency`, `balance`, `intelligence` or `fast`. |
| `hub/smart-auto-long[-<tier>]` | The tier, with a total context window of at least 1,000,000 tokens. |
| `hub/smart-auto-long-input[-<tier>]` | The tier, with a prompt capacity of at least 1,000,000 tokens. |
| `hub/<name>` | An operator preset from `LLM_HUB_SMART_AUTO_PRESETS`. |

Operator presets are a JSON object that maps a name to a policy:

```sh
LLM_HUB_SMART_AUTO_PRESETS='{"team-long": {"tier": "intelligence", "min_context_tokens": 1000000, "include": [["copilot/*"]], "exclude": ["*preview*"]}}'
```

A preset name may use ASCII letters, digits, `-`, `_` and `.`. A name that starts with
`smart-auto` is reserved for the built-in selectors. An unknown `hub/*` name returns 400. The
profile name `hub` is reserved too, so no profile can shadow these selectors.

`/v1/models` lists `hub/smart-auto-balance` first and marks it `recommended: true`. Each `hub/*`
entry has `owned_by: "hub"`. Its limit fields are the smallest value across the models in its
pool.

## Tiers

| Tier | Solver | Review | Reasoning effort |
|---|---|---|---|
| `efficiency` | The cheapest capable model. | None. The answer streams from one call. | Low |
| `balance` (default) | A role-fit model. | One judge and at most one repair. Skipped for plan-backed implementation. | Low for plan-backed implementation, medium otherwise |
| `intelligence` | The strongest role-fit model. | Up to two critics and at most one repair. Skipped for plan-backed implementation. | Medium for plan-backed implementation, high for planning, investigation and implementation, medium otherwise |
| `fast` | The capable model with the best speed score. | None. | Low |

Each review stage has a budget. Balance allows 3 model calls, 64,000 tokens, 256 KiB and 120 s
in total, with 60 s per stage. Intelligence allows 4 calls, 128,000 tokens, 512 KiB and 240 s in
total, with 90 s per stage. A stage that runs out of budget, times out or returns a malformed
verdict leaves the draft as the answer.

Reviewed answers are held until the review finishes, so a Balance or Intelligence answer starts
later than an Efficiency answer. Tool calls are never held: a reply that contains a tool call is
returned at once, unreviewed.

Reviewers are concrete models from the same pool, other than the solver. They prefer another
profile than the solver's. A reviewer that cannot fit the request's estimated prompt is not used.
Review runs on the chat-completions path (`/v1/chat/completions` and `/v1/messages`). A
`/v1/responses` request is served as one stream and is not reviewed. Answers that a native
Anthropic upstream serves are not reviewed either.

## Pool and policy

The pool is the set of concrete chat models that pass every filter, in qualified-id order:

1. The model is a chat model. Speech models and Auto aliases are never in the pool.
2. The model serves the client route, such as `/v1/messages`.
3. No exclude glob matches it. A deny always wins.
4. Every include list has a glob that matches it.
5. The known total context, prompt capacity and output capacity meet the minimums. A model with
   an unknown value never meets a minimum.
6. The estimated prompt, about one token per four request bytes, fits the known prompt capacity.
7. The caller's `x-llm-hub-profiles` filter allows its profile.

A policy only tightens. A selector's policy and the caller's headers are merged, and the merge
keeps the stricter value of each limit, the union of excludes and every include list. Headers
never change the tier that the selector names.

If the pool is empty, llm-hub returns 400 before any upstream call:

```json
{"error": {"message": "smart_auto_no_eligible_model: no model serves hub/smart-auto-balance on /v1/messages under: tier balance, min context 1000000 tokens"}}
```

Fallbacks from `x-llm-hub-fallbacks` or the default chain are filtered by the same pool. A
fallback that the policy forbids is never called.

## Request headers

Every `x-llm-hub-smart-auto-*` header is read by the hub and removed before the request reaches
an upstream. An unknown header with this prefix, an empty value, a repeated single-value header
or a malformed value returns 400.

| Header | Value | Effect |
|---|---|---|
| `x-llm-hub-smart-auto-tier` | `efficiency`, `balance`, `intelligence` or `fast` | Sets the tier when the selector leaves it open. |
| `x-llm-hub-smart-auto-include` | Comma-separated globs over `<profile>/<model>` | Adds an allow-list. A model must match one glob of every list. |
| `x-llm-hub-smart-auto-exclude` | Comma-separated globs | Removes matching models. |
| `x-llm-hub-smart-auto-min-context` | Tokens, such as `1m` or `200k` | Minimum total context. `1m` is 1,000,000. |
| `x-llm-hub-smart-auto-min-input` | Tokens | Minimum prompt capacity. |
| `x-llm-hub-smart-auto-min-output` | Tokens, such as `32k` | Minimum output capacity. |
| `x-llm-hub-smart-auto-role` | `planning`, `investigation`, `implementation`, `validation` or `conversation` | The work role of the request. It replaces a role set by a preset. |
| `x-llm-hub-smart-auto-worker` | An identity, up to 256 characters | Scopes the pin and the history to one worker, such as `<run>/<node>/<attempt>`. |
| `x-llm-hub-smart-auto-parent` | An identity | Groups workers under one workflow in the history. |
| `x-llm-hub-smart-auto-policy` | An identity | A parent policy handle. It is not an authorization. |

Response headers:

| Header | Value |
|---|---|
| `x-llm-hub-model` | The model that served the request, after any review. |
| `x-llm-hub-attempts` | Each attempt with its status, as for every hub request. |
| `x-llm-hub-smart-auto-tier` | The tier that routed the request. |
| `x-llm-hub-smart-auto-role` | The work role of the turn. |
| `x-llm-hub-smart-auto-effort` | The effort written into the body. Absent when the model publishes no levels. |
| `x-llm-hub-smart-auto-reason` | The route reason, such as `fresh`, `pinned`, `incumbent`, `role_switch` or `tool_continuation_replay`. |

## Launch flags

The `llm-hub` agent commands accept the same policy as flags, before the harness name:

| Flag | Same as header |
|---|---|
| `--smart-auto-tier <tier>` | `x-llm-hub-smart-auto-tier` |
| `--smart-auto-min-context <tokens>` | `x-llm-hub-smart-auto-min-context` |
| `--smart-auto-min-input <tokens>` | `x-llm-hub-smart-auto-min-input` |
| `--smart-auto-include <globs>` | `x-llm-hub-smart-auto-include` |
| `--smart-auto-exclude <globs>` | `x-llm-hub-smart-auto-exclude` |
| `--smart-auto-role <role>` | `x-llm-hub-smart-auto-role` |

The Claude launcher sends the flags as lines of `ANTHROPIC_CUSTOM_HEADERS`, after any headers it
already inherits. Other launchers can take only a tier or a one-million minimum, which map to a
built-in preset id. Any other flag fails with a message that names the Claude launcher.

When no model is given and no model variable is set, `llm-hub claude` uses
`hub/smart-auto-balance`. An explicit model is never replaced. See
[Coding agents](coding-agents.md).

## Reasoning effort

llm-hub writes one effort into the body of each routed request, in the field of the client route:

| Route | Field |
|---|---|
| `/v1/chat/completions` | `reasoning_effort` |
| `/v1/responses` | `reasoning.effort` |
| `/v1/messages` | `output_config.effort` |

- A caller's explicit effort is kept when the model publishes that level.
- Otherwise the hub aims for the tier's level (see Tiers) and sends the published level nearest
  to it, with ties going to the higher level. The hub never sends `none`.
- A caller's level that the model does not publish is also moved to the nearest published level.
  The request is not rejected.
- A model that publishes no levels gets no effort field.

Copilot Auto applies the same published-levels rule to its pick. See
[Seeing the pick](copilot-auto.md#seeing-the-pick).

## Roles

The role of a turn is read from the latest real user text. System reminders and skill catalogs
are ignored, and instructions embedded in the text are treated as data. A `role` header or a
preset wins over every rule.

- The first cue word in the text decides the role: planning, investigation, implementation or
  validation. Text with no cue word is implementation when it references a plan, and
  conversation otherwise.
- A reference to a plan or tasks file, an `agent-spec/` path, a command tag or a skill invocation
  marks the turn plan-backed. A plan word in the system prompt does not.
- A tool continuation (a tool result with no new user text) keeps the pinned role and model.

## Affinity and switches

The hub pins one solver per conversation and remembers the solver it used for each role, so a
conversation keeps its model and its prompt cache. The key of a conversation is the worker header,
else the Claude Code session header (`x-claude-code-session-id`), else a hash of the system prompt
and the first user message.

- A pin changes only for a material role change, after the minimum of two user turns on the pin
  and within a budget of two optional switches in any 20 turns. A tool continuation never
  changes it.
- A pin is recorded only after a 2xx response whose stream completed.
- The table holds at most 1,024 conversations and drops one that has been idle for 10 minutes.
  Pins are not restored after a restart.

## Current selection and history

Each dispatch records events: `selected` before the request is sent, then `started`, then
`completed`, `rejected` or `cancelled`. Review adds `judge`, `critic` and `repair` events.

| Route | Purpose |
|---|---|
| `GET /api/smart-auto/current?worker=<id>` or `?session=<id>` | The worker's main model, its active stages, its state, `age_ms` and `stale`. `selection` is `null` before the worker's first event. Returns 400 without either parameter. |
| `GET /api/smart-auto/history?workflow=&worker=&since_seq=&limit=` | Events after `since_seq`, oldest first. `limit` defaults to 200 and is capped at 1,000. |
| `GET /api/smart-auto/decisions?limit=N` | The newest routes, newest first. `limit` defaults to 50 and is capped at 200. |

`workflow` and `worker` are the raw ids the caller sent. The hub hashes them before it matches
events. A current selection older than 10 minutes is `stale: true`. All three routes need a key
when hub auth is on, see [HTTP API](http-api.md#hub-authentication).

## Privacy and storage

An event holds salted scope keys and metadata: the scope keys, the selector, tier, role, stage,
model, state, reason, policy version, effort, status, latency and token counts. It never holds
prompt, response or tool text. The salt is random per install and is kept in the store, so a
stored key does not reveal the id it stands for.

- Events are saved to the store (`LLM_HUB_STORE`, SQLite by default) by a writer thread, so a
  request never waits for the disk. A full writer queue drops the save, not the in-memory event.
- The store keeps the newest 10,000 events and events from the last 14 days.
- On start the hub reloads the history. The current selections and the pins start empty.

Set `LLM_HUB_STORE_PATH` to choose the store file. See [Configuration](configuration.md).

## Limits of this version

- Ranking uses built-in suitability scores and the request counts of the last 30 days. It does
  not use measured latency or quality.
- The upstream does not echo the effort it applied, so the history has no confirmed effort.
- `/v1/responses` requests are not reviewed.
