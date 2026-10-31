# Anthropic Messages

`POST /v1/messages` accepts the Anthropic Messages API and sends OpenAI chat completions to the
profile you route to. An Anthropic SDK can then use any upstream the hub knows. Native Copilot
Claude models and Copilot Responses-only models skip this translation. See
[Models and endpoints](github-copilot.md#models-and-endpoints).

```sh
curl http://127.0.0.1:8888/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"groq/llama-3.3-70b-versatile","max_tokens":256,
       "system":"be terse","messages":[{"role":"user","content":"hi"}]}'
```

- The model id is `<profile>/<model>`. Every [control header](http-api.md#request-and-response-headers)
  works here as on `/v1/chat/completions`.
- The response `model` field names the model that served the request, so a fallback shows.
- Long tool names get an alias upstream and their own name back in the response. See
  [Request transforms](fallbacks-and-transforms.md#request-transforms).

## Streaming

`"stream": true` works. The hub translates the upstream OpenAI stream frame by frame into the
Anthropic events (`message_start`, `content_block_start` / `_delta` / `_stop` for text and for
`tool_use` with `input_json_delta`, `message_delta`, `message_stop`). It buffers only the frame
it is building, and it ends the stream in a valid state even when the upstream stops early. Two
differences from the Anthropic API:

- Content blocks come one at a time. OpenAI can interleave `tool_calls` indices and the
  Anthropic protocol cannot, so a new block closes the previous one.
- `message_start` reports zero tokens and `message_delta` carries the real counts. OpenAI sends
  usage in a chunk after the one with `finish_reason`, so the hub sends the last events on
  `[DONE]` or at the end of the stream.

## Limits

These limits apply to the translated route only.

- `POST /v1/messages/count_tokens` works only for native Copilot Claude models.
- The hub drops content that OpenAI chat completions cannot represent: `thinking` and
  `redacted_thinking` blocks, the `thinking` parameter, `top_k`, and Anthropic server tools
  (`web_search_*`, `computer_*`). An image block whose `source.type` is not `base64` or `url`
  gets a 400.
- `stop_reason` is never `stop_sequence`, and `stop_sequence` is always `null`. The OpenAI
  format does not report which stop string matched.
- A message with a `tool_use` block always reports `stop_reason: "tool_use"`, even when the
  upstream sent `finish_reason: "stop"` with its tool calls. llama.cpp, vLLM in some configs,
  and several gateways do that. Anthropic clients run their agent loop from that field.
- A request body over `LLM_HUB_MAX_REPLAY_BYTES` gets a 400, because translation needs the whole
  body.
