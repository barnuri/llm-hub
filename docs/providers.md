# Providers

A profile is one upstream. Its routing id is the profile name, and its models appear in
`/v1/models` as `<profile>/<model>`. Every per-profile variable is in
[Configuration](configuration.md#per-profile).

## OpenAI-compatible profiles

Each `LLM_HUB_<NAME>_BASE_URL` makes one profile. `<NAME>` is the profile name in upper case,
with `-` written as `_`: profile `my-proxy` reads `LLM_HUB_MY_PROXY_BASE_URL`. Profiles found
this way get lower-case names. Set `LLM_HUB_PROFILES` to choose the order, load a subset, or keep
a dashed name.

```sh
LLM_HUB_OPENAI_BASE_URL=https://api.openai.com/v1
LLM_HUB_OPENAI_API_KEY=sk-...
LLM_HUB_GROQ_BASE_URL=https://api.groq.com/openai/v1
LLM_HUB_GROQ_API_KEY=gsk_...
```

[.env.example](../.env.example) has a full example.

If an upstream has no `/v1/models`, list its models in `LLM_HUB_<NAME>_MODELS`. An upstream that
does not answer contributes its static list, or nothing, and the hub logs a warning.

## GitHub Copilot

A Copilot subscription can serve as a profile, so `copilot/<model>` appears in `/v1/models` and
works in fallback chains. Connect it in one of two ways:

- Native auth in the hub (recommended). See [GitHub Copilot](github-copilot.md).
- The [copilot-api](https://github.com/ericc-ch/copilot-api) proxy. See
  [Through copilot-api](#through-copilot-api).

The abuse-detection warning in [GitHub Copilot](github-copilot.md) applies to both ways.

## Local servers on demand

A profile can carry a `COMMAND` that the hub runs itself. A local OpenAI-compatible server
(whisper.cpp `whisper-server`, mlx-audio, a local vLLM) then runs only while you use it.

1. The first request to the profile starts the process. The hub waits until the host and port
   in `BASE_URL` accept connections. Concurrent requests share that start.
2. If something already listens on that port, the hub uses it and starts nothing.
3. If the process exits, the next request starts it again.
4. The hub stops the process after `IDLE_SECS` without requests, when `COMMAND` changes or
   leaves `.env`, and when the hub shuts down.
5. A start that fails or times out fails that attempt like an unreachable upstream, so
   [fallbacks](fallbacks-and-transforms.md#fallbacks) still apply.

Rules for `COMMAND`:

- The hub runs the program directly, with no shell. It splits the value with POSIX shell quoting
  (Windows command-line rules on Windows). A leading `~/` expands to `$HOME`, or to
  `%USERPROFILE%` when `HOME` is not set.
- Quote the value in `.env`. The `.env` parser skips an unquoted value that has spaces.
- Write the same port in `COMMAND` and in `BASE_URL`.
- Set `MODELS` to keep the model in `/v1/models` while the process is stopped.
- Only the operator sets `COMMAND`. The admin API cannot set or write it.
- `GET /api/profiles` reports `managed` and `running` per profile, never the command.
- The process writes stdout and stderr to `logs/<profile>.log`.
- Under a service, add a `PATH` prefix. See [Service PATH](service-and-updates.md#service-path).

Example: local speech-to-text with whisper.cpp.

```sh
LLM_HUB_WHISPER_BASE_URL=http://127.0.0.1:11440/v1
LLM_HUB_WHISPER_MODELS=whisper-large-v3
LLM_HUB_WHISPER_COMMAND='/usr/bin/env PATH=/opt/homebrew/bin:/usr/bin:/bin whisper-server --model ~/models/whisper/ggml-large-v3.bin --host 127.0.0.1 --port 11440 --inference-path /v1/audio/transcriptions --convert --suppress-nst --beam-size 5'
LLM_HUB_WHISPER_IDLE_SECS=900
```

```sh
curl -F model=whisper/whisper-large-v3 -F file=@a.wav http://127.0.0.1:8888/v1/audio/transcriptions
```

`whisper-server --convert` runs `ffmpeg` from `PATH`. The hub buffers multipart bodies up to
25 MB so it can rewrite the `model` field on each attempt.

On Windows, whisper.cpp releases include `whisper-server.exe`, and `--convert` needs `ffmpeg` on
`PATH`. With Windows command-line rules, a backslash is literal, double quotes group a path with
spaces, and single quotes are plain text. Wrap the value in single quotes in `.env` so the `.env`
parser keeps the backslashes:

```sh
LLM_HUB_WHISPER_COMMAND='"C:\Program Files\whisper.cpp\whisper-server.exe" --model C:\models\ggml-large-v3.bin --host 127.0.0.1 --port 11440 --inference-path /v1/audio/transcriptions --convert'
```

## Through copilot-api

copilot-api is a local server that speaks the OpenAI API to Copilot. Log in once, then point a
plain profile at it:

```sh
npx copilot-api@latest auth   # one time. copilot-api stores its own token.
```

```sh
LLM_HUB_COPILOT_API_BASE_URL=http://127.0.0.1:4141/v1
LLM_HUB_COPILOT_API_COMMAND='/usr/bin/env PATH=/opt/homebrew/bin:/usr/bin:/bin npx copilot-api@latest start --port 4141'
LLM_HUB_COPILOT_API_START_TIMEOUT_SECS=180
LLM_HUB_COPILOT_API_MODELS=gpt-4.1,claude-sonnet-4.5
```

- `COMMAND` is optional. With it, the hub starts copilot-api on demand, as in
  [Local servers on demand](#local-servers-on-demand).
- The first `npx` run downloads the package, so give `START_TIMEOUT_SECS` extra time.
- Without `COMMAND`, run `npx copilot-api@latest start --port 4141` yourself.
- Profiles through copilot-api keep Copilot's dotted Claude ids, for example
  `copilot_api/claude-sonnet-4.5`.
- Use this proxy when native auth does not work for you. [Copilot Auto](copilot-auto.md) needs
  native auth, so a copilot-api profile has no `auto` model.

## Subscriptions

llm-hub does not proxy Claude Pro/Max, ChatGPT Plus/Pro or Cursor Pro plans. See
[Subscriptions](subscriptions.md).
