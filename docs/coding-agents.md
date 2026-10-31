# Coding agents

Five commands start a coding agent (a harness) against the hub. The table of commands is in the
[README](../README.md#harness-commands).

## Shared rules

- **Model argument.** The first argument is the model, unless it is empty or starts with `-`.
  The command passes every other argument to the harness unchanged. Give the model before a
  harness subcommand: `llm-hub codex <model> exec ...`. Without a model, `exec` is the model.
- **Hub address.** `LLM_HUB_URL` and `LLM_HUB_API_KEY` set the hub and its key. Their defaults
  are in [Configuration](configuration.md#harness-command-variables).
- **Health check.** The command calls `GET /healthz` with a 2 s timeout. If the hub does not
  answer, it prints `llm-hub <cmd>: llm-hub is not reachable at <url>` and exits 1.
- **Process.** On Unix the command replaces itself with the harness, so the harness owns the
  terminal and its signals. On Windows it waits for the harness and exits with its code. No
  shell runs in between.
- **Environment.** The harness inherits your environment. Set any other harness variable
  yourself, for example `COPILOT_OFFLINE`.

## llm-hub claude

`llm-hub claude` sets:

| Variable | Value |
|---|---|
| `ANTHROPIC_BASE_URL` | the hub URL |
| `ANTHROPIC_AUTH_TOKEN` | the hub key |
| `ANTHROPIC_SERVER_NAME` | `llm-hub` |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | `1` |
| `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT` | `1` |
| `ENABLE_TOOL_SEARCH` | `true` |
| `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL` | the model, only when you give one |

It then runs `claude --settings <json> <args>`. The JSON comes from
`GET /api/claude-code/settings` (3 s timeout, sent with the hub key) and holds a `/model` picker
list of every hub model. When that call fails or returns nothing, the command leaves out
`--settings`.

The command never sets `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` or
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`. See
[Troubleshooting](troubleshooting.md#claude-code-compacts-the-conversation-early).

How the `/model` picker fills:

- Gateway discovery reads `GET /v1/models` and times out after 3 s. The hub answers from its
  model cache and refreshes the cache in the background, so discovery does not wait on upstreams.
- Discovery keeps only ids that contain `claude` or `anthropic`. The `--settings` list adds
  every other hub model, including `copilot/auto`.
- Discovery needs a non-empty credential, so the commands send the default key `llm-hub` even
  while hub auth is off. With [hub auth](http-api.md#hub-authentication) on, set
  `LLM_HUB_API_KEY` to a real hub key.
- A discovered model can merge into its built-in alias row, for example `opus`.
- The hub adds `[1m]` to the id of a model whose known window is 1M tokens or more, so Claude
  Code does not assume a 200k window.

## llm-hub codex

`llm-hub codex` sets `LLM_HUB_API_KEY` and runs:

```sh
codex -c 'model_providers.llm_hub={name="llm-hub",base_url="<hub>/v1",env_key="LLM_HUB_API_KEY",wire_api="responses"}' \
      -c 'model_provider="llm_hub"' \
      [-c model="<model>"] [-c model_context_window=N -c model_max_output_tokens=M] <args>
```

`model_context_window` and `model_max_output_tokens` appear only when `/v1/models` reports a
window for the model. `M` is `min(N/4, 32768)`. Values set with `-c` also apply to subcommands
such as `exec`. The command never changes `~/.codex/config.toml`.

## llm-hub opencode

`llm-hub opencode` sets `LLM_HUB_API_KEY` and `OPENCODE_CONFIG_CONTENT`. The config adds an
`llm-hub` provider (`@ai-sdk/openai-compatible`, base `<hub>/v1`,
`apiKey: "{env:LLM_HUB_API_KEY}"`). The provider lists every hub model with its context and
output limits, where the hub knows them. With a model, the config also sets
`"model": "llm-hub/<model>"`. The command passes only your arguments to `opencode`.

## llm-hub copilot-cli

`llm-hub copilot-cli` sets `COPILOT_PROVIDER_BASE_URL=<hub>/v1`, `COPILOT_PROVIDER_TYPE=openai`
and `COPILOT_PROVIDER_API_KEY=<key>`. With a model, it also sets `COPILOT_MODEL`. When the hub
knows the model window, it sets `COPILOT_PROVIDER_MAX_OUTPUT_TOKENS` and
`COPILOT_PROVIDER_MAX_PROMPT_TOKENS` (the window minus the output). The command passes only your
arguments to `copilot`.

## llm-hub pi

`llm-hub pi` writes a built-in extension to `llm-hub/pi-extension.ts` in a cache directory. The
directory is the first of these that is set:

1. `$XDG_CACHE_HOME`
2. `%LOCALAPPDATA%`, on Windows only
3. `~/.cache`

The extension reads `/v1/models` and registers an `llm-hub` provider. The command sets
`LLM_HUB_URL` and `LLM_HUB_API_KEY`, then runs
`pi -e <extension> [--model llm-hub/<model>] <args>`.

## A shell wrapper

A shell function can add your usual flags to every `llm-hub claude` run. This zsh or bash example keeps the
model first, so `llm-hub claude` still reads it as the model:

```sh
CLAUDE_SHARED_ARGS=(--permission-mode acceptEdits)

cLLMHub() {
    if [ -n "$1" ] && [ "${1#-}" = "$1" ]; then
        local model="$1"; shift
        llm-hub claude "$model" "${CLAUDE_SHARED_ARGS[@]}" "$@"
    else
        llm-hub claude "${CLAUDE_SHARED_ARGS[@]}" "$@"
    fi
}
```

`cLLMHub copilot/auto` starts Claude Code on Copilot Auto with the shared flags. `cLLMHub -c`
continues the last session on the default models.
