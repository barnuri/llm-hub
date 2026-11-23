# Local models: known issues

Measured facts about serving local GGUF models through the `llama_swap` profile on an Apple Silicon
Mac (M4 Max, 128 GB, llama.cpp build b10217). Read this before you add a local model or debug a local
model that does not answer. The [local model tuner](local-tuner.md) measures prefill speed and proposes
flag changes. This page lists what it does not detect yet.

## Native local serving

Set `LLM_HUB_<NAME>_KIND=local` on a profile to serve registry rows in the same process pool. The hub uses `LLM_HUB_<NAME>_REGISTRY` first, or `LLM_HUB_<NAME>_TUNER_REGISTRY` when no registry key is set. It starts a model only when a request selects it. A request routes to `local/<model-id>`. The configured profile name remains in that qualified id.

Use a pipe-delimited registry row with this shape:

```text
id|repo|file|size_gb|ctx|quant|extra_flags|bin_override|engine
```

The engine is `llama` or `mistralrs`. Empty or `llama` in column 9 uses llama.cpp through `llama-server`. `mistralrs` starts the hub's `local-worker` in the current binary. That row needs the `mistralrs` cargo feature.

The pool checks `<LLM_HUB_<NAME>_MODELS_DIR>/<id>.gguf` first. It then searches the first snapshot of the row's repository in `LLM_HUB_<NAME>_HF_CACHE` for the row's `file` value.

The pool stops idle processes after `LLM_HUB_<NAME>_IDLE_SECS`. `LLM_HUB_<NAME>_MODEL_TTL_SECS` sets per-model idle overrides. A value of `0` keeps that model loaded. `LLM_HUB_<NAME>_MEMORY_GB` sets the pool's memory budget. `LLM_HUB_<NAME>_START_TIMEOUT_SECS` sets the model startup deadline.

### mistral.rs gate

A `mistralrs` row starts only after the matching model file passes a gate record. The hub reads JSONL records from `LLM_HUB_<NAME>_GATE_FILE`. A pass record must match the model id, file path, size and modification time.

Use `LLM_HUB_<NAME>_UNGATED_MODELS` for a comma-separated list of model ids that may start without a passed gate. Set `LLM_HUB_<NAME>_ALLOW_UNGATED=true` to allow every `mistralrs` row without a passed gate. Both overrides are unsafe for a model you have not checked.

A llama.cpp row does not use the mistral.rs gate.

## A stream stalls but a plain request answers

**Symptom.** The client shows no reply. The hub logs
`upstream stream error, closing translated stream: error decoding response body`, or the request ends
as HTTP 200 with an empty body. llama-swap logs `recovered from upstream disconnection during streaming`
and `no valid JSON data found in stream`. The model process sits at 0% CPU.

**Check.** Send one non-streaming request straight to the model's upstream port (llama-swap assigns
`58NN`). A reply there means the stream path is at fault, not the model or the machine. Coding agents
always stream, so they hit the stall every time.

**Fix.** Add `--reasoning off` to the model's registry row (column 7) and regenerate the llama-swap
config. For a Gemma 4 E4B model, every stream hung before the fix, and 3 of 3 streams through the hub
replied in 0.3 to 4.4 seconds after it. The cause is suspected to be thinking output in the server's
stream path, not proven. The same signature appeared earlier on qwen3-4b-2507 and muse-glimmer-30b.

## A dense 31B model cannot serve a coding agent

A dense 31B model (Gemma 4 31B) prefills about 110 tokens/s at best on this machine, and the speed
falls as the context grows (73 tokens/s at 17.8k tokens). A coding agent sends 20k to 94k tokens per
turn, so the first token takes 5 to 20+ minutes and the client gives up. Memory is not the limit: the
model uses about 19 GB of 128 GB. No server flag closes a 10x gap.

Use a dense 31B model only for short prompts. For an agent, use a mixture-of-experts model or a small
model. The prefill guard skips a model that cannot finish within its budget, once the tuner has
measured it.

## Gemma 4 does not support `--cache-reuse`

The server logs `cache_reuse is not supported by this context` for gemma4 models, so `--cache-reuse`
does nothing. Set `--cache-reuse 0` in the row. The tuner's `cache_reuse_unsupported` rule proposes
this when it reads the log line.

## Regenerating the llama-swap config restarts every model

Tools that regenerate `config.yaml` and restart llama-swap stop every loaded model. A request in flight
ends as HTTP 200 with an empty body. Regenerate when no client is using a local model.

## Adding a model

1. Download the GGUF.
2. Make the file available at the path the generated config points to. A missing file gives
   `upstream command exited prematurely` (HTTP 500) on the first request.
3. Regenerate the llama-swap config.
4. Send one non-streaming and one streaming request through the hub before you use the model in an
   agent.
