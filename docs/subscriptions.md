# Subscription plans as providers — decided against

**Decision: llm-hub does not proxy Claude Pro/Max, ChatGPT Plus/Pro, or Cursor Pro
subscriptions. It routes API-key providers only.**

## The question

Every profile is `base_url` + `api_key` HTTP passthrough (`src/configs/profile_config.rs`,
`src/services/upstream.rs`). Subscriptions expose no OpenAI-compatible endpoint, so
supporting them would need a second profile kind.

Two ways it could have worked:

1. **CLI subprocess adapter.** All three vendors ship a headless JSON mode that reuses the
   session from your normal login — `claude -p --output-format stream-json`,
   `codex exec --json`, `cursor-agent -p --output-format stream-json`. The hub would spawn
   the binary per request, flatten `messages` into one prompt, and map stdout JSONL events
   to OpenAI SSE deltas.
2. **OAuth passthrough.** Lift the token out of the vendor's credential store and call the
   private API directly. Reverse-engineers an undocumented auth flow; breaks on every
   rotation.

## Why neither

- **They are agents, not models.** Each injects its own system prompt, uses tools, and reads
  the filesystem. You get an agent's answer, not a completion. `temperature`, `logprobs`,
  `n`, tool-call schemas, and exact token counts do not survive the round trip.
- **Code execution.** `codex exec` and `cursor-agent` run shell commands. A proxied HTTP
  request becomes local execution on the host.
- **Terms of service.** These plans are licensed for use inside the vendor's own products.
  Putting one behind a general API endpoint is a violation, and sharing that endpoint past
  a single user is unambiguously one.
- **Wrong cost profile.** Process spawn per request, seconds of startup, no real concurrency.

## What to do instead

Keep subscriptions in their native clients, and point those clients at llm-hub for the
API-key providers. The **Setup** tab generates the snippets for Claude Code, Codex CLI, and
Cursor.
