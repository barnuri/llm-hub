# Configuration

The hub reads `.env` in its working directory and the process environment. Process environment
wins. The hub reads `.env` again within about 1 s of a change.

Boolean flags accept `1`, `true`, `yes`, `on` and `0`, `false`, `no`, `off`.

## Server

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_BIND` | `127.0.0.1` | Listen address. The service sets `0.0.0.0`. |
| `LLM_HUB_PORT` | `8410` | Listen port. The service sets `8888`. |
| `LLM_HUB_MASTER_KEY` | unset | Key required on `/v1/*` and `/api/*`. Unset with no hub keys means no auth. See [Hub authentication](http-api.md#hub-authentication). |
| `LLM_HUB_CONFIG_READONLY` | `false` | Block UI and API writes to `.env`. |
| `LLM_HUB_AUTO_UPDATE` | `true` | Install updates on startup and once a day. `false` only logs a notice. See [Updates](service-and-updates.md#updates). |
| `LLM_HUB_PROFILES` | found from `*_BASE_URL` | Comma-separated profile names. Sets order and subset, and keeps dashed names. |

## Routing and streaming

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_DEFAULT_FALLBACKS` | empty | Fallback chain for requests without the fallback header. |
| `LLM_HUB_MAX_REPLAY_BYTES` | `268435456` (256 MB) | Largest request body the hub buffers for fallback replay and Anthropic translation. |
| `LLM_HUB_STREAM_ROLE` | `true` | Add `role: "assistant"` to the first streamed delta when the upstream leaves it out. |
| `LLM_HUB_SSE_KEEPALIVE_MS` | `15000` | Idle time before an SSE keepalive comment. `0` turns it off. |
| `LLM_HUB_COPILOT_PRICES_REFRESH_SECS` | `86400` (24 hours) | How often the hub fetches Copilot model prices from GitHub Docs while a Copilot profile is enabled. Fetches stay at least 1 hour apart. `0` turns the fetch off and the built-in prices apply. Editable on the Models page. |
| `LLM_HUB_COPILOT_GUARD_MODELS` | `*` | Models (`<model>`, `<profile>/<model>` or `*`) whose Copilot CLI requests get the tool-loop guard. The guard adds a system-prompt note. It also withholds a tool that the history shows called the same way 3 times in a row. Empty turns it off. |
| `LLM_HUB_COPILOT_STRIP_SAMPLING_MODELS` | 10 local model ids (`DEFAULT_COPILOT_STRIP_SAMPLING_MODELS` in `src/services/transforms.rs`) | Models whose Copilot CLI requests lose Copilot's fixed `temperature`, `top_p` and penalties, so the model's own sampling applies. Empty turns it off. |

## Persistence

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_PERSISTENT` | `false` | Keep usage history, errors, hub API keys, learned Copilot limits and learned rejected fields. |
| `LLM_HUB_STORE` | `sqlite` | `sqlite` keeps every request. `json` keeps a recent window. |
| `LLM_HUB_STORE_PATH` | `llm-hub.db` (sqlite), `llm-hub-stats.json` (json) | Store file. |

## Pricing

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_INPUT_USD_PER_1M` | `0` | Input price for profiles without their own. |
| `LLM_HUB_OUTPUT_USD_PER_1M` | `0` | Output price for profiles without their own. |
| `LLM_HUB_CACHE_READ_USD_PER_1M` | 10% of input | Cache-hit price for profiles without their own. |
| `LLM_HUB_CACHE_WRITE_USD_PER_1M` | the input price | Cache-write price for profiles without their own. |

## Per profile

`<NAME>` is the profile name in upper case, with `-` written as `_`.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_<NAME>_BASE_URL` | required | Upstream base URL. Must start with `http://` or `https://`. |
| `LLM_HUB_<NAME>_API_KEY` | empty | Upstream key the hub sends. Not allowed with `AUTH`. |
| `LLM_HUB_<NAME>_DISPLAY_NAME` | the name | UI label. The routing id stays the name. |
| `LLM_HUB_<NAME>_HEADERS` | none | Extra headers, as a JSON object of strings. |
| `LLM_HUB_<NAME>_TIMEOUT_MS` | none | Deadline per attempt for this profile. |
| `LLM_HUB_<NAME>_ENABLED` | `true` | `false` turns the profile off without deleting it. |
| `LLM_HUB_<NAME>_MODELS` | none | Comma-separated static models, for upstreams without `/v1/models` or while a managed process is stopped. |
| `LLM_HUB_<NAME>_INPUT_USD_PER_1M` | global | Input price. |
| `LLM_HUB_<NAME>_OUTPUT_USD_PER_1M` | global | Output price. |
| `LLM_HUB_<NAME>_CACHE_READ_USD_PER_1M` | 10% of input | Cache-hit price. |
| `LLM_HUB_<NAME>_CACHE_WRITE_USD_PER_1M` | the input price | Cache-write price. |
| `LLM_HUB_<NAME>_MODEL_PRICES` | none | Prices per model: `model:in/out[,cache_read[/cache_write]]`. |
| `LLM_HUB_<NAME>_COMMAND` | none | Local server the hub starts on demand. See [Local servers on demand](providers.md#local-servers-on-demand). |
| `LLM_HUB_<NAME>_IDLE_SECS` | `900` | Stop the managed process after this many idle seconds. `0` means never. |
| `LLM_HUB_<NAME>_START_TIMEOUT_SECS` | `120` | Time the managed process gets to accept connections. |

## Per profile, native Copilot

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_<NAME>_AUTH` | unset | `github-copilot` or `github-copilot-vscode`. See [Native auth](github-copilot.md#native-auth). Only the operator sets it. |
| `LLM_HUB_<NAME>_GITHUB_TOKEN_SOURCE` | `auto` (`github-copilot`), `login` (`github-copilot-vscode`) | `auto`, `gh` or `login`. |
| `LLM_HUB_<NAME>_GH_PATH` | `gh` | The gh binary. Use an absolute path under a service. |
| `LLM_HUB_<NAME>_GH_USER` | unset | Passed as `gh auth token --user`. Letters, digits and `-` only. |
| `LLM_HUB_<NAME>_GITHUB_DOMAIN` | `github.com` | GitHub host for the device login, `gh --hostname` and the credential account. |
| `LLM_HUB_<NAME>_GITHUB_API_URL` | `https://api.github.com`, or `https://api.<domain>` | REST base for `copilot_internal/*`. Must be http or https. |

## Harness command variables

The [harness commands](coding-agents.md) read these. The hub server does not. An empty value
counts as unset.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_URL` | `http://localhost:8888` | Hub address. |
| `LLM_HUB_API_KEY` | `llm-hub` | Key the harness sends to the hub. |
