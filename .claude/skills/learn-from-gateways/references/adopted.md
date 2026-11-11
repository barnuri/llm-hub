# Adopted and Rejected Gateway Findings

Last run: 2026-11-11 (per-model price and quota visibility)

The `learn-from-gateways` skill appends to this file. Skip each item listed here in a new report.

## 2026-10-31 — LiteLLM `github_copilot` review

### Reviewed: llm-hub is better, no change

| Item | Decision |
|---|---|
| Copilot auth token refresh | llm-hub is better. Keep it. |
| `X-Initiator` rule | llm-hub is better. Keep it. |
| Copilot Auto routing | llm-hub is better. Keep it. |

### Adopted

| Id | Item | Notes |
|---|---|---|
| L1 | Translate Anthropic requests to the Responses API for models that support only `/responses` | |
| L2 | Keep a stable `item_id` on `/responses` streams | |
| L3 | Send the messages-proxy headers | Live test returned 200. `x-github-api-version` skipped. |
| L4 | Repair Anthropic-shaped Claude replies on `/chat/completions` | |
| L5 | Allow only known Copilot API hosts | |

### Rejected

None.

## 2026-11-01 — Smart Auto public design review

These are accepted design directions, not implemented runtime behavior.
Private inventories and measurements remain outside tracked references.

| Id | Decision | Reason |
|---|---|---|
| D1 | Name the mode Smart Auto. Recommend Balance by default. | Keep the purpose clear and preserve existing tier vocabulary. |
| D2 | Allow tier-bounded Fusion-style orchestration across providers. | Support planning, investigation, and inexpensive scoped implementation. |
| D3 | Preserve a stable main model and separate worker/role affinity. | Avoid unnecessary prompt-cache loss and cross-worker interference. |
| D4 | Apply hard model and capability restrictions to every participant. | Judges, repairs, and fallback must not evade caller restrictions. |
| D5 | Keep personal telemetry private, including aggregates. | Only public facts, generic decisions, and synthetic fixtures belong in git. |

Pending implementation recommendations: separate context/input/output provenance, normalize cache telemetry, add bounded stage execution, and integrate orch identity and resolved-model display.
This review shipped one runtime change: L6 below. The D1-D5 directions are not implemented yet.

| Id | Item | Notes |
|---|---|---|
| L6 | Parse Copilot's "exceeds the limit of M" rejection and seed measured limits for 19 Copilot models | Live probes returned the ceilings. Every catalog was wrong for several models. |

## 2026-11-11 — Per-model price and quota visibility (pending, not adopted)

Pending recommendations only. No user acceptance is implied.

| Id | Recommendation | Reason | Evidence |
|---|---|---|---|
| P1 | Key bare `model_prices` overrides by profile, or stop the bare fallback when two profiles name the same model | `pricing.rs:81` stores bare ids with no profile; `pricing.rs:110` returns the last profile's rate for every profile | source-verified |
| P2 | Read OpenRouter `input_cache_write_1h` (and a 1h cache-write rate in config) | Live list has it on 36 models; `listed_prices.rs:71-72` drops it | live-observed |
| P3 | Read OpenRouter `pricing.overrides[].min_prompt_tokens` as long-context tiers | Live `openai/gpt-6.1-terra-pro` has a 272000 tier; `listed_prices.rs:56-74` reads only top-level fields | live-observed |
| P4 | Use Copilot `/models` `billing.token_prices` (batch-scaled, `cache_read_price` or `cache_price`) as a fallback when the docs table lacks a model | `pricing.rs:572-581` never reads it; OpenCode reads it live | source-verified |
| P5 | Per-model budget or TPM/RPM visibility, only if a quota source reports it | LiteLLM budget table has `model_tpm_limit`/`model_rpm_limit`; llm-hub has none | source-verified |
