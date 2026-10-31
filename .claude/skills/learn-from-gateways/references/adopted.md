# Adopted and Rejected Gateway Findings

Last run: 2026-10-31 (LiteLLM, focus `github_copilot`)

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
