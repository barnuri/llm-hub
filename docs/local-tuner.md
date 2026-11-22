# Local model tuner

A local model can look healthy and still fail inside a coding agent. An agent sends a large prompt
on every turn, and a local server may need many minutes to prefill it. The client gives up first,
and the user sees no answer. The local tuner measures how fast each local model prefills, warns
before a request cannot finish, and proposes server flag changes. It never applies a change
without your approval.

The tuner has three parts:

- The **probe** measures prefill speed and reads the server's state in the background.
- The **prefill guard** skips a model that cannot prefill the prompt within a budget, before the
  hub sends the request.
- The **rules** turn the evidence into proposals and advice on the
  [Notifications](notifications.md) page. An approved proposal edits your model registry file.

The probe and the rules are off by default. Turn them on with `LLM_HUB_LOCAL_TUNER=true`. The
guard is a separate opt-in per profile, and it also needs the tuner on, because only the probe
measures the prefill speed the guard reads. [Configuration](configuration.md#local-model-tuner)
lists every variable.

The tuner needs persistence (`LLM_HUB_PERSISTENT=true`). The probe measures speed from the hub's
stored request rows. Without a store it has no rows: it fits nothing, the guard lets every request
through, and only the rules that read the server's flags can fire.

## Native local probe

For a `KIND=local` profile, the native probe reads the local pool and the registry without starting a model. The probe reports running model ids, process state and registry health. Use the [configuration keys](configuration.md#native-local-serving) to set the registry, memory budget and process limits.

The native pool also tracks llama.cpp process state. A llama.cpp stream stall may leave `/health` responsive while response bytes stop. Check a non-streaming request before changing the registry:

```sh
curl -H 'Content-Type: application/json' -d '{"model":"<profile>/<model>","messages":[{"role":"user","content":"reply with ok"}],"stream":false}' <hub-url>/v1/chat/completions
```

For a mistral.rs row, use `--reasoning off` when the engine returns no stream data. The native worker supports context size, temperature, top-p, top-k, min-p, reasoning, multimodal projector and tokenizer model flags from `extra_flags`.

A native probe does not start a model. It does not replace the health check or the request fallback rules.

## The probe

The probe runs in the background, never on the request path. Its loop always runs: the first pass
starts 30 seconds after the hub starts, then one pass every 5 minutes. Each pass reads the live
config, so a pass does nothing while `LLM_HUB_LOCAL_TUNER` is off, and turning the tuner on in
`.env` takes effect at the next pass without a restart. Turning it off keeps the last measured
speeds in memory, so the guard keeps using them until the hub restarts.

Each pass checks every enabled profile that is local: a profile with a managed `COMMAND`, or one
whose base URL points to `localhost` or a loopback address. It skips a managed profile whose
process is not running, and it never starts one.

For each local model, the probe reads two kinds of evidence:

- **The hub's own request rows** of the last 7 days. It takes the time to first token and the
  uncached prompt tokens (prompt tokens minus cache-read tokens) of each successful request that
  recorded a time to first token. Keep-warm pings do not count.
- **The llama-swap state**, read through the profile's base URL without its trailing `/v1`:
  - `GET /running` lists the loaded models and the full command line each one runs with. When
    a flag repeats, the last value wins, as in llama-server.
  - `GET /upstream/<model>/props` gives the context size (`n_ctx`). The probe reads it only for a
    model that `/running` lists as `ready`, because llama-swap loads any model named in an
    `/upstream` call, and that load would swap out the model you are using.
  - `GET /logs` shows whether llama-server logged that a model does not support
    `--cache-reuse`. The probe reads it only when at least one model is `ready`, and searches only
    the first 1 MiB. The log does not say which model, so a match counts for every ready model of
    that server.

  When `/running` answers 404, the server is a plain llama-server, and the probe reads its
  `GET /props` instead. Each read times out after 5 seconds. A failed read skips that piece of
  evidence and logs at `debug`.

The rules take the context size from column 5 of the model's [registry](#the-model-registry) row
when that row exists and column 5 holds a number, else from `/props`. The `/props` of a plain llama-server counts only when
the profile has one model.

The probe knows a model's command line only while `/running` lists it as `ready`. For any other
model, and for every model of a plain llama-server, it skips the proposals that add flags
(`kv_cache_q8` and `cache_reuse_unsupported`), because it would guess at the flags.

### The prefill fit

Prefill time grows faster than the prompt: attention costs more per token as the context grows.
One average speed underestimates a long prompt, so the probe fits a curve instead:

```text
prefill_ms ≈ a·n + b·n²     (n = uncached prompt tokens, a ≥ 0, b ≥ 0)
```

The probe sorts the samples into size buckets (up to 2k, 4k, 8k, 16k, 32k, 64k and 128k tokens,
plus one bucket above that). It fits the curve by least squares on the median of each bucket. A
fit is **confident** with at least 3 samples in total, spread over at least 2 buckets. With a
single bucket, the fit is linear only (`b = 0`) and low confidence. The guard and the time-based
rules ignore a low-confidence fit. The guard also ignores the fit for a prompt more than 8 times
the largest prompt it measured.

## The prefill guard

The guard is off by default. Turn it on per profile with `LLM_HUB_<NAME>_PREFILL_GUARD`:

| Value | Budget |
|---|---|
| `off`, `false`, `no`, `0` or unset | The guard is off. |
| `on`, `true`, `yes` | `LLM_HUB_<NAME>_FIRST_BYTE_TIMEOUT_MS` when it is set and above 0, else `LLM_HUB_<NAME>_TIMEOUT_MS` when it is above 0. With neither, the config load fails. |
| A number above 0 | The budget in ms. |
| Anything else | The config load fails. |

The hub cannot see the client's own timeout. Set a number when your client gives up before the
profile's timeout, for example `300000` for a client that cancels after 5 minutes.

Before the hub runs a fallback chain, it estimates the prompt size and asks the fit how long each
guarded model would need to prefill it. The check reads memory only.

The guard has a warm-prefix rule. llama-server keeps the last prompt it served in its cache, so a
later turn of a long agent session pays only for its new tokens. The hub remembers the prompt size
of the last successful request each guarded model served, from any client. When that request
finished within the last 30 minutes, and the new prompt is at least as large, the guard counts
only the time for the difference: the fit's time for the full prompt minus its time for the
remembered one. The hub does not check that the new prompt starts with the old one. A request from
another client in between replaces the remembered size.

When the expected time exceeds the budget, the hub skips the model:

- The next model in the [fallback chain](fallbacks-and-transforms.md#fallbacks) serves the
  request.
- With no model left, the client gets one fast 400. Its message names the prompt size, the
  expected prefill time, the measured speed and the budget, for example
  `local/my-model: prompt ~94k tokens, expected ~21 min to prefill at 73 tok/s (measured); budget 5 min. Use a smaller prompt or another model`.
  After a warm prefix, the message also names the new tokens, for example
  `prompt ~94k tokens (~6k new tokens after a warm prefix)`. When other models of the chain were
  skipped too, the message lists every reason, separated by `;`.

A refusal that changes which model serves the request, or that causes the 400, adds a `guard`
notification (severity `warning`), at most one per profile and model every 10 minutes. The hub keeps that 10-minute window in memory only. The guard lets every request
through for a model without a confident fit.

## Rules

The rules run after each probe pass and write to the Notifications page with the source
`local_tuner`. A **proposal** carries a flag diff and offers Approve and Dismiss. An **advisory**
offers Dismiss only and changes nothing.

Rules that compare time against a budget use the profile's prefill guard budget. Without a guard,
they use `LLM_HUB_<NAME>_FIRST_BYTE_TIMEOUT_MS` when it is above 0, else
`LLM_HUB_<NAME>_TIMEOUT_MS`. With none of them, those rules stay quiet.

| Rule | Kind | Fires when | Change |
|---|---|---|---|
| `kv_cache_q8` | proposal (warning) | `--cache-type-k` (`-ctk`) or `--cache-type-v` (`-ctv`) is missing from the effective command line, set without a value, or is `f16`. | Appends `--cache-type-k q8_0 --cache-type-v q8_0`. A `q8_0` KV cache needs about half the memory of `f16`, so a larger context fits. |
| `cache_reuse_unsupported` | proposal (warning) | The effective `--cache-reuse` is above 0, and the llama-swap log says a model does not support cache reuse. Without that log line, the rule proposes nothing. | Appends `--cache-reuse 0`. A shared template may set the flag earlier in the command, and the last value wins. |
| `ctx_too_large` | proposal (warning) | A confident fit says a prompt that fills the model's context needs longer than the budget. | Sets the context size to the largest multiple of 4,096 tokens that fits the budget, but never below 8,192. The rule stays quiet when that is not smaller than the current size. |
| `tool_schema_heavy` | advisory (info) | Tool definitions make up more than 50% of the prompt bytes of the model's successful requests in the last 7 days. The hub records prompt shapes in the SQLite store only, so with `LLM_HUB_STORE=json` this rule never fires. | Advice: use fewer MCP tools, or the client's bare mode. |
| `harness_advice` | advisory (info) | For one client with at least 5 requests and a measured median prompt (failed calls that report no prompt tokens are left out of the median): a confident fit says the client's median prompt needs longer than the budget, or at least 30% of the client's requests to the model failed. A request counts as failed when it answered 5xx, 408 or 499, or succeeded without a first byte and without output tokens. Requests without a client signature do not count. | Advice: send a smaller prompt, use a client that sends this model smaller prompts, or switch to another local model whose confident fit meets the budget. One notification per client and model. |
| `does_not_fit` | advisory (warning) | A confident fit says a 20,000-token prompt (a bare agent prompt) needs longer than the budget. | Verdict: use the model for short chat only, or remove it. When it fires, the rule skips `harness_advice` for that model. |

A proposal or advisory keeps one id per rule, profile and model (and client, for
`harness_advice`): `tuner:<rule>:<profile>/<model>[:<client>]`. A later probe pass refreshes the
body of a pending notification under that id. It never revives one you dismissed or applied, even
when its numbers change. The `model` field of a tuner notification holds the bare model name,
without the profile.

## The model registry

Approve edits a pipe-delimited model registry, not the llama-swap `config.yaml`. Your own tooling
generates `config.yaml` from the registry, one entry per row. Set the registry path per profile
with `LLM_HUB_<NAME>_TUNER_REGISTRY` (`~/` is expanded). Without it, the rules still raise
proposals, but Approve answers 409 and changes nothing.

Each row has 7 or 8 columns:

```text
id|repo|file|size_gb|ctx|quant|extra_flags[|bin_override]
```

| Column | Meaning |
|---|---|
| 1 `id` | The llama-swap model id. Approve finds the row whose `id` equals the model. |
| 5 `ctx` | The context size. `ctx_too_large` sets it. |
| 7 `extra_flags` | Flags that go at the end of the server command line. Approve appends to it. |

Lines that start with `#`, and blank lines, are comments. The hub keeps every comment, every
other row and every column it does not edit byte for byte, line endings included. When the hub
changes column 7, it rewrites that column with one space between flags.

## Approval

Approving a proposal does this:

1. The hub takes the profile's apply lock. While another approval for the same profile runs, a
   second one answers 409 and changes nothing, so try it again later.
2. It marks the notification `approved`.
3. It edits the model's row (only the first row with that `id`):
   - It removes each flag the diff removes, together with its values.
   - It appends the diff's flags to column 7, unless the column already ends with them.
   - It sets column 5 when the diff changes the context size.

   A row with fewer than 7 columns, a diff value that holds `|` or a line break, or a model with
   no row stops the edit.
4. When the row changed, it copies the original file to `<file>.bak-<unix_seconds>`, or to
   `<file>.bak-<unix_seconds>-1`, `-2`, … when that backup already exists. It then writes the new
   file atomically: a temporary file in the same directory, synced, then renamed over the
   original, with the original's permissions. When the registry path is a symlink, the hub edits
   the link's target, writes the backup beside the target, and keeps the link. When nothing
   changed, the hub writes neither file.
5. It runs `LLM_HUB_<NAME>_TUNER_APPLY_COMMAND` when it is set, also when the row was already up
   to date. A failed edit skips the command. The hub splits the value like a shell (quotes work,
   `~/` is expanded), but runs the program directly, without a shell. The hub kills the command
   after `LLM_HUB_<NAME>_TUNER_APPLY_TIMEOUT_MS` (default 2 minutes).
6. It marks the notification `applied`, or `failed` when the edit failed, or the command exited
   non-zero or timed out. The `result` field holds the edit error, or a one-line summary of the
   edit (with the backup path when the row changed) followed by the last 2,000 characters of the command's output,
   stdout and stderr together. The route answers 200 in both cases.

The hub restarts nothing itself. The apply command owns that: it regenerates the llama-swap
config from the registry and restarts what needs a restart.

## Example

A local llama-swap profile with a 5-minute guard budget and an apply command:

```sh
LLM_HUB_LOCAL_TUNER=true
LLM_HUB_PERSISTENT=true

LLM_HUB_LOCAL_BASE_URL=http://127.0.0.1:8080/v1
LLM_HUB_LOCAL_PREFILL_GUARD=300000
LLM_HUB_LOCAL_TUNER_REGISTRY=/path/to/models.registry
LLM_HUB_LOCAL_TUNER_APPLY_COMMAND='/path/to/sync-models --restart'
LLM_HUB_LOCAL_TUNER_APPLY_TIMEOUT_MS=180000
```

A registry row before approval of `kv_cache_q8`:

```text
my-model|org/my-model-GGUF|my-model-Q4_K_M.gguf|18|65536|Q4_K_M|--jinja --cache-reuse 256
```

The same row after approval. The hub also wrote `/path/to/models.registry.bak-<unix_seconds>`:

```text
my-model|org/my-model-GGUF|my-model-Q4_K_M.gguf|18|65536|Q4_K_M|--jinja --cache-reuse 256 --cache-type-k q8_0 --cache-type-v q8_0
```

The profile keys are env-only. Saving the profile on the Profiles page keeps them.
