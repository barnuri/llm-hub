# Contributing to llm-hub

Issues and pull requests are welcome from everyone. Open an issue first for a feature or a
behavior change, so the design is agreed before you write code. A small bug fix can go straight
to a pull request.

## Set up

```sh
git clone https://github.com/barnuri/llm-hub && cd llm-hub
npm run build:ui        # builds ui-src/ into ui/, which the binary embeds
cargo test
cargo run               # foreground on ./.env, port 8410
```

See [docs/development.md](docs/development.md) for the full command list.

## Rules for every change

### 1. Keep the current behavior as the default

llm-hub has existing users. A new feature is opt-in: an env var, a request header, or a config
value turns it on, and the hub behaves as before when it is off. Change a default only to fix a
bug or when the old default is harmful, and say so in the pull request.

This includes error shapes and status codes. Clients branch on them, so a change to an error
body or status is a behavior change.

### 2. Follow SOLID

- **Single responsibility.** One module does one job. Put new logic in its own file under
  `src/services/` and call it from the route. Do not grow `src/routes/proxy.rs` with
  provider-specific branches.
- **Open/closed.** Extend through the existing seams (`services/transforms.rs`,
  `services/body_transform.rs`, profile config) instead of editing the core request loop.
- **Liskov substitution.** A new upstream, credential store or transport must work wherever the
  trait it implements is used, with no special cases at the call site.
- **Interface segregation.** Keep traits small. A caller depends only on the methods it uses.
- **Dependency inversion.** Depend on traits, not concrete clients, so tests can swap in fakes
  (see `FakeGhCli`, `MemoryCredentialStore`).

### 3. Keep the request path fast

- No new per-request network call, process spawn or disk read. Cache in memory and refresh in
  the background.
- Stream responses through. Buffer only error bodies or opt-in features that need the whole
  answer.
- For a request-path change, compare the hub's time to first byte with a direct `curl` to the
  upstream. The hub may add at most 50 ms.

### 4. Test it

- A bug fix comes with a test that fails before the fix.
- A feature comes with tests for the on state and the off (default) state.
- Upstream calls in tests use `wiremock`. Tests never call a real provider.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass.
  The `pr` workflow runs all three on Linux and Windows.

### 5. Keep the scope small

One pull request solves one problem. A provider quirk that only one client needs belongs behind
an opt-in flag. Unrelated cleanup goes in its own pull request.

### 6. Keep it public-safe

The repo is public. Keep tokens, internal hosts, tenant ids, quotas, spend figures, session names
and local paths out of code, tests, fixtures, docs and commit messages.

## Pull requests

- Fill in the pull request template: what and why, behavior for existing users, screenshots or
  request/response output, and tests.
- Pull requests are squash-merged. The pull request title becomes the commit message on
  `master`, so write it as an imperative sentence ("Return context_length_exceeded for Copilot
  Responses rejections").
- Every merge to `master` publishes a release, so `master` must always pass the checks.
