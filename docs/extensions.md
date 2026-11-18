# Extensions

> Status: proposal. Nothing on this page is built yet. The API names and file formats can change
> before the first release that ships them. Comments are welcome in the pull request or an issue.

llm-hub should let you change its behavior without forking it. This page describes how: one set
of request phases, and three ways to hook into them. Pick the simplest way that does the job.

| Way | You write | Good for | Cost per request |
|---|---|---|---|
| [Rules](#rules) | A YAML or JSON file | Headers, body fields, error codes, blocking, fallbacks | Microseconds. A body parse only when a body rule matches |
| [Hooks](#hooks) | An HTTP service in any language | Policy checks, guardrails, audit | One local HTTP call per matching request, with a timeout |
| [Rust extensions](#rust-extensions) | Rust code, built with llm-hub as a library | Custom auth, metrics, routes, anything else | A function call |

WebAssembly plugins are a possible fourth way later. See [Not planned yet](#not-planned-yet).

## Principles

1. **Off by default.** With no rules file, no hooks and no extensions, llm-hub behaves exactly as
   it does today, and it skips the extension code path entirely.
2. **One model for every way.** Rules, hooks and Rust extensions run in the same phases, in the
   same order, with the same context. Built-in features move onto the same phases over time.
3. **Every route, not only inference.** An extension can see `/v1/*`, `/api/*` and the UI routes.
   Gateways that hook only the inference path force people to wrap the whole server instead.
4. **Fail loudly at load, fail safely at run time.** An unknown key, an unknown phase or an
   invalid rule stops startup with a message that names the file and line. At run time, an
   extension that errors or panics is logged and skipped, unless it is marked `fail: closed`.
5. **Streaming stays streaming.** No phase buffers a successful stream unless an extension asks for
   chunks. Per-chunk phases cost nothing when no extension uses them.
6. **Measured.** Each extension's time is recorded per request. The hub may add at most 50 ms to
   the time to first byte, extensions included.

## Phases

Every request passes these phases in order. Request phases run extensions in their `order`.
Response phases run them in reverse, so the first extension to see the request is the last to
see the response.

| Phase | When | An extension can |
|---|---|---|
| `http_request` | Every route, before auth | Read or set headers, reject, answer directly |
| `auth` | Every route that needs a key | Accept or reject the caller, attach an identity |
| `request` | `/v1/*` after the body is parsed | Change the body and headers, pick the model, set fallbacks, reject, answer directly |
| `attempt` | Before each upstream attempt, once per fallback | Change this attempt's body and headers, skip this model |
| `response` | Upstream status and headers arrive | Retry on the next fallback, change the status or headers, rewrite an error body |
| `chunk` | Each streamed event (opt-in) | Change, drop or add an event |
| `http_response` | Every route, before the response leaves | Set response headers, read the final status |
| `complete` | After the last byte, off the request path | Log, count, export metrics |
| `config_change` | A profile, key or setting is saved | Validate before the save, react after it |

Each request carries a typed context that lives until the last byte of a stream is sent. An
extension stores its own data there in `request` and reads it back in `complete`, with no global
map keyed by a request id.

## Rules

Rules need no code. llm-hub reads them from `extensions.yaml` (or `extensions.json`) next to its
`.env`, or from the path in `LLM_HUB_EXTENSIONS_FILE`. It reloads the file when it changes. If the
new file is invalid, the hub keeps the old rules and logs why.

A rule has a `phase`, an optional `match`, and one or more `actions`. Every condition in `match`
must hold. Rules run in file order. `${NAME}` takes a value from the environment.

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

  # Copilot calls a context overflow model_max_prompt_tokens_exceeded. Codex compacts only on
  # context_length_exceeded. (llm-hub already does this one built in. It shows the shape.)
  - name: context-code
    phase: response
    match: { profile: copilot, path: /v1/responses, status: 400, error_code: model_max_prompt_tokens_exceeded }
    actions:
      - set_field: { /error/code: context_length_exceeded }

  # Keep a model away from one client.
  - name: no-premium-for-ci
    phase: request
    match: { header: { x-client: ci }, model: "*/claude-opus-*" }
    actions:
      - reject: { status: 403, message: "CI may not use Opus models" }

  # A fallback chain for every request that names no chain itself.
  - name: default-chain
    phase: request
    match: { model: "copilot/gpt-5*", header_absent: x-llm-hub-fallbacks }
    actions:
      - set_fallbacks: [ "copilot/gpt-5-mini", "groq/llama-3.3-70b-versatile" ]

  # Never forward the caller's own Authorization header to a local model.
  - name: strip-auth-local
    phase: attempt
    match: { profile: local }
    actions:
      - remove_header: authorization
```

### Match conditions

| Key | Matches | Example |
|---|---|---|
| `path` | The client path, glob | `/v1/responses`, `/api/*` |
| `method` | The HTTP method | `POST` |
| `api` | The client API shape | `chat`, `messages`, `responses` |
| `model` | The requested or attempted model, glob | `copilot/claude-*` |
| `profile` | The profile of the attempt | `copilot` |
| `header` | Header values, exact | `{ x-client: ci }` |
| `header_absent` | A header that is not set | `x-llm-hub-fallbacks` |
| `stream` | Whether the client asked for a stream | `true` |
| `status` | The upstream status (`response` phase) | `400`, `5xx` |
| `error_code` | `error.code` in the upstream error body | `rate_limit_exceeded` |

### Actions

| Action | Phases | Effect |
|---|---|---|
| `set_header`, `remove_header` | all but `complete` | Change request headers, or response headers in `response` and `http_response` |
| `set_field`, `remove_field` | `request`, `attempt`, `response` | Change a JSON body field by JSON Pointer. On `response`, only error bodies |
| `set_model` | `request`, `attempt` | Send a different model id |
| `set_fallbacks` | `request` | Replace the fallback chain |
| `reject` | `http_request`, `auth`, `request` | Answer with this status and message in the client's API format |
| `retry_next` | `response` | Treat this response as failed and try the next fallback |
| `set_status` | `response` | Change the status sent to the client |
| `tag` | all | Add a label to the request in stats and logs |

`set_field` and `remove_field` parse the body only when the rule matches. A request that no body
rule matches is forwarded as raw bytes, as today.

## Hooks

A hook is an HTTP endpoint that llm-hub calls in one phase. Use it for logic that rules cannot
express and that you want in another language or in another process.

```yaml
hooks:
  - name: guardrail
    phase: request
    url: http://127.0.0.1:9100/check
    match: { api: [chat, messages, responses] }
    send: [headers, body]    # default: headers only. Authorization is never sent.
    timeout_ms: 200
    fail: open               # open: skip the hook on error or timeout. closed: reject the request.

  - name: audit
    phase: complete          # off the request path. Never delays the response.
    url: http://127.0.0.1:9100/audit
```

llm-hub sends one JSON object and reads one back:

```json
{ "phase": "request", "request_id": "…", "path": "/v1/chat/completions", "model": "copilot/gpt-5",
  "profile": "copilot", "headers": { "x-client": "ci" }, "body": { "messages": [] } }
```

```json
{ "action": "continue" }
{ "action": "reject", "status": 400, "message": "prompt contains a secret" }
{ "action": "patch", "set_headers": { "x-reviewed": "1" }, "body_patch": [ { "op": "remove", "path": "/temperature" } ] }
```

`body_patch` is a JSON Patch (RFC 6902). Hooks reuse one pooled connection. llm-hub never starts
a process per request. A `complete` hook gets the final status, usage, latency and attempts.

## Rust extensions

To add code, build your own binary with llm-hub as a library. Your `main.rs` adds extensions and
calls `run`. The rest of llm-hub is unchanged.

```toml
# Cargo.toml
[dependencies]
llm-hub = { git = "https://github.com/barnuri/llm-hub", tag = "v0.2.0" }
tokio = { version = "1", features = ["full"] }
```

```rust
use llm_hub::ext::{Extension, Flow, Phase, RequestCtx, CompleteCtx};
use llm_hub::Hub;

/// Counts requests per model and rejects callers without a team header.
struct TeamGate;

impl Extension for TeamGate {
    fn name(&self) -> &'static str { "team-gate" }

    fn phases(&self) -> &'static [Phase] { &[Phase::Request, Phase::Complete] }

    fn on_request(&self, ctx: &mut RequestCtx) -> Flow {
        match ctx.header("x-team") {
            Some(team) => {
                ctx.state().insert(Team(team.to_owned()));
                Flow::Continue
            }
            None => Flow::reject(401, "x-team header is required"),
        }
    }

    fn on_complete(&self, ctx: &CompleteCtx) {
        let team = ctx.state().get::<Team>().map_or("unknown", |team| team.0.as_str());
        metrics::counter!("requests_by_team", "team" => team.to_owned()).increment(1);
    }
}

struct Team(String);

#[tokio::main]
async fn main() {
    Hub::from_env()
        .extension(TeamGate)
        .run()
        .await
}
```

Rules for the trait:

- **Explicit phases.** `phases()` lists the phases the extension uses. Registration fails when
  `phases()` names a phase whose method the extension does not override, or the reverse. A hook
  never stops running unnoticed after an upgrade.
- **Default methods.** Every phase method has a no-op default, so a new phase in a later llm-hub
  release does not break existing extensions.
- **Small context types.** Each phase gets only what it can use: `HttpCtx`, `RequestCtx`,
  `AttemptCtx`, `ResponseCtx`, `ChunkCtx`, `CompleteCtx`, `ConfigChangeCtx`.
- **Routes behind auth.** `Hub::route(path, handler)` mounts extra routes under `/ext/`. They sit
  behind the master key unless the extension says otherwise, so a new route is never open by
  accident.
- **Plain middleware.** `Hub::layer(tower_layer)` adds any `tower` layer around every route, for
  needs the phases do not cover.
- **Panics are caught.** A panic in a phase method is logged as that extension's error and handled
  like `fail: open`.

Rules and hooks are built on this same trait. A rule file compiles into extensions at load time.

## Use cases

These come from issues people hit when extending other gateways.

| Need | How |
|---|---|
| Add, remove or rename headers for one profile or model | Rule: `set_header`, `remove_header` |
| Drop a parameter a model rejects | Rule: `remove_field` in `attempt` |
| Map a provider's error code to the one a client expects | Rule: `set_field` in `response` |
| Block a model for some callers | Rule: `reject` in `request` |
| Default fallback chains per model | Rule: `set_fallbacks` |
| Retry on a status llm-hub does not retry | Rule: `retry_next` in `response` |
| Single sign-on or roles for the admin API | Rust: `auth` phase on `/api/*` |
| Prompt guardrail in Python | Hook in `request`, `fail: closed` |
| Audit log or billing export | Hook or Rust in `complete` |
| Per-team metrics with request-scoped labels | Rust: request context plus `complete` |
| Validate a profile before it is saved | Rust or hook in `config_change` |
| Access log and panic recovery on every route | Rust: `http_request` and `http_response`, or `Hub::layer` |
| A custom endpoint | Rust: `Hub::route` |
| Rewrite each streamed event | Rust: `chunk` |

## Lessons from other gateways

- **Bifrost** has phase hooks, compiled-in plugins, Go shared-object plugins and an embeddable Go
  core. Its plugins cannot add routes. A pre-request error is only logged and cannot block. Shared
  objects need the exact Go version and do not work on Windows. Its WebAssembly plugins are
  deprecated. Plugins are matched by interface, so a plugin that misses a new method stops running
  without an error. llm-hub takes the phases and the library model, and adds explicit phase
  registration, routes and blocking.
- **LiteLLM** declares Python callbacks and guardrails in its config, and has an HTTP guardrail API
  for other languages. Per-chunk hooks have a fast path when no callback uses them. llm-hub takes
  the HTTP hook contract and the zero-cost fast path.
- **Envoy AI Gateway** and **Kong** route and change bodies from declarative config, with small
  limits (top-level fields, short lists). llm-hub takes model matching from the body and
  JSON-field actions.
- **Pingora** and **River** (Rust proxies) define one trait with fixed phases, and turn
  declarative config into implementations of that trait. llm-hub follows that design.

## Rollout

Each step keeps the current behavior as the default and ships with tests and docs.

1. Split the crate into `lib.rs` and a thin `main.rs`. No behavior change.
2. Add the phases and the `Extension` trait inside llm-hub. Move one built-in feature onto it
   (the Copilot context code rename) to prove the design.
3. Add rules files, with load-time validation and reload.
4. Make the library API public: `Hub` builder, `Extension`, `route`, `layer`.
5. Add HTTP hooks.

## Not planned yet

- **WebAssembly plugins.** They give isolation and need no rebuild, but add a runtime to the
  binary and a cost of tens of microseconds per call. Revisit when people ask for third-party
  plugins they cannot build into a binary.
- **Dynamic libraries.** Rust has no stable ABI, so a shared object needs the same compiler and
  dependency versions as the hub. Building with llm-hub as a library avoids that.
- **An expression language** (such as CEL) in rule conditions. Start with the fixed match keys
  above, and add expressions only for needs they cannot express.
