# Gateway and Routing Sources

Reviewed 2026-11-01. These references describe public contracts or inspected source, not live inference guarantees.

## Gateways and catalogs

| Source | Role | Public reference |
|---|---|---|
| Bifrost | Gateway model schema, provider-aware limits | [Models schema](https://github.com/maximhq/bifrost/blob/46ca04029ce3d4381506f9db653aa47a1f818003/docs/openapi/schemas/inference/models.yaml#L15-L64) |
| LiteLLM | Local complexity routing and cache evidence | [Complexity router](https://github.com/BerriAI/litellm/blob/60fc121ce8ab995cb2997e0bbd0a980ee93c627d/litellm/router_strategy/complexity_router/README.md#L71-L106) |
| models.dev | Model-family catalog and provider overrides | [Limits](https://github.com/anomalyco/models.dev/blob/165c3753fe6d46226d53aba3ddcd5b56e0b56cbb/packages/core/src/schema-v2.ts#L321-L330), [provider overrides](https://github.com/anomalyco/models.dev/blob/165c3753fe6d46226d53aba3ddcd5b56e0b56cbb/packages/core/src/schema-v2.ts#L905-L930) |
| OpenCode | Client limit consumption and model filtering | [Provider configuration](https://opencode.ai/docs/providers/), [models.dev](https://models.dev/) |
| OpenRouter | Hosted API routing and cache contract | [Models API](https://openrouter.ai/docs/api-reference/models/get-models), [provider routing](https://openrouter.ai/docs/features/provider-routing), [prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching) |
| Portkey | Additional gateway routing comparison | [Gateway repository](https://github.com/Portkey-AI/gateway) |

Bifrost exposes optional context, input, output, architecture, supported parameters, and request-limit fields.
Its catalog lookup requires the concrete provider. Unknown models and individual fields can be absent.
Do not copy its fallback from context to input without preserving the distinction.

LiteLLM's local heuristic avoids classifier calls. Its optional cache-cost prediction can make token-count requests.
That prediction is unsuitable for llm-hub's local selection path.
Reuse its requirement for matching caller/deployment/model/prefix cache evidence instead.
Its advertised sub-millisecond classification is an upstream claim, not an llm-hub measurement.

models.dev separates lab models from provider overrides. Fields can be optional.
Community validation checks schema shape, not every serving limit.
OpenCode uses its catalog for client budgets. Picker filtering alone is not server-side enforcement.

OpenRouter separates model context, top-provider context/output, and per-request limits.
Its `only` provider allowlist differs from `order` preferences.
`require_parameters` is false by default. Ordering alone does not prohibit fallback.
Its session identifier supports provider affinity. That does not guarantee a cache hit.

Portkey's README lists fallback and conditional routing.
Its caching claims depend on hosted or enterprise edition. Verify the applicable implementation.

## Orchestrators and router libraries

| Source | Relevant behavior | Reference |
|---|---|---|
| Copilot Fusion | Single, Cascade, Critique, effort tiers, cache-aware main-model stability | [Official concepts](https://docs.github.com/en/copilot/concepts/models/copilot-fusion), [launch](https://github.blog/changelog/2026-10-28-route-and-orchestrate-models-with-github-copilot-fusion/) |
| NVIDIA NeMo Switchyard | Embeddable Rust routing, execution-stage signals, optional decision-model calls | [Repository](https://github.com/NVIDIA-NeMo/Switchyard) |
| RouteLLM | Strong/weak model selection and threshold calibration | [Repository](https://github.com/lm-sys/RouteLLM) |
| LLMRouter | Multiple router methods and replay benchmarks | [Repository](https://github.com/ulab-uiuc/LLMRouter) |
| ClawRouter | Classify, filter, rank, fallback pipeline | [Repository](https://github.com/BlockRunAI/ClawRouter) |

HydraFusion refers here to GitHub's Copilot Fusion work, not AICPS's sensor-fusion repository.
Fusion documentation is a behavior reference. It does not establish an open-source implementation.

Switchyard provides a Rust core. Stage routing can avoid classifier calls, while other algorithms request them.
Its beta API requires review before dependency adoption. No dependency is adopted by this reference list.

RouteLLM emphasizes pairwise strong/weak selection, not an agent orchestration engine.
LLMRouter provides broader methods. Replay benchmark results do not establish live quality for a new model pool.

ClawRouter documents capability filtering and model exclusions.
Its documented escape when exclusions remove a whole tier conflicts with strict llm-hub restrictions.
Do not adopt that escape.

TensorZero was archived at this review. Retain it only as historical design material, not a default maintained reference.
Check status again before relying on it.

## Research discipline

Re-read current public contracts before implementation. Pin inspected source revisions.
Label documentation, source inspection, live observation, and inference separately.
Do not include local inventories, account limits, measured usage, or private orch profiles in this file.

## Source study 2026-11-01

Pinned revisions: LiteLLM `60fc121c`, Bifrost `46ca0402`, models.dev `165c3753`, OpenCode `97a71f0c`.

| Topic | LiteLLM | Bifrost | OpenCode / models.dev | OpenRouter (docs) |
|---|---|---|---|---|
| Limit source | Static JSON; `github_copilot/` rows from GitHub's published table | Remote datasheet, 24h refresh; provider value wins | OpenCode reads Copilot `/models` live: `context = max_context_window_tokens ?? max_prompt_tokens`, `input = max_prompt_tokens` | `context_length`, `top_provider`, `per_request_limits` |
| Context check before send | `_pre_call_checks` drops a deployment only when the limit is known; unknown fails open | None; only clamps output and thinking budget | Client-side compaction at window − max(15%, 16k) | None documented |
| Effort | Rounds up to the next declared level; adaptive models get `output_config.effort` | Snaps to the nearest published level, ties up; effort↔budget ratios | Variants from Copilot `supports.reasoning_effort` | Unified `reasoning.effort` mapped to budget ratios |
| Sticky routing | Session pin, 1h TTL, dropped on failure; prefix-hash cache pin 5m/1h | Session binding written on success, rebound on fallback | n/a | `session_id`, then `prompt_cache_key`, then first-messages hash; 10 min idle; not updated on error |
| Model filters | Allow list only, globs; fallbacks rechecked | Allow and deny lists, `*` and `regex:`; rechecked per fallback attempt | whitelist then blacklist | `allowed_models` / `excluded_models`, exclusion wins |

Live measurement on 2026-11-01 contradicted every catalog for Copilot limits. The table above describes how each tool reads limits, not which values are right.

Adopt for llm-hub: measured or learned limits over any catalog; fail-closed hard filters on every participant and fallback; deny wins over allow; success-only affinity writes; effort snapping to the model's published levels.

Reject for llm-hub: request-time token-count calls, LLM or embedding classifiers, Redis or KV-backed pins, semantic caching, fallback on every error (breaks cache affinity), and ClawRouter-style relaxation of exclusions.

Other gateways checked: vLLM semantic-router (category router with per-category reasoning switch), Portkey conditional router, Agent Router (formerly Envoy AI Gateway). Helicone ai-gateway is GPL-3.0 and stale.
