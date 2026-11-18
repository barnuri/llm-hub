# Adopted and Rejected Gateway Findings

Last run: 2026-11-18 (extension model)

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

## 2026-11-18 — Extension model (adopted, implemented in PR #5)

Pinned revisions: Bifrost `2ddd8d37`, LiteLLM `e21416aa`, Envoy AI Gateway `f99b015f`, Kong `1c7a0aa7`, Pingora `3f87e92c`, River `0b09036f`, Portkey `669825cb`, Noveum `8962fc2e`, APISIX `9b4e4ca2`, Plano `b13ddd63`.
Design and reference: `docs/extensions.md`. The user approved rollout steps 1-4 on 2026-11-18.

| Id | Decision | Status | Reason | Evidence |
|---|---|---|---|---|
| X1 | One `Extension` trait with fixed phases | adopted, partial: `request`, `attempt`, `response`, `complete` on `/v1/*`. Every-route phases are pending | Bifrost transport hooks see only inference routes; plugins cannot add routes | source-verified |
| X2 | No silent skip of an extension | adopted, changed: no `phases()` method. Rust calls every trait method, and each phase method has a no-op default | Bifrost matches Go plugins by interface; a plugin missing a new method stops running silently. A Rust trait object cannot skip a method | source-verified |
| X3 | Declarative rules file compiled into the same trait | adopted: YAML or JSON, `deny_unknown_fields`, load-time phase checks, reload on change, `llm-hub extensions check` | River compiles config into Pingora phase modules; Envoy AI Gateway body mutations stay small | source-verified |
| X4 | HTTP hooks with timeout and one fail open/closed setting, pooled client, no per-request process | pending | LiteLLM generic guardrail API; ext_proc costs 2-5 ms per call; Copilot CLI's split crash/timeout/HTTP failure modes are the mistake to avoid | documented |
| X5 | `lib.rs` plus thin `main.rs`, `Hub` builder | adopted: `extension`, `routes` (under `/ext`, behind the master key), `router` (any tower layer) | Bifrost core, Vector, Pingora embed this way; avoids Go .so-style ABI limits | source-verified |
| X6 | Near-zero cost when nothing is configured | adopted: bodies parse only when a rule or extension reads them; error bodies read only on `wants_error_body`. Measured +0.05 ms median TTFB. Per-chunk phase pending | LiteLLM streaming fast path; opencode `has()` check | source-verified, live-observed |
| X7 | Defer WebAssembly | adopted (deferred) | Bifrost deprecated its WASM plugins; Extism round trip is tens to hundreds of µs | documented |
| X8 | The hub's stats, limit learning and retries read the upstream reply as it came; response changes apply only to the client | adopted | A body rewrite before the core broke Copilot limit learning in testing | live-observed |
| X9 | `mode: off|shadow|enforce` per rule, `closed` failure only in enforce | pending | Noveum and Portkey separate verdict, mode and failure policy, so a shadow rollout cannot cause an outage | source-verified |
| X10 | No per-chunk HTTP hooks; a chunk phase only for in-process Rust | pending | Plano makes one round trip per chunk per filter; pi notes awaited stream handlers stall the stream | source-verified |
| X11 | Provider-shaped error envelopes on reject, no invented statuses | adopted: `AppError::Rejected` renders in the client's API format | Portkey uses non-standard 246/446 statuses | source-verified |
