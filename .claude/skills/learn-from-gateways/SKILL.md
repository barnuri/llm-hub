---
name: learn-from-gateways
description: Research BerriAI/litellm and maximhq/bifrost for gateway behaviours that llm-hub lacks or does worse. Send the accepted gaps to /code-gen. Use when the user says "learn from litellm", "compare with bifrost", "what do other gateways do better", or "gateway gap research".
---

# Learn From Gateways

Compare llm-hub with two open-source gateways. Report the gaps. Send the accepted gaps to
`/code-gen`. Record each decision in `references/adopted.md`.

## Sources

| Repo | Language | License | Default branch |
|---|---|---|---|
| `BerriAI/litellm` | Python | NOASSERTION | `main` |
| `maximhq/bifrost` | Go | Apache-2.0 | `dev` |

LiteLLM has no clear license. Learn the behaviour from it. Do not copy its code. Never copy
code from a LiteLLM `enterprise/` directory. For Bifrost, write the behaviour in idiomatic Rust.
Do not port its Go code line by line.

## Adoption Filter: Performance First

llm-hub puts latency first (see the repo `CLAUDE.md`). Reject a gap when the fix does any of
these things:

- It adds a per-request network call, process spawn, or disk read on the request path.
- It buffers a stream that llm-hub now passes through.
- It translates a request that the upstream accepts in its native format.
- It adds a proxy hop in place of an in-process upstream.

A fix on the request path may add at most 50 ms to the time to first byte. Measure it against a
direct `curl` to the upstream.

## Steps

1. Pick one focus. Examples: a provider such as `github_copilot`, Anthropic passthrough, the
   Responses API, streaming, retries and fallbacks, caching, cost and quota, MCP. If the user
   gives no focus, use the upstream changes since the last run date in `references/adopted.md`.
2. Fetch the upstream source into the session scratchpad. Never write upstream files into the
   repo. Use these commands:

   ```sh
   # One file (use ref=dev for bifrost)
   ghx api "repos/BerriAI/litellm/contents/<path>?ref=main" --jq .content | base64 -d > "$SCRATCH/<name>"
   # Find the files for a focus
   ghx search code "<term>" --repo BerriAI/litellm
   ghx search code "<term>" --repo maximhq/bifrost
   # Changes since the last run
   ghx api "repos/<owner>/<repo>/commits?path=<path>&since=<YYYY-MM-DD>T00:00:00Z" --jq '.[] | "\(.sha[0:8]) \(.commit.message | split("\n")[0])"'
   ```

3. Read the matching llm-hub code in `src/services/` and `src/routes/`. For Copilot, read these
   files:
   - `src/routes/copilot.rs`
   - `src/routes/proxy.rs`
   - `src/services/copilot_auto.rs`
   - `src/services/copilot_auto_decisions.rs`
   - `src/services/copilot_headers.rs`
   - `src/services/copilot_login.rs`
   - `src/services/copilot_sessions.rs`
   - `src/services/copilot_usage.rs`
   - `src/services/upstream_auth.rs`
   - `src/services/github_token_resolver.rs`
   - `src/schemas/copilot_*.rs`

   For Anthropic translation, read `src/services/anthropic/` (`request.rs`, `response.rs`,
   `stream.rs`).
4. Report at most 8 findings. Rank them by user impact. Skip each item that `references/adopted.md`
   already lists. Give each finding these fields:
   - the upstream `file:line`
   - the llm-hub `file:line`, or "missing"
   - one line of recommendation
   - `verified` or `unverified`
5. Before you adopt a claim about upstream behaviour, test it live with one minimal, cheap
   request. Mark each claim that you did not test as `unverified`. Never send a large prompt to
   a paid model to test a limit. Estimate the size with `count_tokens` first.
6. Ask the user which findings to accept. For the accepted gaps, invoke `/code-gen` with the
   findings as the brief.
7. Append each adopted or rejected item to `references/adopted.md`. Add today's date and a
   one-line reason. Update the last run date.

## Known Facts

Do not test these again unless the user asks.

- Copilot `/v1/messages` accepted an Opus prompt of 840k tokens. Copilot `/models` reported
  `max_prompt_tokens` 200000 for that model. Do not trust `/models` limits as hard limits.
- Claude Code tool search needs `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` unset.
