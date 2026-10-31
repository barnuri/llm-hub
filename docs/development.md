# Development

| Command | Purpose |
|---|---|
| `cargo test` | Unit and integration tests (wiremock). |
| `cargo run` | Run in the foreground with `./.env` (default port 8410). |
| `npm run build:ui` | Type-check and build `ui-src/` into `ui/`. The binary embeds `ui/`. |
| `cargo build --release` | Build `target/release/llm-hub`. |
| `npm run record-demo` | Record `docs/demo.gif` again (needs playwright and ffmpeg). |

## Reinstall after a change

An installed service runs `target/release/llm-hub` from the checkout. Changes to Rust code,
`ui-src/` or embedded files reach it only after a rebuild and a reinstall:

```sh
npm run build:ui
cargo build --release
./target/release/llm-hub service install
curl -fsS http://127.0.0.1:8888/healthz   # expect: ok
```

`scripts/auto-reinstall.sh` runs the same steps when `src`, `ui-src/src`, `Cargo.toml` or
`Cargo.lock` are newer than the release binary.

## Design notes

- [Anthropic Messages endpoint and transforms plan](anthropic-and-transforms-plan.md): the
  original design of `/v1/messages` and the request transforms.
- [Subscriptions](subscriptions.md): why the hub does not proxy subscription plans.
