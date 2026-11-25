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
`Cargo.lock` are newer than the release binary. It is for development only: it builds the working
tree, which can hold unmerged work, and it refuses when the service runs from a releases dir.

## Deploy stable

The always-on service should run merged code only. `scripts/deploy-stable.sh` deploys a ref
(default `origin/master`) from a dedicated clone that nobody edits:

```sh
git clone <repo-url> ~/llm-hub-deploy          # once; any path works
scripts/deploy-stable.sh --clone ~/llm-hub-deploy --dry-run
scripts/deploy-stable.sh --clone ~/llm-hub-deploy
```

It fetches, skips a sha that is already deployed, requires green GitHub checks on the commit
(`--skip-ci` bypasses; `GH` overrides the `gh` command), builds with `cargo build --release --locked`
(and `npm run build:ui` when `ui-src` changed), copies the binary to `<releases>/<sha>/llm-hub`,
points `<releases>/current` at it, runs `service install`, and waits 20 s for `/healthz`. On failure it
points `current` back to the previous release and reinstalls. It keeps the last 3 releases and takes a
lock against parallel runs. Flags: `--ref`, `--releases` (default `~/.local/share/llm-hub/releases`,
or `LLM_HUB_RELEASES`), `--keep`.

`service install` writes `<releases>/current/llm-hub` when it runs from a release dir, so a dev build in
`target/` never changes what the service runs. The service working directory is the parent of the
releases dir: put `.env` and `logs/` there. `GET /api/build` and the UI header show the running commit.

### Roll back

```sh
R=~/.local/share/llm-hub/releases
ls -t $R                                   # pick an older sha
ln -sfn $R/<sha> $R/current
$R/current/llm-hub service install
curl -fsS http://127.0.0.1:8888/healthz
```

The next `deploy-stable.sh` run deploys `origin/master` again, so revert or fix master first.

## Design notes

- [Anthropic Messages endpoint and transforms plan](anthropic-and-transforms-plan.md): the
  original design of `/v1/messages` and the request transforms.
- [Subscriptions](subscriptions.md): why the hub does not proxy subscription plans.
- [Extensions](extensions.md): rules files and Rust extensions.
