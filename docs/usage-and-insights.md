# Usage and insights

The UI tabs are **Overview**, **Insights**, **Errors**, **Models**, **Stats**, **Copilot Auto**,
**Profiles**, **API keys** and **Setup**. Live stats are in memory. Usage history, errors and
insights need `LLM_HUB_PERSISTENT=true`.

Each request records its client from the User-Agent (plus a marker for Copilot CLI's
`x-initiator` header). The hub classifies it as `claude-code`, `claude-code-headless`,
`claude-agent-sdk`, `copilot-cli`, `opencode`, `cursor`, and others.

## Insights

The **Insights** tab ranks models by what happened through the hub: success rate, current
failure streak, time to first token, output speed, prompt-read speed, cache hits and the most
common failure reasons.

- Overall score = 60% reliability + 40% speed. Speed is relative to the fastest ranked model.
- A model gets a rank once it has `min_requests` calls in the window (default 5) and its newest
  3 calls did not all fail.
- The scores measure speed and failed calls, not answer quality.

```sh
curl 'http://127.0.0.1:8888/api/insights?range=7d&min_requests=5'
curl 'http://127.0.0.1:8888/api/insights/pick?by=overall&profile=llama_swap'
```

- `range` is `1d`, `7d` (default), `30d` or `all`.
- `by` is `overall`, `stability`, `speed`, `ttft`, `decode` or `prefill`.
- `pick` returns the winner and the runners-up. It answers 404 when no model has a rank yet, so a
  script can fall back to a fixed model.
- `GET /api/insights/report` is the same data as one document for agents: a plain-language
  `summary`, every `pick`, `best_for` per use case, stats per harness, failure reasons and
  caveats.

## Model details

Each model also carries:

- **Context window** from the upstream model list (`max_input_tokens`, `context_length`, or
  llama-swap's `metadata.context_length`).
- **Use cases and summary** from llama-swap `metadata.use_cases` and `metadata.summary`.
  `best_for` picks the best-ranked model per use case, or the declared one when none has enough
  calls.
- **Harness stats.** `best_harness` is the harness with the highest success rate on that model,
  from at least 3 calls.

Prices for the cost estimate come from the `*_USD_PER_1M` and `MODEL_PRICES` variables. See
[Pricing](configuration.md#pricing).

The Copilot quota and Copilot Auto decisions have their own pages:
[Usage API](github-copilot.md#usage-api) and [Seeing the pick](copilot-auto.md#seeing-the-pick).
