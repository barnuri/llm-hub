# HTTP API

The hub speaks OpenAI chat completions, OpenAI Responses and Anthropic Messages on one port. Any
OpenAI-compatible or Anthropic client works:

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8888/v1", api_key="unused")
client.chat.completions.create(model="groq/llama-3.3-70b-versatile", messages=[...])
```

## Routes

Routes marked "key" need a key only when [hub auth](#hub-authentication) is on.

| Method | Path | Purpose | Auth |
|---|---|---|---|
| GET | `/healthz` | Liveness. Returns `ok`. | none |
| GET | `/v1/models` | Merged model list, `<profile>/<model>` ids. | key |
| GET | `/v1/models/{id}` | One model. | key |
| POST | `/v1/chat/completions` | OpenAI chat completions, routed by model prefix. | key |
| POST | `/v1/responses` | OpenAI Responses, routed by model prefix. | key |
| POST | `/v1/messages` | Anthropic Messages. See [Anthropic Messages](anthropic-messages.md). | key |
| POST | `/v1/messages/count_tokens` | Token count. Native Copilot Claude models only. | key |
| any | `/v1/*` (other paths) | Forwarded to the profile the body's `model` names, for example `/v1/audio/transcriptions`. | key |
| GET | `/api/profiles` | Profiles, with `managed` and `running` flags. | key |
| POST | `/api/profiles` | Create or update a profile in `.env`. | key |
| DELETE | `/api/profiles/{name}` | Delete a profile from `.env`. | key |
| POST | `/api/profiles/{name}/test` | Test a profile's upstream. | key |
| POST | `/api/fallbacks` | Set the default fallback chain. See [Fallbacks](fallbacks-and-transforms.md#fallbacks). | key |
| GET | `/api/claude-code/settings` | `claude --settings` JSON with a picker entry for every hub model. | key |
| GET | `/api/copilot/usage` | Copilot quota. See [Usage API](github-copilot.md#usage-api). | key |
| GET | `/api/copilot/usage/history` | Stored quota readings and the daily burn. Query: `range` (`7d` default, `30d`). Needs the sqlite store. See [Usage API](github-copilot.md#usage-api). | key |
| GET | `/api/copilot/auto/decisions` | Recent Copilot Auto decisions. See [Seeing the pick](copilot-auto.md#seeing-the-pick). | key |
| GET | `/api/smart-auto/current` | A worker's current Smart Auto selection. Query: `worker` or `session`. See [Smart Auto](smart-auto.md#current-selection-and-history). | key |
| GET | `/api/smart-auto/history` | Smart Auto selection events after `since_seq`. Query: `workflow`, `worker`, `since_seq`, `limit`. See [Smart Auto](smart-auto.md#current-selection-and-history). | key |
| GET | `/api/smart-auto/decisions` | Recent Smart Auto routes. Query: `limit`. See [Smart Auto](smart-auto.md#current-selection-and-history). | key |
| GET | `/api/stats` | Request stats. Query: `range` (`live` default, `1d`, `7d`, `30d`, `all`), `profile`, `model`. | key |
| GET | `/api/usage` | Usage history with costs. Needs persistence. | key |
| GET | `/api/live` | Server-sent events: one `data:` line per finished request, as a usage row, with a `: keep-alive` comment every 15 s. Keep-warm pings are skipped. | key |
| GET | `/api/errors` | Failed requests. Query: `range` (default `7d`). Needs persistence. | key |
| GET | `/api/insights` | Model rankings. See [Insights](usage-and-insights.md#insights). | key |
| GET | `/api/insights/pick` | Best model. See [Insights](usage-and-insights.md#insights). | key |
| GET | `/api/insights/report` | Insights as one document for agents. | key |
| GET | `/api/keys` | List hub API keys. | key |
| POST | `/api/keys` | Create a hub API key. Returns it once. | key |
| DELETE | `/api/keys/{name}` | Delete a hub API key. | key |
| POST | `/api/restart` | Restart the hub. A service manager restarts a service. A foreground run starts itself again. | key |
| GET | any other path | The web UI. | none |

`POST /api/profiles`, `DELETE /api/profiles/{name}` and `POST /api/fallbacks` fail while
`LLM_HUB_CONFIG_READONLY=true`.

## Request and response headers

| Header | Direction | Purpose |
|---|---|---|
| `X-LLM-Hub-Fallbacks` (alias `X-Fallbacks`) | request | Comma-separated fallback chain. See [Fallbacks](fallbacks-and-transforms.md#fallbacks). |
| `X-LLM-Hub-Timeout-Ms` | request | Deadline per attempt. |
| `X-LLM-Hub-Retry-On` | request | Statuses that trigger a retry, for example `429,503`. |
| `X-LLM-Hub-System-Prompt-Mode` | request | See [Request transforms](fallbacks-and-transforms.md#request-transforms). |
| `X-LLM-Hub-Reasoning-Strip` | request | See [Request transforms](fallbacks-and-transforms.md#request-transforms). |
| `X-LLM-Hub-Model` | response | The model that served the request. |
| `X-LLM-Hub-Attempts` | response | Each attempt with its status. |
| `X-LLM-Hub-Transforms` | response | The transforms that fired. Absent when none fired. |
| `X-LLM-Hub-Auto-Pick`, `X-LLM-Hub-Auto-Reason` | response | See [Seeing the pick](copilot-auto.md#seeing-the-pick). |
| `X-LLM-Hub-Smart-Auto-*` | request and response | Smart Auto policy, worker identity and the route. Stripped before the upstream. See [Smart Auto](smart-auto.md#request-headers). |

## Hub authentication

Hub auth is off until you set `LLM_HUB_MASTER_KEY` or create a hub API key in the **API keys**
tab. Hub API keys need `LLM_HUB_PERSISTENT=true`. The hub shows a new key once and stores only
its hash.

- While hub auth is off, the hub ignores the client key and sends each profile's own key
  upstream.
- When auth is on, every `/v1/*` and `/api/*` route needs the master key or an enabled hub key,
  as `Authorization: Bearer <key>` or `x-api-key: <key>`.
- `/healthz` and the UI files never need a key.
