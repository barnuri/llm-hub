---
name: llm-hub-perf
description: >
  Measure llm-hub's added latency on the request path, and decide from the numbers whether a
  performance change is needed. Use before and after a change to the request path, when a user
  reports llm-hub as slow, or to compare two builds. Holds the baseline numbers and the known
  candidate optimizations, each with the measurement that would justify it.
  Triggers: "is the hub slower", "measure ttfb", "performance check", "did this hurt latency",
  "optimize the request path".
allowed-tools: Bash, Read, Grep, Glob, Edit, Write, AskUserQuestion
---

# llm-hub-perf

Run from the repo root. Read the repo `CLAUDE.md` first: the hub may add at most 50 ms to the time
to first byte of a direct upstream call.

Change code for performance only when a measurement shows a need. A difference inside the
run-to-run spread (about ±0.1 ms at the median on a laptop) is noise.

## Measure

1. Build the binaries to compare: `cargo build --release`, and copy `target/release/llm-hub` to a
   temp dir for each build. Keep the old build as the baseline.
2. Run `scripts/bench.sh` from this folder. It starts a mock upstream and each binary in turn,
   and prints the median and p90 time to first byte:

   ```sh
   .claude/skills/llm-hub-perf/scripts/bench.sh before=/tmp/llm-hub-before after=$PWD/target/release/llm-hub
   BODY_KB=200 .claude/skills/llm-hub-perf/scripts/bench.sh before=... after=...
   RULES=path/to/extensions.yaml .claude/skills/llm-hub-perf/scripts/bench.sh after=...
   ```

   `RUNS` (default 300) and `ROUNDS` (default 2) set the sample size. Two rounds interleave the
   binaries, so a slow minute on the machine does not favor one of them.
3. Compare medians across rounds. Report p90 only as a hint, because it is noisy on a laptop.
4. For a real upstream, use `scripts/measure-ttfb.sh` at the repo root against the direct URL.

## Baseline (2026-11-18, release build, Apple Silicon laptop, mock upstream)

| Setup | 1 KB body median | 200 KB body median |
|---|---|---|
| Direct to the mock upstream | 0.64 ms | not measured |
| llm-hub before extensions | 0.75–0.83 ms | 1.00–1.09 ms |
| llm-hub with extensions, no rules file | 0.81–0.85 ms | 1.06–1.10 ms |
| With 2 rules, one changes the body | 0.87–0.88 ms | 1.21–1.23 ms |

## Candidate optimizations

Apply one only when a measurement shows its cost. Each line names the measurement to take first.

| Id | Candidate | Expected gain | Take this measurement first |
|---|---|---|---|
| C1 | `response_phase` clones the reply headers twice on every attempt, because the built-in extension makes the set non-empty. Skip the clone when no extension changes headers, or clone once and diff lazily | Microseconds | Bench a reply with 30+ headers, before and after |
| C2 | A rule that matches on `model` or `stream` parses the whole body into a `serde_json::Value`. Read only those two fields with a borrowed `serde_json` struct | About 0.1 ms per 200 KB body with rules | `BODY_KB=200` with a rule that matches on `model` but changes nothing |
| C3 | The request and attempt phases build contexts and clone `Bytes` handles when no extension overrides those phases. Let `ExtensionSet` record at build time which phases have an override, through a `fn phases()` hint on built-ins and rules | Sub-microsecond | Only with a profile (`cargo flamegraph`) that shows the phase loop |
| C4 | `panic = "unwind"` in the release profile adds unwind tables (13.1 MB to 16.4 MB binary with the YAML parser). It is needed to catch a panicking extension | Binary size, not latency | Do not change without a decision about extension panics |

## When to run

Run it for every change to the request path: `src/routes/proxy.rs`, `src/routes/anthropic.rs`, `src/ext/`, the `src/services/` modules the proxy calls, `src/utils/headers.rs`, or `Cargo.toml` dependencies and release profile. Bench `master` and the branch, with a 1 KB
and a 200 KB body, and also with `RULES` when the change touches `src/ext/`.

## Report

Put the result in the PR body under `## Performance`, in this shape:

```markdown
## Performance

No regression. Medians of time to first byte, mock upstream, 300 runs per build, 2 rounds:

| Build | 1 KB median | 200 KB median |
|---|---|---|
| master | 0.81–0.85 ms | 1.06–1.10 ms |
| this PR | 0.80–0.86 ms | 1.05–1.11 ms |
```

Lead with the verdict: "No regression", or "Regression of N ms at the median in <case>", with the
cause. Give the range across rounds for each build. A regression above the noise needs a fix or
a stated reason before merge.
