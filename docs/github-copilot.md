# GitHub Copilot

A native Copilot profile lets the hub talk to Copilot itself, with no proxy in between. Its models
route as `<profile>/<model>`, for example `copilot/claude-opus-4-5`. For the proxy way, see
[Through copilot-api](providers.md#through-copilot-api).

Warning: heavy automated use of Copilot outside its own clients can trigger GitHub's abuse
detection and suspend your Copilot access. This applies to native auth and to copilot-api. The
copilot-api README gives the same warning.

## Native auth

Set `AUTH` on a profile and the hub talks to Copilot itself. `BASE_URL` stays required. The hub
uses it only when Copilot reports no API endpoint of its own. Do not set `API_KEY` on a profile
with `AUTH`: the hub rejects that config. Every native Copilot variable is in
[Configuration](configuration.md#per-profile-native-copilot).

### Flows

| `AUTH` | Flow | GitHub token |
|---|---|---|
| `github-copilot` | The GitHub token is the bearer for every Copilot request (the opencode flow). The hub reads the API base and the entitlement from `copilot_internal/user`. | `gh`, else `llm-hub copilot login` |
| `github-copilot-vscode` | The hub exchanges the GitHub token for a short-lived Copilot token at `copilot_internal/v2/token` and refreshes it before it expires. It sends the VS Code `Editor-*` headers and the `vscode-chat` integration id (the copilot-api flow). | `llm-hub copilot login` only |

`GITHUB_TOKEN_SOURCE` selects where the GitHub token comes from:

- `auto` uses `gh auth token` when `gh` is installed and logged in. Otherwise it uses the
  `llm-hub copilot login` token. A missing `gh` is not an error.
- `gh` uses only `gh`.
- `login` uses only the `llm-hub copilot login` token. It is the only value
  `github-copilot-vscode` accepts, because the token exchange rejects `gh` tokens with a 403.

When a Copilot request gets a 401, the hub drops its cached token, gets a new one, and retries
the request once.

### With gh

After `gh auth login`, this is the whole profile:

```sh
LLM_HUB_COPILOT_BASE_URL=https://api.githubcopilot.com
LLM_HUB_COPILOT_AUTH=github-copilot
```

- Under a service, set `GH_PATH` to the absolute path of `gh`
  (`LLM_HUB_COPILOT_GH_PATH=/opt/homebrew/bin/gh`). See
  [Service PATH](service-and-updates.md#service-path).
- `GH_USER` selects one of several logged-in accounts (`gh auth token --user <login>`).
- If `GH_TOKEN` (github.com) or `GH_ENTERPRISE_TOKEN` (other hosts) is set in the hub process
  environment, the hub uses it and does not run `gh`. Set those in the service environment only.
  Never put them in `.env`.

### With llm-hub copilot login

Log in once with the GitHub device flow, from the directory with `.env`:

```sh
cd ~/llm-hub
llm-hub copilot login --profile copilot
```

- The command reads `.env` from the current directory to find the profile.
- You can leave out `--profile` when only one profile has `AUTH`.
- The command prints a code and a URL. After you approve, it checks your Copilot entitlement and
  stores the token in the OS credential store: service `llm-hub`, account `<AUTH>:<domain>` (for
  example `github-copilot:github.com`). It never writes the token to a file or to `.env`.
- `llm-hub copilot logout --profile copilot` removes the token.
- Profiles with the same `AUTH` value and domain share one login. The two `AUTH` values use
  different OAuth client ids, so each needs its own login.

A `github-copilot-vscode` profile needs its own name and its own login:

```sh
LLM_HUB_COPILOT_VSCODE_BASE_URL=https://api.githubcopilot.com
LLM_HUB_COPILOT_VSCODE_AUTH=github-copilot-vscode
```

Run `llm-hub copilot login --profile copilot_vscode` once. Its models route as
`copilot_vscode/<model>`.

### Credential store per OS

- **macOS:** the login Keychain. The LaunchAgent runs as the logged-in user, so it can read the
  item. On first access, macOS can ask whether `llm-hub` may use the item. Choose
  **Always Allow**. Requests to the profile fail until you answer. A rebuilt or updated binary
  is likely to ask again (not yet verified).
- **Linux:** a Secret Service provider, for example gnome-keyring, must run in the user session.
  On a host without one, use `gh`.
- **Windows:** Credential Manager.

### GitHub Enterprise

`GITHUB_DOMAIN` sets the GitHub host for the device login, for `gh auth token --hostname`, and
for the credential account (`LLM_HUB_COPILOT_GITHUB_DOMAIN=acme.ghe.com`). The REST base is then
`https://api.<domain>`. Set `GITHUB_API_URL` when it differs, for example on GitHub Enterprise
Server.

### Headers

`github-copilot` sends `Authorization`, `User-Agent`, `Openai-Intent`, `X-Initiator`, and
`Copilot-Vision-Request` when the request has an image. It leaves out `Copilot-Integration-Id`
and `X-GitHub-Api-Version` on purpose. With a `gh` token, Copilot answers 400 "IDE
authentication failed: IDE token is malformed" when either header is present.

`X-Initiator` is `agent` for tool-loop follow-ups and `user` for a new user turn, so follow-ups
do not use premium requests. A `HEADERS` entry with the same name overrides the hub's value.

## Models and endpoints

- The hub drops models that Copilot hides (`model_picker_enabled: false`) or that your policy
  disables.
- Copilot names Claude models with dots (`claude-opus-4.5`). Claude Code does not recognise
  them. A native profile lists them with dashes (`copilot/claude-opus-4-5`) and sends the dotted
  id upstream. Dotted ids still work in requests.
- `/v1/messages` for a Copilot Claude model goes to Copilot's own `/v1/messages` without
  translation, so thinking blocks and tool use pass through as sent.
  `/v1/messages/count_tokens` for those models goes to Copilot's `/v1/messages/count_tokens`.
  The [limits of the translated route](anthropic-messages.md#limits) do not apply to them.
- Copilot serves some models only through the Responses API, for example gpt-5 and later,
  except gpt-5-mini. They work on `/v1/responses`. On `/v1/messages`, the hub bridges the request
  to Copilot's `/responses` and translates the reply back to Anthropic events. This is the
  Responses bridge. These models do not work on `/v1/chat/completions`.
- [Copilot Auto](copilot-auto.md) adds the model `<profile>/auto`.

## Limits learned from rejections

Copilot's `/models` understates the prompt limit of Claude models on `/v1/messages`. The hub
ships limits measured on 2026-10-31 and uses them in `/v1/models`.

1. Copilot rejects a prompt that is too large with `model_max_prompt_tokens_exceeded`.
2. The hub reads the real limit from the error ("N tokens > M maximum") and uses it from then on.
3. With `LLM_HUB_PERSISTENT=true`, the store keeps learned limits across restarts.

A seeded limit never replaces a learned one. The hub never sends probe requests.

## Rejected-field stripping

Copilot's `/v1/messages` rejects some Anthropic request fields with
`<path>: Extra inputs are not permitted`. Examples are `context_management` and
`messages.<N>.output_config`.

1. A 400 with that message names the rejected fields.
2. The hub learns each field for that profile and retries the request once without it.
3. Later requests to the profile never send the field.
4. With `LLM_HUB_PERSISTENT=true`, the store keeps the learned fields across restarts.

The hub learns two shapes of path: a top-level key, and a key on every message
(`messages.<N>.<key>`). It ignores other shapes.

## Usage API

`GET /api/copilot/usage?profile=<name>` returns the Copilot quota of a native profile.

- Without `profile`, it uses the first enabled Copilot profile.
- The data comes from `copilot_internal/user`, in the quota shape the Claude Code status line
  reads.
- The hub caches it for 1 minute.
- It answers 404 when no Copilot profile matches, and 503 when Copilot does not answer. The hub
  log has the reason.
