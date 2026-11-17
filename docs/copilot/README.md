# Use GitHub Copilot in Claude Code, with llm-hub

llm-hub is a small program that runs on your computer. It lets Claude Code use the models you
already have in GitHub Copilot.

Source code: [github.com/barnuri/llm-hub](https://github.com/barnuri/llm-hub).

## Why install it

1. Use your Copilot models in Claude Code, with one command.
2. See the price and a short description of every model, right in the `/model` picker.
3. Let Copilot Auto pick a model per prompt, or pick one yourself with `/model`.
4. Updates arrive on their own.
5. It is open source under the MIT license.

We recommend installing it. One command below.

You set it up once. After that, you start Claude Code with one command.

![Demo: Claude Code through llm-hub, from a prompt to the reply and the status line](images/demo-live.gif)

## What you need

1. A GitHub Copilot subscription.
2. Claude Code installed. Check it with `claude --version`.
3. A Mac or Linux computer. For Windows, see the [install guide](../service-and-updates.md#install).

## Install in one command

Paste this in a terminal. It installs everything, signs you in to GitHub, and writes the settings.
Run it again any time: it does only the steps that are not done yet.

```sh
curl -fsSL https://raw.githubusercontent.com/barnuri/llm-hub/master/scripts/install.sh | sh -s -- --copilot
```

The script:

1. Installs the GitHub CLI (`gh`) with Homebrew, apt, or dnf when it is missing.
2. Installs `llm-hub` when it is missing.
3. Creates `~/llm-hub/.env` with the Copilot settings. Existing values stay.
4. Runs `gh auth login` in the browser when you are not logged in. llm-hub reads the token from `gh`.
5. Checks the token against GitHub.

Flags (all optional):

| Flag | Effect |
|------|--------|
| `--dir PATH` | Settings folder (default `~/llm-hub`) |
| `--profile NAME` | Profile name (default `copilot`) |
| `--auth-flow github-copilot-vscode` | Use the VS Code flow instead |
| `--device-login` | Sign in with llm-hub's device code instead of `gh` |
| `--service` | Also run llm-hub in the background (`llm-hub service install`) |
| `--force-env` | Replace the managed `.env` values with the defaults |
| `--skip-install` | Do not install `gh` or `llm-hub` |
| `--skip-gh-login` | Do not start `gh auth login` |

To pass flags through the pipe: `... | sh -s -- --service --dir ~/llm-hub`.
Or save the file and run it: `sh scripts/install.sh --help`.

**Then start Claude Code on Copilot.** Keep llm-hub running in one terminal (run `llm-hub`
from the folder). In a second terminal, run:

```sh
llm-hub claude copilot/auto
```

Claude Code starts. Type your question as usual. The status line at the bottom shows the model,
the context used, and your Copilot quota. See the [Claude Code guide](claude-cli.md#start-a-session).

## What `copilot/auto` means

Copilot Auto picks a model for each thing you ask. A quick question can use a small, fast model.
A harder task can use a stronger one. You do not choose. See a live example in the
[integration guide](integration.md#copilot-auto-in-practice).

To pick a model yourself, type `/model` inside Claude Code. To list only your Copilot models, start
with `llm-hub --profiles copilot claude copilot/auto`. Each model shows its price and a one-line
description from GitHub Docs.

![Scrolling the /model picker through every Copilot model](images/claude-model-picker-scroll.gif)

All 24 entries in one list:

![The /model picker with every Copilot model and its description](images/claude-model-picker-all.png)

## See the price of each model

llm-hub shows the price per 1 million tokens for each model. Open the **Models** page and pick the
**copilot** tab, or open `http://localhost:8888/models?profile=copilot`.

![Models page filtered to Copilot, with price per model](images/hub-models-copilot.png)

## Good to know

- **Updates are automatic.** llm-hub checks for a new version when it starts and once a day.
  Set `LLM_HUB_AUTO_UPDATE=false` in `.env` to turn this off.
- **Keep the terminal open.** If llm-hub is not running, Claude Code stops with
  "llm-hub is not reachable". Run `llm-hub service install` to start it in the background.
- **Use it for normal work.** GitHub can block accounts that send heavy automated traffic through
  Copilot outside its own apps. Daily use is fine.

## Next

- [Integration](integration.md): how llm-hub connects to Copilot.
- [Claude Code guide](claude-cli.md): more about `llm-hub claude`.
- [GitHub Copilot reference](../github-copilot.md): every setting.
- [Troubleshooting](../troubleshooting.md): when something breaks.
