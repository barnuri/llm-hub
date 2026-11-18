# Extensions

llm-hub lets you change its behavior without forking it. Extensions run in fixed request phases.
There are two ways to write one:

| Way | You write | Good for | Cost per request |
|---|---|---|---|
| [Rules](#rules) | A YAML or JSON file | Headers, body fields, error codes, blocking, fallbacks, retries | Microseconds. A body parse only when a rule reads or changes the body |
| [Rust extensions](#rust-extensions) | Rust code, built with llm-hub as a library | Custom auth, metrics, routes, middleware, anything else | A function call |

HTTP hooks for other languages are planned. See [Planned](#planned).

## Principles

1. **Off by default.** With no rules file and no Rust extensions, llm-hub behaves as before.
2. **One model.** Rules and Rust extensions run in the same phases, in the same order, with the
   same context. llm-hub's own built-in changes use the same phases.
3. **Fail loudly at load, safely at run time.** An unknown key, an action a phase cannot run, an
   unset `${NAME}` or a duplicate rule name stops startup with the file and the reason. When an
   extension panics at run time, the hub logs it and goes on with that method's default result.
   Changes the extension made before the panic stay.
4. **Streaming stays streaming.** A successful stream is never buffered. Only an error body is
   read, and only when an extension asks for it.
5. **The hub keeps the truth.** Stats, limit learning and retries read the upstream reply as it
   came. Response changes apply to what the client gets.

## Phases

| Phase | When | An extension can |
|---|---|---|
| `request` | Once per `/v1/*` request, before the hub resolves the model and fallbacks. Sees the client's headers and body as sent | Change headers and the body, set the model and fallbacks, reject |
| `attempt` | Before each upstream call: the primary model and each fallback. Sees the body sent, and the headers before the hub adds upstream auth | Change this attempt's headers and body, reject |
| `response` | When an upstream reply arrives, before the hub delivers it or tries the next fallback | Retry on the next fallback, change the status, headers or error body the client gets |
| `complete` | Each time the hub records an outcome: every failed attempt, the final reply and keep-warm pings | Log, count, export metrics |

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

A request carries typed state from `request` to `attempt` and `response`
(`ctx.state_mut().insert(value)`).

## Rules

llm-hub reads rules from the file in `LLM_HUB_EXTENSIONS_FILE`, else from `extensions.yaml`,
`extensions.yml` or `extensions.json` in its working directory. It reloads the file within a
second of a change. When the new file is invalid, it keeps the old rules and logs why.

Check a file without starting the server:

```sh
llm-hub extensions check               # the file the server would load
llm-hub extensions check my-rules.yaml
```

A rule has a unique `name`, a `phase`, an optional `match`, and one or more `actions`. Every
condition in `match` must hold. Rules run in file order. `${NAME}` in an action value takes a value
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

  # A local server that rejects OpenAI's `metadata` field.
  - name: no-metadata-local
    phase: attempt
    match: { profile: local }
    actions:
      - remove_field: /metadata
```

### Match conditions

| Key | Phases | Matches |
|---|---|---|
| `path` | all | The client path without the query, glob (`/v1/*`) |
| `method` | request | The HTTP method |
| `api` | all | `chat`, `messages`, `responses` or `other` |
| `model` | all | Glob. In `request`, the `model` field of the body. Later, the attempt's `<profile>/<model>` |
| `profile` | attempt, response | The attempt's profile |
| `header` | all | Header values, exact. In `response`, the reply headers |
| `header_absent` | all | Headers that are not set |
| `stream` | request, attempt | The body's `stream` flag |
| `status` | response | A status (`400`) or class (`5xx`) |
| `error_code` | response | `error.code` in the JSON error body |

`header` takes a map and `stream` takes `true` or `false`. Every other key takes one value or a
list, and a list matches when any value matches. A key that a phase cannot
check is a load error.

### Actions

| Action | Phases | Effect |
|---|---|---|
| `set_header`, `remove_header` | all | Request headers in `request` and `attempt`. The client's reply headers in `response` |
| `set_field`, `remove_field` | all | A JSON Pointer in the body (`/metadata/team`). In `response`, the error body only, and only when it is uncompressed JSON. `set_field` creates missing objects |
| `set_model` | request, attempt | The body's `model` field |
| `set_fallbacks` | request | The fallback chain (`x-llm-hub-fallbacks`) |
| `reject` | request, attempt | Answer with `status` (4xx or 5xx) and `message`, in the client's API format |
| `retry_next` | response | Try the next model in the chain. On the last model, deliver the reply |
| `set_status` | response | The status the client gets |

On the Anthropic Messages route, when llm-hub translates the reply, a `response` body change is not
applied, because the error body is in the upstream's format.

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

| API | Use |
|---|---|
| `Extension` | A trait. Only `name` is required. Every phase method has a no-op default, so you override only what you need, and a new phase in a later release does not break you |
| `on_request`, `on_attempt` | Return `Flow::Continue` or `Flow::reject(status, message)` |
| `wants_error_body`, `on_response` | Ask for the error body, then return `ResponseFlow::Continue` or `ResponseFlow::RetryNext` |
| `on_complete` | Profile, model, upstream status, latency, tokens and error of each recorded outcome. It has no request state |
| `Hub::routes` | Your routes under `/ext`, behind the master key like `/api` |
| `Hub::router` | Any change to the finished router, such as a `tower` layer around every route |

`llm_hub::axum` and `llm_hub::serde_json` re-export the versions llm-hub uses. The library API is new
and can change between minor versions. Pin a tag.

## Use cases

| Need | How |
|---|---|
| Add, remove or rename headers for one profile or model | Rule: `set_header`, `remove_header` |
| Drop a parameter a model rejects | Rule: `remove_field` in `attempt` |
| Map a provider's error code to the one a client expects | Rule: `set_field` in `response` |
| Block a model for some callers | Rule: `reject` in `request` |
| Default fallback chains per model | Rule: `set_fallbacks` |
| Retry on an error llm-hub does not retry | Rule: `retry_next` in `response` |
| Per-team metrics or billing export | Rust: read the team in `on_request`, keep it in request state, and count in `on_response` |
| Custom auth on the inference routes | Rust: `on_request` with `Flow::reject`. It also runs, headers only, for unbuffered bodies and `GET /v1/models/{id}` |
| An extra endpoint | Rust: `Hub::routes` |
| Access log, panic recovery or CORS on every route | Rust: `Hub::router` with a `tower` layer |

## Planned

1. **HTTP hooks.** A service in any language called in `request`, `attempt` or `complete`, on a
   pooled connection with a timeout of a few hundred milliseconds and one `fail: open|closed`
   setting that covers every failure. llm-hub never sends `Authorization` to a hook, and never
   starts a process per request.
2. **More phases.** `http_request` and `http_response` on every route, `config_change` before and
   after a profile or key is saved, and an opt-in `chunk` phase for streamed events that only Rust
   extensions can use, because a network call per event would stall the stream.
3. **Rule modes.** `mode: shadow` to log what a rule would do without doing it.
4. **WebAssembly plugins**, if people need third-party plugins they cannot build into a binary.
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
