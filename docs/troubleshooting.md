# Troubleshooting

## llm-hub claude says the hub is not reachable

The command checks `<LLM_HUB_URL>/healthz`. Check `llm-hub service status`, or set `LLM_HUB_URL`
to the address of a [foreground run](commands.md#foreground-run) (`http://127.0.0.1:8410`).

## Claude Code compacts the conversation early

Without tool search, Claude Code sends the full definition of every tool, MCP tools included, in
each request. Those definitions fill the context window and trigger autocompact.

- `ENABLE_TOOL_SEARCH=true` loads tool definitions on demand. `llm-hub claude` sets it.
- Do not set `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`. It turns tool search off and brings the
  problem back.

## Long sessions get a 400 on /v1/messages, or lose their fallbacks

The body is over `LLM_HUB_MAX_REPLAY_BYTES`. Older versions defaulted to 2 MB, and a long Claude
Code session exceeds that. Remove a small `LLM_HUB_MAX_REPLAY_BYTES` from `.env`, or raise it.
The current default is in [Configuration](configuration.md#routing-and-streaming).

## A 400 says a prompt is over the known limit, but the session is under it

The hub estimates a prompt at one token per four request bytes. That overstates agent bodies by
about 1.46x, so the hub refuses a request locally only when the estimate is over the limit by
150% (`PROMPT_REFUSAL_MARGIN_PERCENT`). A request inside that margin goes to the upstream, which
returns its own 400 when the prompt is really too long. The hub learns the limit from that 400.

## A Copilot Claude model reports a smaller window than it accepts

Copilot's `/models` understates the prompt limit of Claude models. The hub replaces it with a
measured or learned limit. See
[Limits learned from rejections](github-copilot.md#limits-learned-from-rejections).

## A Copilot profile fails under the service but works in the foreground

The service has a minimal `PATH`. See [Service PATH](service-and-updates.md#service-path). On
macOS, also check for a Keychain prompt. See
[Credential store per OS](github-copilot.md#credential-store-per-os).

## A gpt-5 Copilot model fails on /v1/chat/completions

Copilot serves it only through the Responses API. Use `/v1/responses` (`llm-hub codex`) or
`/v1/messages` (`llm-hub claude`). See [Models and endpoints](github-copilot.md#models-and-endpoints).

## The UI shows no usage, errors or insights

Set `LLM_HUB_PERSISTENT=true`.

## An unknown command prints the help

`llm-hub` has a fixed set of commands. See [Commands](commands.md). An unknown command exits
with code 2 and never starts a server.
