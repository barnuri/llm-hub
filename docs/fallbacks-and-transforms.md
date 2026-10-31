# Fallbacks and transforms

## Fallbacks

Send an ordered chain. The hub tries the next model only after a connect error, a timeout, 408,
429 or 5xx. It never retries 400, 401, 403, 404 or 422, because those describe your request and
fail the same way on every upstream.

```sh
curl http://127.0.0.1:8888/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'X-LLM-Hub-Fallbacks: groq/llama-3.3-70b-versatile, openai/gpt-4o-mini' \
  -d '{"model":"openai/gpt-5","messages":[{"role":"user","content":"hi"}]}'
```

The response headers tell you what happened:

```
X-LLM-Hub-Model: groq/llama-3.3-70b-versatile
X-LLM-Hub-Attempts: openai/gpt-5=429, groq/llama-3.3-70b-versatile=200
```

- `X-LLM-Hub-Timeout-Ms` sets a deadline per attempt. `X-LLM-Hub-Retry-On: 429,503` replaces the
  set of statuses that trigger a retry.
- A fallback replays the request body, so the hub buffers it. A body over
  `LLM_HUB_MAX_REPLAY_BYTES` streams through with fallbacks off.
- The default chain applies when a request has no fallback header. The header always wins. Set
  the default chain in one of three ways. All three write `LLM_HUB_DEFAULT_FALLBACKS`.
  - **UI:** the **Profiles** tab has a "Default fallbacks" editor.
  - **API:** `POST /api/fallbacks` with
    `{"fallbacks":["groq/llama-3.3-70b-versatile","openai/gpt-4o-mini"]}`.
  - **Env:** `LLM_HUB_DEFAULT_FALLBACKS=groq/llama-3.3-70b-versatile,openai/gpt-4o-mini`.
- The **Setup** tab has a fallback picker that adds `X-LLM-Hub-Fallbacks` to each generated
  snippet.

## Request transforms

Two opt-in request headers change the body before it goes upstream, for clients that cannot.
Both work on `/v1/chat/completions` and on `/v1/messages`, because the hub translates the
Anthropic body first. Both run once, before the fallback chain, so every attempt sends the same
bytes.

| Header | Values | Effect |
|---|---|---|
| `X-LLM-Hub-System-Prompt-Mode` | `none`, `truncate`, `drop` | `truncate` cuts each `system` or `developer` message to 1000 characters (`system` on the Anthropic route). `drop` removes them, unless no messages would remain. |
| `X-LLM-Hub-Reasoning-Strip` | `true`, `1`, `yes`, `on` | Removes `reasoning`, `reasoning_effort` and `thinking`, for upstreams that answer 400 to them. |

An unknown value, or no header, leaves the request unchanged, with no error. The hub never
picks a transform from the model name or the profile. If it did, a transform could fire for the
primary model and not for a fallback, and `X-LLM-Hub-Attempts` would describe two different
requests. The `X-LLM-Hub-Transforms` response header lists the transforms that fired, for example
`system-prompt=truncate, tool-names=3, reasoning-strip`.

## Always-on changes

Three more changes are always on, because the clients they fix cannot ask for them:

- **First streamed role.** When the first streamed `choices[0].delta` has no `role`, the hub adds
  `role: "assistant"`. The OpenAI spec requires it, and some local servers leave it out. The hub
  never overwrites a role and touches only the first delta. Set `LLM_HUB_STREAM_ROLE=false` to
  turn it off.
- **Tool-name truncation.** OpenAI-compatible upstreams reject a function name over 64 bytes,
  and MCP tool names (`mcp__<server>__<tool>`) often exceed that. The hub replaces a longer name
  with `<first 55 chars>_<8 hex of its sha256>` and restores the original in the response: in
  `tool_calls[].function.name`, in `tool_use` blocks, and frame by frame in a stream. It
  rewrites `tools[]`, `tool_choice`, and `tool_calls[]` in earlier assistant messages too. The
  alias depends only on the name, so it is the same on every turn and every attempt. The hub
  stores nothing between requests. Shorter names and echoed aliases pass through unchanged.
- **SSE keepalive.** When a streamed response sends no bytes for `LLM_HUB_SSE_KEEPALIVE_MS`, the
  hub sends the SSE comment `: llm-hub keepalive`. A slow local model can stay silent for the
  whole prompt read, and clients with an idle timeout would close the connection. Parsers drop
  comment lines. Set `0` to turn it off.

The hub also has two Copilot CLI transforms, set by `LLM_HUB_COPILOT_GUARD_MODELS` and
`LLM_HUB_COPILOT_STRIP_SAMPLING_MODELS`. See
[Routing and streaming](configuration.md#routing-and-streaming).
