# Adopted and Rejected Gateway Findings

Last run: 2026-11-01 (public routing, catalog, and orchestration research)

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
