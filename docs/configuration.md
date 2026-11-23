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
| `LLM_HUB_EXTENSIONS_FILE` | `extensions.yaml`, `extensions.yml` or `extensions.json` in the working directory | Rules file for [extensions](extensions.md#rules). Reloaded on change. `llm-hub extensions check` validates it. |
| `LLM_HUB_EXTENSIONS_DISABLED` | empty | Comma-separated extension names or globs to turn off, such as `upstream-limits` or `local-*`. `hooks` turns off the hooks of the rules file, and `rules:<name or glob>` turns off single rules. The UI cannot turn these back on. Applied when `.env` changes. See [Turn extensions on and off](extensions.md#turn-extensions-on-and-off). |
| `LLM_HUB_STREAM_ROLE` | `true` | Add `role: "assistant"` to the first streamed delta when the upstream leaves it out. |
| `LLM_HUB_SSE_KEEPALIVE_MS` | `15000` | Idle time before an SSE keepalive comment. `0` turns it off. |
| `LLM_HUB_DEADLINE_MS` | none | Default time limit of a whole fallback chain. `X-LLM-Hub-Deadline-Ms` wins. See [Fallbacks](fallbacks-and-transforms.md#fallbacks). |
| `LLM_HUB_QUOTA_COOLDOWN_SECS` | `3600` | How long a profile that is out of credits is skipped. `0` cools nothing, but the next fallback still runs. |
| `LLM_HUB_FIRST_BYTE_TIMEOUT_MS` | `20000` | Longest wait for the first byte of a streamed Smart Auto attempt on a remote profile. A model that misses it is cooled and the next fallback is tried. `0` turns it off. See [Live signals](smart-auto.md#live-signals). |
| `LLM_HUB_COPILOT_PRICES_REFRESH_SECS` | `86400` (24 hours) | How often the hub fetches Copilot model prices from GitHub Docs while a Copilot profile is enabled. The same interval governs Copilot model descriptions fetched from the GitHub Docs model comparison page. Fetches stay at least 1 hour apart. `0` turns the fetch off and the built-in prices apply. Editable on the Models page. |
| `LLM_HUB_COPILOT_GUARD_MODELS` | `*` | Models (`<model>`, `<profile>/<model>` or `*`) whose Copilot CLI requests get the tool-loop guard. The guard adds a system-prompt note. It also withholds a tool that the history shows called the same way 3 times in a row. Empty turns it off. |
| `LLM_HUB_COPILOT_STRIP_SAMPLING_MODELS` | 10 local model ids (`DEFAULT_COPILOT_STRIP_SAMPLING_MODELS` in `src/services/transforms.rs`) | Models whose Copilot CLI requests lose Copilot's fixed `temperature`, `top_p` and penalties, so the model's own sampling applies. Empty turns it off. |

## Keep-warm

Keep-warm sends a one-token ping on an idle stream so its prompt cache stays warm. It works for
every remote profile, on chat completions, messages and responses calls. See
[Cache keep-warm](github-copilot.md#cache-keep-warm) for what it sends and what it costs.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_KEEP_WARM` | `true` | Turns the pings on or off for every profile. |
| `LLM_HUB_<NAME>_KEEP_WARM` | `true` | `false` stops the pings for this one profile. The Profiles page has a "Keep prompt cache warm" checkbox that writes it. |
| `LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES` | `30` | Minutes after a stream's last user request at which the pings stop. The range is `5` to `10080` (one week). A value outside it fails the config load. |

The Models page writes both variables to `.env`. A value set in the process environment wins over
`.env`, so the page shows its controls disabled when the service environment sets either variable.

## Media fallback

Media fallback turns images, audio and files into text before an attempt goes to a model that
cannot read them. Off by default. See [Media fallback](media-fallback.md).

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_MEDIA_FALLBACK` | `false` | Turns media fallback on. |

| `LLM_HUB_MEDIA_FALLBACK_VISION_MODEL` | `llama_swap/muse-glimmer-30b` | The `<profile>/<model>` that captions images. Empty: images become placeholders. |
| `LLM_HUB_MEDIA_FALLBACK_STT_MODEL` | unset | The `<profile>/<model>` that transcribes audio through `/v1/audio/transcriptions`. Unset: audio becomes a placeholder. |
| `LLM_HUB_MEDIA_FALLBACK_TEXT_ONLY` | empty | Comma-separated model globs (`llama_swap/*`) that always get text, on top of the models the hub knows cannot read images. |
| `LLM_HUB_MEDIA_FALLBACK_MAX_ITEM_BYTES` | `20971520` (20 MiB) | Largest encoded media item that is converted. A larger one becomes a placeholder. |
| `LLM_HUB_MEDIA_FALLBACK_MAX_ITEMS` | `16` | Conversions per request that are not cached. Later items become placeholders. |
| `LLM_HUB_MEDIA_FALLBACK_TIMEOUT_MS` | `120000` | Longest wait for one conversion. A local model may have to load first. |

The Models page writes the first three variables to `.env`, and shows its controls disabled when
the service environment sets one of them.

## Local model tuner

The tuner measures how fast local models prefill, and proposes server flag changes. See
[Local model tuner](local-tuner.md). The per-profile keys are env-only, and a profile save on the
Profiles page keeps them.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_LOCAL_TUNER` | `false` | Turns the background probe and its proposals on. The prefill guard needs it too, because only the probe measures prefill speed. The probe reads stored request rows, so it needs `LLM_HUB_PERSISTENT=true`. |
| `LLM_HUB_<NAME>_PREFILL_GUARD` | `off` | Skip a model of this profile that cannot prefill the prompt within the budget. `on` uses `LLM_HUB_<NAME>_FIRST_BYTE_TIMEOUT_MS` when it is above 0, else `LLM_HUB_<NAME>_TIMEOUT_MS`, and fails the config load when neither is above 0. A number is the budget in ms. See [The prefill guard](local-tuner.md#the-prefill-guard). |
| `LLM_HUB_<NAME>_TUNER_REGISTRY` | none | The pipe-delimited model registry that Approve edits. `~/` is expanded. Without it, Approve answers 409. |
| `LLM_HUB_<NAME>_TUNER_APPLY_COMMAND` | none | Command that runs after an approved registry edit, for example a script that regenerates the llama-swap config and restarts it. Quotes and `~/` work. It runs without a shell. |
| `LLM_HUB_<NAME>_TUNER_APPLY_TIMEOUT_MS` | `120000` | The hub kills the apply command after this time and marks the proposal `failed`. Must be above 0. |

## Notifications

See [Notifications](notifications.md). The hub builds the sinks at startup, so a change to
`LLM_HUB_NOTIFY_DESKTOP`, `LLM_HUB_NOTIFY_WEBHOOK_URL`, `LLM_HUB_NOTIFY_COMMAND` or
`LLM_HUB_NOTIFY_TIMEOUT_MS` needs a restart. The other variables take effect when `.env` changes.

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_NOTIFICATIONS` | `true` | `false` turns the inbox and every sink off. |
| `LLM_HUB_NOTIFY_DESKTOP` | `true` | `false` stops the desktop popup. |
| `LLM_HUB_NOTIFY_WEBHOOK_URL` | none | An `http://` or `https://` URL that receives each notification as a JSON POST. |
| `LLM_HUB_NOTIFY_COMMAND` | none | A command that receives each notification as JSON on stdin. Quotes and `~/` work. It runs without a shell. |
| `LLM_HUB_NOTIFY_MIN_SEVERITY` | `warning` | `info`, `warning` or `critical`. A lower severity reaches the inbox but no sink. |
| `LLM_HUB_NOTIFY_TIMEOUT_MS` | `10000` | Longest wait for one desktop popup, webhook call or command run. Must be above 0. |
| `LLM_HUB_NOTIFY_MUTE` | empty | Mute rules: `;` between rules, `field=glob` pairs separated by `,` in a rule. Fields: `source`, `kind`, `rule`, `profile`, `model`, `title`. At most 200 rules and 256 characters per glob. See [Mute and snooze](notifications.md#mute-and-snooze). |
| `LLM_HUB_NOTIFY_BUDGET_STEPS` | `25,50,75,80,85,95,100` | Budget steps in percent, 1 to 1000, at most 20. See [Budget notifications](notifications.md#budget-notifications). |
| `LLM_HUB_BUDGET_MONTHLY_USD` | none | Monthly spend budget in USD for all profiles together, against the hub's measured cost. A number above 0. Needs `LLM_HUB_PERSISTENT=true`. |
| `LLM_HUB_<NAME>_BUDGET_MONTHLY_USD` | none | Monthly spend budget in USD for one profile. A number above 0. Env-only: a profile save on the Profiles page keeps it. |

## Persistence

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_PERSISTENT` | `false` | Keep usage history, errors, hub API keys, learned Copilot limits, learned rejected fields, notifications and the mute rules added in the UI. |
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
| `LLM_HUB_<NAME>_KEEP_WARM` | `true` | `false` stops [keep-warm](#keep-warm) pings for this profile. |
| `LLM_HUB_<NAME>_FIRST_BYTE_TIMEOUT_MS` | none | First-byte limit for a streamed Smart Auto attempt on this profile. It beats `LLM_HUB_FIRST_BYTE_TIMEOUT_MS`, applies to a local server too, and `0` turns the limit off for the profile. Env-only. |
| `LLM_HUB_<NAME>_MODELS` | none | Comma-separated static models, for upstreams without `/v1/models` or while a managed process is stopped. |
| `LLM_HUB_<NAME>_INPUT_USD_PER_1M` | global | Input price. |
| `LLM_HUB_<NAME>_OUTPUT_USD_PER_1M` | global | Output price. |
| `LLM_HUB_<NAME>_CACHE_READ_USD_PER_1M` | 10% of input | Cache-hit price. |
| `LLM_HUB_<NAME>_CACHE_WRITE_USD_PER_1M` | the input price | Cache-write price. |
| `LLM_HUB_<NAME>_MODEL_PRICES` | none | Prices per model: `model:in/out[,cache_read[/cache_write]]`. |
| `LLM_HUB_<NAME>_COMMAND` | none | Local server the hub starts on demand. See [Local servers on demand](providers.md#local-servers-on-demand). |
| `LLM_HUB_<NAME>_IDLE_SECS` | `900` | Stop the managed process after this many idle seconds. `0` means never, and the hub starts the process at boot. |
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
