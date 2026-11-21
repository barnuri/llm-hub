# Extensions

llm-hub lets you change its behavior without forking it. Extensions run in fixed phases. There
are three ways to write one:

| Way | You write | Good for | Cost per request |
|---|---|---|---|
| [Rules](#rules) | A YAML or JSON file | Headers, body fields, error codes, blocking, fallbacks, retries | Microseconds. A body parse only when a rule reads or changes the body |
| [HTTP hooks](#http-hooks) | A small web service in any language, listed in the same file | Policy checks, prompt guardrails, audit | One HTTP call to a pooled connection per matching request, with a timeout |
| [Rust extensions](#rust-extensions) | Rust code, built with llm-hub as a library | Custom auth, metrics, routes, middleware, anything else | A function call |

## Principles

1. **Off by default.** With no rules file and no Rust extensions, llm-hub behaves as before.
2. **One model.** Rules, hooks and Rust extensions run in the same phases, in the same order, with
   the same context. llm-hub's own built-in changes use the same phases.
3. **Fail loudly at load, safely at run time.** An unknown key, an action a phase cannot run, an
   unset `${NAME}` or a duplicate rule name stops startup with the file and the reason. When an
   extension panics at run time, the hub logs it and goes on with that method's default result.
   Changes the extension made before the panic stay.
4. **Streaming stays streaming.** A successful stream is never buffered. Only an error body is
   read, and only when an extension asks for it. No phase runs per streamed event.
5. **The hub keeps the truth.** Stats, limit learning and retries read the upstream reply as it
   came. Response changes apply to what the client gets.

## Phases

| Phase | When | An extension can |
|---|---|---|
| `http_request` | Every request on every route (`/v1/*`, `/api/*`, the UI and `/healthz`), before the hub checks the master key. Sees the method, path and headers | Change headers, reject |
| `request` | Once per `/v1/*` request, before the hub resolves the model and fallbacks. Sees the client's headers and body as sent | Change headers and the body, set the model and fallbacks, reject |
| `attempt` | Before each upstream call: the primary model and each fallback. Sees the body sent, and the headers before the hub adds upstream auth | Change this attempt's headers and body, reject |
| `response` | When an upstream reply arrives, before the hub delivers it or tries the next fallback | Retry on the next fallback, change the status, headers or error body the client gets |
| `http_response` | Every response on every route, as it leaves the hub, including a rejection | Change headers and the status |
| `complete` | Each time the hub records an outcome: every failed attempt, the final reply and keep-warm pings | Log, count, export metrics. A Rust extension reads the request state here |
| `config_change` | Before an admin call saves a setting, and after it saved | Rust only: reject the save, or audit it |

Some requests run only part of this:

- A body over `LLM_HUB_MAX_REPLAY_BYTES` streams through unbuffered, and `GET /v1/models/{id}` has
  no body. Their `request` phase sees the headers and an empty body, so `model` and `stream`
  conditions do not match them. They have no `attempt` or `response` phase.
- `GET /v1/models` runs no phase.
- On `/v1/messages`, the `request` phase sees the headers without `anthropic-version`.
- The hub always removes `authorization`, `x-api-key`, `host` and `x-llm-hub-*` headers before it
  sends a request upstream, then adds the profile's own auth. An `attempt` change to those headers
  has no effect.
- `complete` sees the upstream status, before a `response` change. It can run before the next
  fallback, so keep it fast.

Request and attempt phases run extensions in order: built-ins, then Rust extensions in the order
you add them, then the rules file. The response phase runs them in reverse, so rules see the
upstream reply before the built-ins change it. The first rejection stops a phase. In the response
phase every extension runs, and any `retry_next` wins.

A request carries typed state from `request` to `attempt`, `response` and `complete`
(`ctx.state_mut().insert(value)`, then `ctx.state().get::<T>()`). `complete` reads the state as it
stood after the last `response` phase. A value must implement `Clone`.

`config_change` runs for these admin calls: save or delete a profile, set the default fallbacks,
the Copilot price refresh, keep-warm, media fallback, create or delete an API key, and save the
Smart Auto rules. It sees the kind of change and, for a profile or key, its name. It never sees a secret.

## Built-ins

Most built-ins are sync extensions that change error replies. One built-in is async, because it
can call a model, so the hub runs it itself instead of through the sync `Extension` trait:

| Built-in | Phase | What it does |
|---|---|---|
| Media fallback | `attempt` | Off by default (`LLM_HUB_MEDIA_FALLBACK`). Before the other attempt extensions, it turns the images, audio and files of an attempt to a text-only model into text: captions from a vision model, transcripts from a speech model, PDF text. Extensions in the same phase see the text. See [Media fallback](media-fallback.md) |

## Rules

llm-hub reads the rules file from the file in `LLM_HUB_EXTENSIONS_FILE`, else from `extensions.yaml`,
`extensions.yml` or `extensions.json` in its working directory. It reloads the file within a
second of a change. When the new file is invalid, it keeps the old rules and logs why.

Check a file without starting the server:

```sh
llm-hub extensions check               # the file the server would load
llm-hub extensions check my-rules.yaml
```

The hub also reloads when a `${NAME}` the file names changes in `.env`, or when
`LLM_HUB_EXTENSIONS_FILE` points elsewhere. It waits 150 ms after a change, so it never reads a
half-written save.

A rule has a unique `name`, a `phase`, an optional `mode`, an optional `match`, and one or more
`actions`. Every condition in `match` must hold. Rules run in file order. `${NAME}` in an action value takes a value
from `.env` or the process environment.

```yaml
rules:
  # Send a team header to one profile only.
  - name: team-header
    phase: attempt
    match: { profile: groq }
    actions:
      - set_header: { x-team: "${TEAM_ID}" }

  # A model that rejects `temperature`: remove it from the body.
  - name: no-temperature-on-o-models
    phase: attempt
    match: { model: "openai/o*" }
    actions:
      - remove_field: /temperature

  # Keep a model away from one client.
  - name: no-opus-for-ci
    phase: request
    match: { header: { x-client: ci }, model: "*/claude-opus-*" }
    actions:
      - reject: { status: 403, message: "CI may not use Opus models" }

  # A fallback chain for every streamed request that names no chain itself.
  - name: default-chain
    phase: request
    match: { model: "copilot/gpt-5*", stream: true, header_absent: x-llm-hub-fallbacks }
    actions:
      - set_fallbacks: [ "copilot/gpt-5-mini", "groq/llama-3.3-70b-versatile" ]

  # Try the next model when an upstream says it is overloaded.
  - name: overloaded
    phase: response
    match: { status: 5xx, error_code: overloaded }
    actions:
      - retry_next: true

  # A gateway that answers 400 while it warms a model up.
  - name: warming
    phase: response
    match: { status: 400, error_message: "model is warming up" }
    actions:
      - retry_next: true

  # A local server that rejects OpenAI's `metadata` field.
  - name: no-metadata-local
    phase: attempt
    match: { profile: local }
    actions:
      - remove_field: /metadata
```

A rule's phase is `http_request`, `request`, `attempt`, `response` or `http_response`. A later rule
in the same phase sees the changes of an earlier one, including a model that `set_model` changed.

### Match conditions

| Key | Phases | Matches |
|---|---|---|
| `path` | all | The client path without the query, glob (`/v1/*`) |
| `method` | http_request, request, http_response | The HTTP method |
| `api` | all | `chat`, `messages`, `responses` or `other` |
| `model` | request, attempt, response | Glob. In `request`, the `model` field of the body. Later, the attempt's `<profile>/<model>` |
| `profile` | attempt, response | The attempt's profile |
| `header` | all | Header values, exact. In `response` and `http_response`, the reply headers |
| `header_absent` | all | Headers that are not set |
| `stream` | request, attempt | The body's `stream` flag |
| `status` | response, http_response | A status (`400`) or class (`5xx`) |
| `error_code` | response | `error.code` in the JSON error body |
| `error_message` | response | Part of the error message (`error.message`, a string `error`, `message` or `detail`), in any case |

`header` takes a map and `stream` takes `true` or `false`. Every other key takes one value or a
list, and a list matches when any value matches. A key that a phase cannot
check is a load error.

### Actions

| Action | Phases | Effect |
|---|---|---|
| `set_header`, `remove_header` | all | Request headers in `http_request`, `request` and `attempt`. The client's reply headers in `response` and `http_response` |
| `set_field`, `remove_field` | request, attempt, response | A JSON Pointer in the body (`/metadata/team`). In `response`, the error body only, and only when it is uncompressed JSON. `set_field` creates missing objects |
| `set_model` | request, attempt | The body's `model` field |
| `set_fallbacks` | request | The fallback chain (`x-llm-hub-fallbacks`) |
| `reject` | http_request, request, attempt | Answer with `status` (4xx or 5xx) and `message`, in the client's API format |
| `retry_next` | response | Try the next model in the chain. On the last model, deliver the reply |
| `set_status` | response, http_response | The status the client gets |
| `notify` | all | Send a notification to the Notifications inbox. See [Notifications](#notifications) |

On the Anthropic Messages route, when llm-hub translates the reply, a `response` body change is not
applied, because the error body is in the upstream's format.

### Shadow mode

Add `mode: shadow` to a rule, and the hub evaluates it and logs what it would have done, with
the rule name, the phase and its actions. It changes nothing. Roll a new `reject` rule out in
shadow mode, read the log, then remove the line.

```yaml
rules:
  - name: no-opus-for-ci
    mode: shadow
    phase: request
    match: { header: { x-client: ci }, model: "*/claude-opus-*" }
    actions:
      - reject: { status: 403, message: "CI may not use Opus models" }
```

```
INFO llm_hub::ext::rules::rules_extension: A shadow rule matched. The hub took no action. rule=no-opus-for-ci phase="request" actions=reject(403)
```

`llm-hub extensions check` prints how many rules and hooks are in shadow mode.

### Notifications

A `notify` action sends a notification to the Notifications inbox each time its rule matches, from
source `ext:rules`. It takes a `title`, an optional `body` and an optional `severity` (`info`,
`warning` or `critical`, default `warning`). `${NAME}` takes a value from the environment, as in the
other actions. In `request` the notification names the body's model, and in `attempt` and
`response` the attempt's profile and model. A shadow rule logs `notify` and sends nothing.

```yaml
rules:
  - name: overloaded-alert
    phase: response
    match: { status: 5xx, error_code: overloaded }
    actions:
      - notify: { title: "Upstream overloaded", body: "Retrying on the next model", severity: warning }
      - retry_next: true
```

The rule sends one notification per matching request. All rules share the source `ext:rules`
and its limit of 60 a minute, and the hub drops the rest, so match narrowly. The inbox, the mute rules, the severity filter and the sinks treat it like any
other notification. In `attempt` and `response` its `model` field is `<profile>/<model>`, so a
mute glob on the bare name needs a leading `*`. See [Notifications](notifications.md). With
`LLM_HUB_NOTIFICATIONS=false` the hub drops it.

## HTTP hooks

A hook is a web service that the hub calls in the `request`, `attempt` or `complete` phase. Use it
for logic that rules cannot express, or that you want in another language. Hooks sit under `hooks:`
in the same file as the rules, and reload with it.

```yaml
hooks:
  - name: guardrail
    phase: request
    url: http://127.0.0.1:9100/check
    match: { api: [chat, messages, responses] }
    send: [headers, body]   # default: headers only
    timeout_ms: 200         # default 300, at most 5000
    fail: closed            # default: open

  - name: audit
    phase: complete
    url: https://audit.example.com/llm-hub
    match: { status: [429, 5xx] }
```

| Key | Meaning |
|---|---|
| `name` | Unique among hooks |
| `phase` | `request`, `attempt` or `complete` |
| `url` | `https`, or `http` to this machine (`localhost`, `127.0.0.1`, `[::1]`). `${NAME}` takes a value from the environment |
| `match` | The match keys of the phase. For `complete`: `profile`, `model` and `status` |
| `send` | `headers`, `body` or both. A `complete` hook takes none |
| `timeout_ms` | The whole call, 1 to 5000. Default 300 |
| `fail` | `open` (default) skips the hook when a call fails. `closed` rejects the request with 503. One setting covers every failure: no connection, timeout, a status that is not 2xx, a reply over 1 MiB and an invalid reply |
| `mode` | `shadow` calls the hook and logs its verdict, and changes nothing |

Hooks run after the rules of the same phase, in file order. Each hook sees the changes of the
one before it. The hub reuses one pooled connection client, never follows a redirect, and never
sends `Authorization`, `X-Api-Key`, `Cookie` or `Proxy-Authorization` to a hook.

The hub sends one JSON object per call:

```json
{ "phase": "request", "method": "POST", "path": "/v1/chat/completions", "api": "chat",
  "model": "copilot/gpt-5", "headers": { "x-client": "ci" }, "body": { "messages": [] } }
```

An `attempt` call carries `profile`, `model` (as `<profile>/<model>`) and `upstream_path`, and no
`method`. A `complete` call carries `profile`, `model`, `status`, `latency_ms`, `ttft_ms`,
`tokens_in`, `tokens_out`, `error` and `client`.

A hook answers with one JSON object. An empty 2xx answer means `continue`:

```json
{ "action": "continue" }
{ "action": "reject", "status": 400, "message": "the prompt contains a secret" }
{ "action": "patch", "set_headers": { "x-reviewed": "1" }, "remove_headers": ["x-old"],
  "body_patch": [ { "op": "add", "path": "/metadata/team", "value": "blue" },
                  { "op": "remove", "path": "/temperature" } ] }
```

`body_patch` is a JSON Patch (RFC 6902) with the `add`, `replace` and `remove` operations. The hub
checks the answer strictly: an unknown field, an unknown operation or a bad header makes the call
fail, so a hook never passes with a feature the hub lacks. A `complete` hook runs on a spawned task
and never delays a response, and its answer is ignored. A failed `complete` call is logged, whatever
`fail` says. No hook runs per streamed event, because a network call per event would stall the
stream.

## Rust extensions

Build your own binary with llm-hub as a library. Your extensions run inside llm-hub without a
change to its code.

```toml
# Cargo.toml
[dependencies]
llm-hub = { git = "https://github.com/barnuri/llm-hub", tag = "v0.1.70" }
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.7", features = ["trace"] }
```

```rust
use llm_hub::Hub;
use llm_hub::axum::{Router, routing::get};
use llm_hub::ext::{CompleteCtx, Extension, Flow, RequestCtx};

struct TeamGate;

impl Extension for TeamGate {
    fn name(&self) -> &str {
        "team-gate"
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        if ctx.headers().contains_key("x-team") {
            Flow::Continue
        } else {
            Flow::reject(401, "the x-team header is required")
        }
    }

    fn on_complete(&self, ctx: &CompleteCtx<'_>) {
        println!("{} answered {} in {} ms", ctx.model(), ctx.status(), ctx.latency_ms());
    }
}

#[tokio::main]
async fn main() {
    Hub::new()
        .extension(TeamGate)
        .routes(Router::new().route("/status", get(|| async { "ok" })))
        .router(|router| router.layer(tower_http::trace::TraceLayer::new_for_http()))
        .run()
        .await;
}
```

`examples/custom_hub.rs` is a complete example: `cargo run --example custom_hub`.

An extension sends a notification from any phase with `ctx.notify(..)`:

```rust
use llm_hub::ext::{CompleteCtx, Extension, ExtNotification, ExtSeverity};

struct SlowAlert;

impl Extension for SlowAlert {
    fn name(&self) -> &str {
        "slow-alert"
    }

    fn on_complete(&self, ctx: &CompleteCtx<'_>) {
        if ctx.latency_ms() > 60_000 {
            ctx.notify(
                ExtNotification::new(
                    ExtSeverity::Warning,
                    "Slow reply",
                    format!("{} took {} ms", ctx.model(), ctx.latency_ms()),
                )
                .with_model(ctx.model())
                .with_profile(ctx.profile()),
            );
        }
    }
}
```

The hub stores it in the inbox with source `ext:slow-alert`, kind `custom` and a Dismiss button,
after the phase method returns. The mute rules, the severity filter
(`LLM_HUB_NOTIFY_MIN_SEVERITY`) and the sinks apply as for any notification. With
`LLM_HUB_NOTIFICATIONS=false` the hub drops it. The call never waits on a sink: the hub stores the
notification in memory, saves it on a blocking thread and queues it for the sinks. Each call adds
one inbox entry. Each extension gets 60 a minute as source `ext:<name>`, and the hub drops the
rest, so do not call it on every request.

| API | Use |
|---|---|
| `Extension` | A trait. Only `name` is required. Every phase method has a no-op default, so you override only what you need, and a new phase in a later release does not break you |
| `on_http_request`, `on_http_response` | Every route. Change headers or the status, or reject before the master key check |
| `on_request`, `on_attempt` | Return `Flow::Continue` or `Flow::reject(status, message)` |
| `wants_error_body`, `on_response` | Ask for the error body, then return `ResponseFlow::Continue` or `ResponseFlow::RetryNext` |
| `on_complete` | Profile, model, upstream status, latency, tokens, error and the request state of each recorded outcome |
| `on_config_change`, `on_config_changed` | Before and after an admin call saves a setting. A rejection stops the save |
| `ctx.notify(ExtNotification)` | Every phase context. Sends a notification to the Notifications inbox from source `ext:<name>`, where `<name>` is the extension's `name()` |
| `Hub::routes` | Your routes under `/ext`, behind the master key like `/api` |
| `Hub::router` | Any change to the finished router, such as a `tower` layer around every route |

`llm_hub::axum` and `llm_hub::serde_json` re-export the versions llm-hub uses. The library API is new
and can change between minor versions. Pin a tag.

## Turn extensions on and off

Every extension can be turned off without a change to its code or the rules file:

| Name | Kind | Phases |
|---|---|---|
| `copilot-context-code`, `payload-too-large`, `local-not-ready`, `upstream-limits` | builtin | `response` |
| `rules` | rules | The phases its rules use. Each rule can also be turned off by its own name |
| `hooks` | hook | The phases of the hooks in the rules file, all of them together |
| The `name()` of a Rust extension | library | Not listed, because the hub cannot see which methods it overrides |

There are two ways:

- **The Settings > Extensions page** (or `PUT /api/extensions/{name}` and
  `PUT /api/extensions/rules/{rule}` with `{"enabled": false}`). The hub saves the choice in its
  store, so it survives a restart when `LLM_HUB_PERSISTENT=true`. Without a store it holds the
  choice until the hub stops. `GET /api/extensions` lists every extension and rule with
  `enabled` and `disabled_by` (`env`, `ui` or `null`).
- **`LLM_HUB_EXTENSIONS_DISABLED`**, a comma list of extension names or globs, and
  `rules:<name or glob>` for single rules:

  ```sh
  LLM_HUB_EXTENSIONS_DISABLED=upstream-limits,hooks,rules:no-opus-*
  ```

  The env wins. The page shows such an entry as "off by env", and the API answers 409 to a
  change of it. The hub applies a changed list when `.env` changes.

The hub removes what is off when it builds the extension set: at startup, on a toggle, on a
rules file reload and on an env change. A request runs only the extensions that are on, so a
toggle costs a request nothing. A turned-off rule keeps its place, and turning it back on needs
no reload of the rules file. A UI choice for a name that is gone, such as a deleted rule, stays
saved and applies again when the name comes back. Media fallback is not an extension in this
list: `LLM_HUB_MEDIA_FALLBACK` turns it on and off.

## Use cases

| Need | How |
|---|---|
| Add, remove or rename headers for one profile or model | Rule: `set_header`, `remove_header` |
| Drop a parameter a model rejects | Rule: `remove_field` in `attempt` |
| Map a provider's error code to the one a client expects | Rule: `set_field` in `response` |
| Block a model for some callers | Rule: `reject` in `request` |
| Default fallback chains per model | Rule: `set_fallbacks` |
| Retry on an error llm-hub does not retry | Rule: `retry_next` in `response` |
| Block or tag a request on any route, such as `/api/*` | Rule: `reject` or `set_header` in `http_request` |
| Add a header or change a status on every response | Rule: `set_header` or `set_status` in `http_response` |
| Try a new `reject` rule without risk | Rule or hook: `mode: shadow` |
| Prompt guardrail in Python | Hook in `request`, `fail: closed`, `send: [body]` |
| Audit log or billing export | Hook in `complete`, or Rust `on_complete` |
| Alert someone about an upstream error or a slow model | Rule: `notify`, or Rust `ctx.notify(..)` |
| Per-team metrics or billing from a library build | Rust: keep the team in request state in `on_request`, read it in `on_complete` |
| Custom auth on the inference routes | Rust: `on_request` with `Flow::reject`. It also runs, headers only, for unbuffered bodies and `GET /v1/models/{id}` |
| Freeze or audit settings | Rust: `on_config_change` and `on_config_changed` |
| Rule out a built-in or a rule that misbehaves | Settings > Extensions, or `LLM_HUB_EXTENSIONS_DISABLED` |
| An extra endpoint | Rust: `Hub::routes` |
| Access log, panic recovery or CORS on every route | Rust: `Hub::router` with a `tower` layer |

## Planned

1. **A `chunk` phase** for streamed events, for Rust extensions only. A network call per event
   would stall the stream, so hooks will never get it.
2. **Hook trust.** A hash check for hook URLs loaded from a project file, as Codex does.
3. **WebAssembly plugins**, if people need third-party plugins they cannot build into a binary.
   Bifrost deprecated its WebAssembly plugins, and a call costs tens of microseconds.

## Lessons from other projects

- **Bifrost** has phase hooks, an embeddable Go core and Go shared-object plugins. Its hooks run
  only on inference routes, plugins cannot add routes, a pre-request error is only logged, shared
  objects need the exact Go version and do not work on Windows, and a plugin that misses a new
  interface method stops running without an error. llm-hub's trait methods have defaults and are
  always called, and Rust extensions can add routes and middleware.
- **LiteLLM** declares Python callbacks in its config and skips per-chunk work when no callback
  uses it. **Portkey** runs guardrail plugins before and after a request, but not on the Responses
  API, and its output checks see nothing on streams. llm-hub's phases cover every `/v1/*` API.
- **Envoy AI Gateway**, **Kong** and **APISIX** change headers and bodies from declarative config,
  and **Pingora** and **River** (Rust proxies) compile config into the same trait their code
  extensions implement. llm-hub's rules file follows that design.
- **Claude Code**, **Codex** and **opencode** hooks taught: compile matchers once at load, report
  invalid config with its file and reason, keep hook failures in one setting, and keep hooks with
  long timeouts off the request path.
