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

## Tests

<!-- The tests that cover this change. A bug fix needs a test that fails before the fix. -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Follows [CONTRIBUTING.md](../CONTRIBUTING.md): SOLID, one responsibility per module, no new per-request network call or disk read
- [ ] Request-path changes: time to first byte measured against a direct upstream call (at most 50 ms added)
- [ ] Docs in `docs/` updated for a new option or behavior
- [ ] No secrets, internal hosts, quotas or personal paths in code, tests or the description
