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
- A change to the request path (`src/routes/proxy.rs`, `src/routes/anthropic.rs`, `src/ext/`, the `src/services/` modules the proxy calls, `src/utils/headers.rs`, or `Cargo.toml` dependencies and release profile) runs the `llm-hub-perf` skill on `master` and on the branch. Its table of medians goes in the PR body under `## Performance`. The skill holds the bench script, the baseline and the candidate optimizations.
- Change code for performance only when a measurement shows the need.
- Keep the binary small. The `binary-size` check fails a PR whose release binary grows more than 2% against the latest release, unless the PR body has a `## Binary size` section that says what added the size and why it is worth it. Check locally with `cargo build --release && scripts/binary-size.sh`. Prefer a feature flag or a smaller crate to a large dependency.

## Extensions

`src/lib.rs` is the crate. `src/main.rs` only calls `llm_hub::run()`. Request behavior that can run in a phase (`http_request`, `request`, `attempt`, `response`, `http_response`, `complete`, `config_change`) goes in an extension, not in `attempt_loop`. Use a built-in in `src/ext/builtin/`, a rule in a rules file, or a user's own `Extension`. See `docs/extensions.md`. The public API is `llm_hub::Hub` and `llm_hub::ext`. A change there breaks library users.

## Tests and coverage

Every change to `src/` comes with unit tests in the same change. The `coverage` check fails when the line coverage of any file under `src/` is below 80%.

- Cover every new code path: the success path, the edge cases and the error path. A bug fix adds a test that fails before the fix.
- Test a new opt-in feature in the on state. Also test that the default (off) state keeps the old behavior.
- Mock upstreams with `wiremock`. Never call a real provider, service manager or keychain from a test.
- CI measures coverage on Linux, so code for another OS counts only there.

## Checks before you report done

Run `scripts/check.sh` before you report a change as done, and fix every failure. It runs what CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the tests with the 80% per-file coverage gate. `scripts/check.sh fast` runs only format and lint. There are no git hooks: the GitHub checks on the PR are the gate.

Never raise, loosen or suggest raising a CI gate (`TEST_TOTAL_MAX_S`, the 1000 ms slowest-test limit, `COVERAGE_MIN`, in `scripts/test-speed.ts` or the workflows). Each gate exists for a reason. When one fails, fix the tests or code that cause it. A re-run of a flaky gate is a diagnosis step, not a fix. Do not offer "raise the gate" as an option.

Clippy enforces these rules, so do not spend review on them. The `[lints]` table in `Cargo.toml` and `clippy.toml` hold the full list:

- No `unwrap`, `expect`, `panic!`, `todo!` or `dbg!` outside tests. Return a `Result` or an `Option`.
- Every `#[allow(...)]` names a `reason = "..."`.
- Every `unsafe` block has a `// SAFETY:` comment.
- `Arc::clone(&x)`, not `x.clone()`, for a ref-counted pointer.
- Every public item in the library (`llm_hub::*`) has a `///` doc and implements `Debug`.

Clippy cannot check these, and reviews found them most often:

- Check every sentence of a doc change against the code. Name each case the code does not cover.
- Search for an existing helper before you write one, such as `utils/glob.rs` or the capped body readers in `proxy.rs`. Reuse or extend it. Never add a second copy.
- Extract logic that two places repeat into one function or trait. Do not copy it.
- Use `StatusCode` and its `is_client_error()` and `is_server_error()`, not bare `u16` numbers.
- Do not clone to satisfy the borrow checker. Restructure the borrow first.
- Keep comments to two lines, in the active voice, with the actor named.
- When code moves, move its tests with it.

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
| `scripts/check.sh` | Format, lint, and tests with the coverage gate, as CI runs them |
| `scripts/check.sh fast` | Format and lint only |
| `cargo llvm-cov --all-targets --summary-only` | Tests with a per-file line coverage table |
| `cargo run` | Foreground on `.env` (default `:8410`) |
| `npm run build:ui` | Typecheck + Vite build into `ui/` |
| `./target/release/llm-hub service install` | Reload LaunchAgent to the new release binary |
| `./target/release/llm-hub service status` | launchd status |

## Public repo

llm-hub is public on GitHub. Keep measured spend, quotas, session or project names and local paths out of docs/, comments, tests, fixtures and commit messages. Put private reports in the gitignored agent-spec/. Check git status for stray reports before a commit.
