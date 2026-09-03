# llm-hub

Single-binary LLM proxy. UI source is `ui-src/`; `npm run build:ui` embeds it into `ui/` for rust-embed. The installed macOS LaunchAgent runs `target/release/llm-hub` from this repo on `0.0.0.0:8888` (not 8410).

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
