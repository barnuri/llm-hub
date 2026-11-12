# llm-hub

Single-binary LLM proxy. UI source is `ui-src/`; `npm run build:ui` embeds it into `ui/` for rust-embed. The installed macOS LaunchAgent runs `target/release/llm-hub` from this repo on `0.0.0.0:8888` (not 8410).

## Performance and latency first

Every change must keep the request path at the lowest latency available. Measure before you choose, and pick the option with the lower time to first byte.

- Never add a per-request network call, process spawn, or disk read on the request path. Cache upstream auth, tokens, and lookups in memory, and refresh them in the background or on a 401.
- Reuse the pooled per-profile `reqwest` client. Keep the background model refresh interval (`MODELS_CACHE_TTL`) below both `POOL_IDLE_TIMEOUT` and the upstream's idle close, so the connection stays warm.
- Stream responses through without buffering. Forward a request in its native format when the upstream supports it, instead of translating it.
- Prefer an in-process upstream over an extra proxy hop.
- Review buffering: Balance buffers planning and implementation turns, except plan-backed implementation. Intelligence buffers every turn on chat completions and `/v1/messages` except plan-backed implementation, so a judge or critic can check the draft first. Native Anthropic attempts, `/v1/responses` turns and turns with no eligible reviewer stream. See `docs/smart-auto.md`.
- When a change touches the request path, compare the hub's time to first byte with a direct `curl` to the upstream. The hub may add at most 50 ms.

## After finishing work here

Source and `ui-src/` changes are invisible to the running service until you rebuild and reinstall:

```sh
npm run build:ui
cargo build --release
./target/release/llm-hub service install
```

Then confirm `http://127.0.0.1:8888/healthz` is 200. `service install` is reinstall-safe (bootout + bootstrap). Do this at the end of every session that changes Rust, the UI, or embedded assets.

## Commands

| Command | Description |
|---------|-------------|
| `cargo test` | Unit + integration tests |
| `cargo run` | Foreground on `.env` (default `:8410`) |
| `npm run build:ui` | Typecheck + Vite build into `ui/` |
| `./target/release/llm-hub service install` | Reload LaunchAgent to the new release binary |
| `./target/release/llm-hub service status` | launchd status |

## Public repo

llm-hub is public on GitHub. Keep measured spend, quotas, session or project names and local paths out of docs/, comments, tests, fixtures and commit messages. Put private reports in the gitignored agent-spec/. Check git status for stray reports before a commit.
