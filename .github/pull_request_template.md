## What and why

<!-- One paragraph. Which problem does this fix, or which feature does it add? Link the issue: "Fixes #123". -->

## Type

- [ ] Bug fix
- [ ] Feature
- [ ] Refactor (no behavior change)
- [ ] Docs or CI

## How to reproduce (bug fix)

<!-- The request, client and model that failed, and what the hub returned before this change. -->

## Behavior for existing users

<!-- llm-hub has existing users. New behavior must be opt-in (env var, header or config) and the
     current behavior must stay the default. If this PR changes a default, say what changes, who it
     affects, and why opt-in is not enough. -->

- [ ] The default behavior is unchanged
- [ ] The default behavior changes (explain above)

## Screenshots or output

<!-- UI changes: before and after screenshots. API changes: the request and the response
     (status, body or SSE events). Remove tokens, hosts and personal data. -->

## Performance

<!-- Required when this PR changes the request path: src/routes/proxy.rs, src/routes/anthropic.rs,
     src/ext/, the src/services/ modules the proxy calls, src/utils/headers.rs, or Cargo.toml
     dependencies and the release profile. Run the llm-hub-perf skill (.claude/skills/llm-hub-perf/)
     on master and on this branch, and paste its table. Otherwise write "Not on the request path." -->

| Build | 1 KB median | 200 KB median |
|---|---|---|
| master | | |
| this PR | | |

## Tests

<!-- The tests that cover this change. A bug fix needs a test that fails before the fix.
     Every new code path in src/ needs a unit test: success path, edge cases, error path.
     The `coverage` check fails when any file under src/ is below 80% line coverage. -->

- Coverage of the files this PR changes (from the `coverage` check summary): <!-- e.g. src/services/x.rs 93% -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Unit tests cover every new code path in `src/`, and every file under `src/` has at least 80% line coverage
- [ ] A new opt-in feature has a test for the on state and a test for the default state
- [ ] Follows [CONTRIBUTING.md](../CONTRIBUTING.md): SOLID, one responsibility per module, no new per-request network call or disk read
- [ ] Request-path changes: the Performance table above comes from the `llm-hub-perf` skill (at most 50 ms added)
- [ ] Docs in `docs/` updated for a new option or behavior
- [ ] No secrets, internal hosts, quotas or personal paths in code, tests or the description
