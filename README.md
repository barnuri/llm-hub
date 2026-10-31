# llm-hub

llm-hub is one local endpoint for all your LLM providers. It merges the model lists of any number
of OpenAI-compatible APIs into one `/v1/models` and routes each request by its prefix:
`groq/llama-3.3-70b-versatile` goes to the `groq` profile, `copilot/claude-opus-4-5` to Copilot.

![demo](docs/demo.gif)

## Quick start

1. Get the binary. Pick one way:

   ```sh
   # From source (binary at target/release/llm-hub)
   git clone https://github.com/barnuri/llm-hub && cd llm-hub
   cargo build --release

   # Install script, macOS and Linux
   curl -fsSL https://raw.githubusercontent.com/barnuri/llm-hub/master/scripts/install.sh | sh
   ```

   For release binaries and Windows, see [Install](docs/service-and-updates.md#install).

2. Write a `.env` with one profile in the directory the hub will run from:

   ```sh
   mkdir -p ~/llm-hub && cd ~/llm-hub
   printf 'LLM_HUB_GROQ_BASE_URL=https://api.groq.com/openai/v1\nLLM_HUB_GROQ_API_KEY=gsk_...\n' > .env
   ```

   For Copilot, local models and other upstreams, see [Providers](docs/providers.md).

3. Install the hub as a background service, from the same directory:

   ```sh
   llm-hub service install
   ```

4. Open the UI at <http://127.0.0.1:8888>. The **Models** tab lists every model the hub can
   route to. The **Setup** tab generates client snippets.

5. Start a coding agent against the hub:

   ```sh
   llm-hub claude                               # Claude Code, default models
   llm-hub claude copilot/claude-opus-4-5       # Claude Code, one model for every role
   ```

## Harness commands

| Command | Harness | API it uses | Example |
|---|---|---|---|
| `llm-hub claude` | Claude Code (`claude`) | Anthropic Messages, `/v1/messages` | `llm-hub claude copilot/auto` |
| `llm-hub codex` | Codex CLI (`codex`) | OpenAI Responses, `/v1/responses` | `llm-hub codex openai/gpt-5 exec "fix the tests"` |
| `llm-hub opencode` | opencode (`opencode`) | OpenAI chat completions | `llm-hub opencode groq/llama-3.3-70b-versatile` |
| `llm-hub copilot-cli` | GitHub Copilot CLI (`copilot`) | OpenAI chat completions | `llm-hub copilot-cli copilot/gpt-4.1` |
| `llm-hub pi` | pi (`pi`) | OpenAI chat completions | `llm-hub pi copilot/claude-sonnet-4-5` |

See [Coding agents](docs/coding-agents.md) for what each command sets.

## Documentation

- [Commands](docs/commands.md): every `llm-hub` command, including `help` and `serve`.
- [Providers](docs/providers.md): profiles, local models, managed commands and the copilot-api proxy.
- [GitHub Copilot](docs/github-copilot.md): native Copilot auth, model limits and the usage API.
- [Copilot Auto](docs/copilot-auto.md): how `copilot/auto` picks a model, and what hydra is.
- [Coding agents](docs/coding-agents.md): the five harness commands, their environment and a shell wrapper.
- [HTTP API](docs/http-api.md): routes, control headers and hub authentication.
- [Anthropic Messages](docs/anthropic-messages.md): the translated `/v1/messages` route and its limits.
- [Fallbacks and transforms](docs/fallbacks-and-transforms.md): fallback chains and request changes.
- [Usage and insights](docs/usage-and-insights.md): UI tabs, stats and model rankings.
- [Configuration](docs/configuration.md): every `LLM_HUB_*` variable.
- [Service and updates](docs/service-and-updates.md): install, the background service, Docker and updates.
- [Troubleshooting](docs/troubleshooting.md): common failures and their fixes.
- [Development](docs/development.md): build, test and reinstall from a checkout.
- [Subscriptions](docs/subscriptions.md): why llm-hub does not proxy Claude, ChatGPT or Cursor plans.

## License

MIT
