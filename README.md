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

Or grab a binary from [releases](https://github.com/barnuri/llm-hub/releases), or build from source: `cargo build --release`.

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

## Environment reference

| Variable | Default | Purpose |
|---|---|---|
| `LLM_HUB_PROFILES` | auto | Optional: profiles are discovered from `_BASE_URL` vars; set this to control order, subset, or dashed names |
| `LLM_HUB_<NAME>_BASE_URL` | — | Upstream base URL (per profile, required) |
| `LLM_HUB_<NAME>_API_KEY` | empty | Upstream key, injected on forward |
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

Profile names map to env segments uppercased with `-` → `_` (`my-proxy` →
`LLM_HUB_MY_PROXY_BASE_URL`). See [.env.example](.env.example).

## Updating

```sh
llm-hub update
```

Checks GitHub releases, downloads the binary for your OS/arch, and swaps it
atomically. The hub also logs a notice on startup when a newer version exists.

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
