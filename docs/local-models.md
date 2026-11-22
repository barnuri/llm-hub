# Local models: known issues

Measured facts about serving local GGUF models through the `llama_swap` profile on an Apple Silicon
Mac (M4 Max, 128 GB, llama.cpp build b10217). Read this before you add a local model or debug a local
model that does not answer. The [local model tuner](local-tuner.md) measures prefill speed and proposes
flag changes. This page lists what it does not detect yet.

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
