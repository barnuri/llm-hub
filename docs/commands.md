# Commands

`llm-hub help` prints this list in the terminal.

| Command | Effect |
|---|---|
| `llm-hub`, `llm-hub serve` | Read `.env` and the process environment, then start the server in the foreground. |
| `llm-hub help`, `--help`, `-h` | Print every command. |
| `llm-hub version`, `--version`, `-V` | Print `llm-hub v<version>`. |
| `llm-hub update` | Update the binary once. See [Updates](service-and-updates.md#updates). |
| `llm-hub service install` | Install and start the background service, or reload it. See [Service](service-and-updates.md#service). |
| `llm-hub service uninstall` | Remove the background service. |
| `llm-hub service status` | Show the service status. |
| `llm-hub copilot login [--profile NAME]` | Log in to GitHub for a native Copilot profile. See [Native auth](github-copilot.md#native-auth). |
| `llm-hub copilot logout [--profile NAME]` | Remove that login from the OS credential store. |
| `llm-hub claude [model] [args...]` | Start Claude Code against the hub. |
| `llm-hub codex [model] [args...]` | Start Codex CLI against the hub. |
| `llm-hub opencode [model] [args...]` | Start opencode against the hub. |
| `llm-hub copilot-cli [model] [args...]` | Start GitHub Copilot CLI against the hub. |
| `llm-hub pi [model] [args...]` | Start pi against the hub. |

The five harness commands are in [Coding agents](coding-agents.md).

Notes:

- An unknown command prints `llm-hub: unknown command '<name>'` and the help, then exits with
  code 2. It never starts a server.
- `--profile` must come right after the action: `llm-hub copilot login --profile copilot`.
- The Copilot CLI command is `copilot-cli`, because `llm-hub copilot` is the login command.

## Foreground run

Run `llm-hub` in the directory with `.env`. A foreground run listens on `127.0.0.1:8410`, unless
`.env` sets `LLM_HUB_BIND` or `LLM_HUB_PORT`. The [service](service-and-updates.md#service) uses
another address. The examples in these docs use the service address, `http://127.0.0.1:8888`.
