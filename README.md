# llm-hub

One local endpoint for every LLM provider you use. llm-hub merges the model
lists of any number of OpenAI-compatible APIs into a single `/v1/models`, and
routes every request by prefix: `groq/llama-3.3-70b-versatile` goes to your
groq profile, `openai/gpt-4o` to OpenAI, `vllm/qwen-32b` to your local vLLM.

![demo](docs/demo.gif)

- **Single binary** (Rust, ~7 MB) — no runtime, no database required
- **Model aggregation** — every upstream's `/v1/models`, prefixed `<profile>/<model>`, one list
- **Streaming passthrough** — SSE streams forwarded unbuffered
- **Fallbacks via header** — `X-LLM-Hub-Fallbacks: groq/llama-3.3-70b, openai/gpt-4o-mini`
- **Web UI** — models, live stats, usage history, profile CRUD, hub API keys, copy-paste setup snippets for Claude Code / Codex / Cursor / SDKs
- **Local-first** — binds `127.0.0.1`, auth optional, self-update built in

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/barnuri/llm-hub/main/scripts/install.sh | sh
```

Or grab a binary from [releases](https://github.com/barnuri/llm-hub/releases), or install from source below.

## Install from source

```sh
git clone https://github.com/barnuri/llm-hub && cd llm-hub
cargo build --release   # binary at target/release/llm-hub
```

Or `cargo install --path .` — note that copy lives outside the repo tree, so it
updates via release binaries unless you run the binary from the repo. Source
installs are auto-detected by the updater, which runs `git pull` + rebuild
instead of downloading a release.

## Quick start

```sh
cd ~/somewhere
cat > .env <<'ENV'
LLM_HUB_OPENAI_BASE_URL=https://api.openai.com/v1
LLM_HUB_OPENAI_API_KEY=sk-...
LLM_HUB_GROQ_BASE_URL=https://api.groq.com/openai/v1
LLM_HUB_GROQ_API_KEY=gsk_...
ENV
llm-hub
# UI:     http://127.0.0.1:8410
# models: curl localhost:8410/v1/models
```

Point any OpenAI-compatible client at it:

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8410/v1", api_key="dummy")
client.chat.completions.create(model="groq/llama-3.3-70b-versatile", messages=[...])
```

The `api_key` is ignored while auth is off (the hub injects each profile's real
key upstream). Set `LLM_HUB_MASTER_KEY` or create hub API keys in the UI to
require one. The **Setup** tab in the UI generates ready snippets for Claude
Code, Codex CLI, Cursor, Continue, aider, OpenAI SDKs, Claude Agent SDK,
LangChain, and LiteLLM.

## Anthropic Messages endpoint

`POST /v1/messages` speaks the Anthropic Messages API on the front and OpenAI
chat-completions to whichever profile you route to, so an Anthropic SDK can
target any upstream the hub knows about.

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8410
export ANTHROPIC_API_KEY=dummy   # ignored while auth is off
curl localhost:8410/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"groq/llama-3.3-70b-versatile","max_tokens":256,
       "system":"be terse","messages":[{"role":"user","content":"hi"}]}'
```

The model id is the usual `<profile>/<model>`, and every `X-LLM-Hub-*` control
header works here exactly as it does on `/v1/chat/completions` — fallback
chains, `X-LLM-Hub-Retry-On`, `X-LLM-Hub-Timeout-Ms`, and the
`X-LLM-Hub-Model` / `X-LLM-Hub-Attempts` response headers. The translated
message body also reports the model actually served, so a request that fell
over to a fallback says so in `model`. Tool names longer than 64 characters —
routine once MCP servers are in play — are aliased on the way upstream and
restored on the way back (see [request transforms](#request-transforms)), so an
Anthropic client keeps using its own names.

`"stream": true` works: the upstream's OpenAI SSE stream is translated frame by
frame into the Anthropic event sequence (`message_start`, `content_block_start` /
`_delta` / `_stop` for text and for streamed `tool_use` with `input_json_delta`,
`message_delta`, `message_stop`). Nothing is buffered beyond the frame being
assembled, and the stream is left well-formed even if the upstream is cut
mid-flight. Two deviations worth knowing:

- Content blocks are **serialized**: OpenAI may interleave `tool_calls` indices,
  Anthropic's protocol cannot, so opening a block closes the previous one.
- `message_start` reports zero tokens and `message_delta` carries the real
  counts — OpenAI only reports usage in a chunk that arrives after the one
  carrying `finish_reason`, so the terminal events are flushed on `[DONE]` (or
  at end of stream) rather than earlier.

Current limits:

- `POST /v1/messages/count_tokens` is not implemented.
- Content that OpenAI chat-completions cannot represent is dropped rather than
  faked: `thinking` / `redacted_thinking` blocks, the `thinking` parameter,
  `top_k`, and Anthropic server tools (`web_search_*`, `computer_*`). An image
  block whose `source.type` is neither `base64` nor `url` is a 400.
- `stop_reason` is never `stop_sequence`, and `stop_sequence` is always `null`:
  the OpenAI wire format never reports which stop string matched. A message that
  carries a `tool_use` block always reports `tool_use`, even when the upstream
  reported `finish_reason: "stop"` alongside its tool calls (llama.cpp, vLLM in
  some configs, and several gateways do) — Anthropic clients drive their agent
  loop off that field, not off the block list.
- Request bodies over `LLM_HUB_MAX_REPLAY_BYTES` are a 400 here rather than an
  unbuffered passthrough — translation needs the whole document.

## Fallbacks

Send an ordered chain; the hub retries the next model only on connect errors,
timeouts, 408, 429, or 5xx. It never retries 400/401/403/404/422 — those
describe your request and would fail identically everywhere.

```sh
curl localhost:8410/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'X-LLM-Hub-Fallbacks: groq/llama-3.3-70b-versatile, openai/gpt-4o-mini' \
  -d '{"model":"openai/gpt-5","messages":[{"role":"user","content":"hi"}]}'
```

Response headers always tell you what happened:

```
X-LLM-Hub-Model: groq/llama-3.3-70b-versatile
X-LLM-Hub-Attempts: openai/gpt-5=429, groq/llama-3.3-70b-versatile=200
```

Extras: `X-LLM-Hub-Timeout-Ms` (per-attempt deadline), `X-LLM-Hub-Retry-On:
429,503` (replace the retry status set), `LLM_HUB_DEFAULT_FALLBACKS` (chain
applied when no header is present). Fallback needs the request body buffered
for replay; bodies over `LLM_HUB_MAX_REPLAY_BYTES` (default 2 MB) stream
through with fallbacks disabled.

Set the default chain any of three ways (per-request header always wins):

- **UI** — the **Profiles** tab has a "Default fallbacks" editor: pick models
  from the live list, reorder, save. For the per-request header, the **Setup**
  tab has an optional fallback picker that bakes `X-LLM-Hub-Fallbacks` into
  every generated client snippet.
- **API** — `POST /api/fallbacks` with
  `{"fallbacks":["groq/llama-3.3-70b-versatile","openai/gpt-4o-mini"]}`.
- **Env** — `LLM_HUB_DEFAULT_FALLBACKS=groq/llama-3.3-70b-versatile,openai/gpt-4o-mini`
  in `.env`. The UI and API persist to the same variable.

## Request transforms

Two opt-in request headers reshape the body on its way upstream, for the times
the client cannot. Both apply to `/v1/chat/completions` and to `/v1/messages`
alike — the Anthropic body is translated first, so one implementation covers
both — and both run once, before the fallback chain, so every attempt sends
byte-identical bytes.

| Header | Direction | Values | Effect |
|---|---|---|---|
| `X-LLM-Hub-System-Prompt-Mode` | request | `none` \| `truncate` \| `drop` | Trims or drops the system prompt. `truncate` caps each `system`/`developer` message at 1000 characters (`system` on the Anthropic route); `drop` removes them, unless that would leave no messages at all. |
| `X-LLM-Hub-Reasoning-Strip` | request | `true` \| `1` \| `yes` \| `on` | Removes `reasoning`, `reasoning_effort` and `thinking` from the request — for upstreams that 400 on parameters they do not model. |
| `X-LLM-Hub-Transforms` | response | e.g. `system-prompt=truncate, tool-names=3, reasoning-strip` | Diagnostic: which transforms actually fired. Omitted when none did. |

An unrecognized value — or an absent header — leaves the request untouched, with
no error and no warning. Nothing here is inferred from the model name or the
profile: a transform that fired for the primary but not for a fallback would
make `X-LLM-Hub-Attempts` describe two different requests.

Two more repairs are always on, because the clients they fix cannot ask for
them.

**Streamed first-delta role.** When a streamed first `choices[0].delta` arrives
without a `role`, the hub inserts `role: "assistant"` (the OpenAI spec requires
it; some local servers omit it, which breaks clients that rebuild the message
from deltas alone). An existing role is never overwritten, only the first delta
is touched, and the rewriter stops framing the stream the moment it is done.
Set `LLM_HUB_STREAM_ROLE=false` to disable it.

**Tool-name truncation.** OpenAI-compatible upstreams reject a function name
over 64 characters with a 400, and MCP tools blow past that routinely — a tool
reached through a server prefix is named `mcp__<server>__<tool>`. Any longer
name is replaced on the way upstream with `<first 55 chars>_<8 hex of its
sha256>`, and the original is put back in the response — in the OpenAI
`tool_calls[].function.name`, in the Anthropic route's `tool_use` blocks, and
frame by frame in a stream. `tools[]`, `tool_choice`, and `tool_calls[]` on prior assistant
messages are all rewritten, so a forced tool choice keeps naming a tool the
request still declares. The alias is a pure function of the name: it is the
same on every turn of a conversation and on every attempt in a fallback chain,
and nothing is stored between requests. Names of 64 characters or fewer — and
aliases echoed back by the client — are passed through untouched, so
`X-LLM-Hub-Transforms` only reports `tool-names=N` when something was actually
renamed.

## Environment reference

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_PROFILES` | auto | Optional: profiles are discovered from `_BASE_URL` vars; set this to control order, subset, or dashed names |
| `LLM_HUB_<NAME>_BASE_URL` | — | Upstream base URL (per profile, required) |
| `LLM_HUB_<NAME>_API_KEY` | empty | Upstream key, injected on forward |
| `LLM_HUB_<NAME>_DISPLAY_NAME` | — | Optional UI label; routing id stays `<NAME>` |
| `LLM_HUB_<NAME>_HEADERS` | — | Extra headers, JSON object |
| `LLM_HUB_<NAME>_TIMEOUT_MS` | none | Per-attempt timeout for this profile |
| `LLM_HUB_<NAME>_ENABLED` | `true` | Disable without deleting |
| `LLM_HUB_<NAME>_MODELS` | — | Static model list when upstream lacks `/v1/models` |
| `LLM_HUB_BIND` / `LLM_HUB_PORT` | `127.0.0.1` / `8410` | Listen address |
| `LLM_HUB_MASTER_KEY` | unset | Require this key on `/v1/*` and `/api/*`; unset = no auth |
| `LLM_HUB_DEFAULT_FALLBACKS` | — | Default fallback chain |
| `LLM_HUB_MAX_REPLAY_BYTES` | `2097152` | Body buffer cap for fallback replay |
| `LLM_HUB_PERSISTENT` | `false` | Durable usage history + hub API keys |
| `LLM_HUB_STORE` | `sqlite` | `sqlite` or `json` (when persistent) |
| `LLM_HUB_STORE_PATH` | `llm-hub.db` | Store location |
| `LLM_HUB_CONFIG_READONLY` | `false` | Block UI writes to `.env` |
| `LLM_HUB_STREAM_ROLE` | `true` | Inject `role:"assistant"` into the first streamed delta when the upstream omits it |
| `LLM_HUB_AUTO_UPDATE` | `true` | Apply updates automatically (startup + daily check); `false` = notice only |

Profile names map to env segments uppercased with `-` → `_` (`my-proxy` →
`LLM_HUB_MY_PROXY_BASE_URL`). See [.env.example](.env.example).

## Run as a service

```sh
cd ~/somewhere   # the directory with your .env
llm-hub service install
```

Registers the hub as an always-on background service: a launchd LaunchAgent on
macOS, a systemd user unit on Linux (run `loginctl enable-linger $USER` to
start at boot without a login session), a Task Scheduler task on Windows. Run
it from the directory containing your `.env` — that working directory is baked
into the service definition, so the service reads that `.env` for profiles,
keys, and fallbacks. The definition also bakes in `LLM_HUB_PORT=8888` and
`LLM_HUB_BIND=0.0.0.0`, which win over `.env` — an installed service always
serves on `0.0.0.0:8888`, while foreground runs keep using `.env`. Logs land in
`<that directory>/logs/llm-hub.log`. `llm-hub service uninstall` removes it,
`llm-hub service status` checks it. Reinstall after updating to pick up
definition changes. On macOS, install also adds the binary to the Application
Firewall allow list (sudo may prompt) — without it, macOS silently drops LAN
connections to unsigned background binaries on every port.

## Updating

Auto-update is on by default: the hub checks GitHub releases on startup and
daily, downloads the binary for your OS/arch, and swaps it atomically. Source
installs are updated via `git pull` + `cargo build --release` instead (the
automatic pull is skipped while the checkout has uncommitted changes). Set
`LLM_HUB_AUTO_UPDATE=false` to only log a notice when a newer version exists.

```sh
llm-hub update   # manual one-shot update, same logic
```

## Docker

```sh
docker build -t llm-hub .
docker run --rm -p 8410:8410 --env-file .env -e LLM_HUB_BIND=0.0.0.0 llm-hub
```

## Development

```sh
cargo test                 # unit + integration (wiremock)
cargo run                  # uses ./.env
node scripts/record-demo.mjs   # re-record docs/demo.gif (playwright + ffmpeg)
```

## License

MIT
