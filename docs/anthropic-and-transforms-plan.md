# Anthropic Messages endpoint + request/response transforms — implementation plan

Design document. No code in this repo has been changed by writing it.

Scope: five features, delivered as four ordered packages.

| # | Feature | Package |
|---|---|---|
| 1 | `POST /v1/messages` — Anthropic Messages API, non-streaming | **P1** |
| 1 | `POST /v1/messages` — streaming (Anthropic SSE event sequence) | **P2** |
| 2 | System-prompt trim (`x-llm-hub-system-prompt-mode`) | **P3** |
| 4 | Stream role injection | **P3** |
| 5 | Reasoning strip (`x-llm-hub-reasoning-strip`) | **P3** |
| 3 | Tool-name truncation + restoration | **P4** |
| 6 | Extra: TTFC (time-to-first-chunk) stat + `x-llm-hub-transforms` response header | **P3/P4** |

---

## 0. The two architectural decisions everything else follows from

### 0.1 Transforms run on the OpenAI-shaped body, *after* Anthropic→OpenAI translation

The task requires the system-prompt trim to apply to "BOTH the anthropic route's `system` field AND
chat/completions system/developer messages". The naive reading is two implementations. It is one, if
the seam is placed correctly:

```
POST /v1/messages   ──► anthropic::request::to_openai(Value) ──┐
                                                               ├─► transforms::apply_request(&mut Value, &HeaderMap)
POST /v1/chat/completions (and every other /v1/*) ─────────────┘        │
                                                                        ▼
                                                            attempt_loop (unchanged: chain, retry,
                                                            timeout, per-attempt re-serialize, stats)
```

Because the Anthropic `system` field becomes an OpenAI `system` message during translation, and
Anthropic `tools[]` become OpenAI `function` tools, **one implementation of each transform covers both
routes**. `transforms.rs` never learns that the Anthropic route exists.

This also satisfies the "ONE new module applied at a single seam" constraint literally: there is
exactly one call site, one line, inside `attempt_loop`.

### 0.2 The Anthropic route reuses `attempt_loop` — it does not get its own loop

Considered and rejected: a separate `anthropic_attempt_loop`. The fallback chain, retry policy,
per-attempt timeout, per-attempt model rewrite, failure recording, attempts trail and stats
recording are ~100 lines that would have to be duplicated and would drift. The three things that
genuinely differ are all parameters, not control flow:

1. **Body translation** — happens *once*, before the loop.
2. **Upstream path** — a fixed `/v1/chat/completions` instead of the incoming path. One `let`.
3. **Response transform** — a value passed to `build_response`.

The Anthropic request body already carries `model` at the top level with the same
`<profile>/<model>` meaning, so `parse_model_or_400`, `body_for_attempt`, the fallback chain and the
`x-llm-hub-*` control headers all work verbatim. `attempt_loop` gains **one parameter**
(`route: RouteKind`); everything else is untouched.

Consequence worth stating: an Anthropic-SDK client automatically gets `X-LLM-Hub-Fallbacks`,
`X-LLM-Hub-Retry-On`, `X-LLM-Hub-Timeout-Ms`, `X-LLM-Hub-Model` and `X-LLM-Hub-Attempts` for free.

### 0.3 Corollary: transforms are header-driven only, never profile-driven

Bifrost's `reasoningstrip` keys off the model name (`contains("glm")`). The hub must not, and the
reason is structural, not stylistic: the transform seam runs **once, before the chain**, while the
profile is only known **per attempt**. A profile-conditional transform would have to re-run and
re-serialize inside the loop, and — worse — the primary and the fallback would then receive
*different request bodies*, making the `x-llm-hub-attempts` trail no longer describe one comparable
request. Header-driven keeps every attempt byte-identical.

This is the documented answer to feature 5's "pick ONE": **explicit header**, consistent with
feature 2.

---

## 1. New files

| Path | Contents |
|---|---|
| `src/services/transforms.rs` | **The single seam.** `RouteKind`, `TransformPlan`, `apply_request()`, `response_transform()`, and the three body transforms that live here: system-prompt trim, reasoning strip, stream-role decision. Delegates tool-name work to `tool_names.rs`. |
| `src/services/tool_names.rs` | `NameMap`, `short_name()`, `truncate_in_body()`, quoted-restore helpers. Split out purely on size (~250 lines); `transforms.rs` remains the only caller and the only seam. |
| `src/services/sse.rs` | `SseFrames` — an incremental `\n\n`-delimited frame splitter, plus `parse_data_frame()` / `format_event()`. Shared by the SSE rewriter and the Anthropic stream translator. |
| `src/services/body_transform.rs` | `BodyTransform` enum (`Sse` / `AnthropicStream` / `Buffered`) with `push` / `finish` / `observed_usage`. The one place a response body is allowed to be rewritten. |
| `src/services/anthropic.rs` | Barrel: `pub mod request; pub mod response; pub mod stream;` (mirrors `src/configs.rs` barrelling `configs/`). |
| `src/services/anthropic/request.rs` | Anthropic Messages → OpenAI chat-completions request translation. |
| `src/services/anthropic/response.rs` | OpenAI chat completion → Anthropic message; OpenAI error → Anthropic error; `map_stop_reason`. |
| `src/services/anthropic/stream.rs` | `AnthropicStream` — the OpenAI-SSE → Anthropic-SSE state machine. |
| `src/routes/anthropic.rs` | `POST /v1/messages` handler. Reads the body capped, rejects oversized/multipart, strips Anthropic-only request headers, calls `attempt_loop(.., RouteKind::Anthropic)`. |
| `tests/anthropic_messages.rs` | HTTP-level integration tests (non-stream + stream + fallback interop). Copies the `HubProcess` / `start_hub` / `wait_ready` harness, per current convention. |
| `tests/transforms.rs` | HTTP-level integration tests for P3/P4 (header-driven transforms end-to-end against wiremock). |

**Deliberately not created:** typed structs for the Anthropic/OpenAI wire shapes. The entire existing
proxy path is `serde_json::Value`-based (`body_for_attempt`, `inject_stream_usage`, `scrape_usage`);
there is not one wire-shape struct in the crate today. Value-based translation also tolerates the
fields Anthropic adds every few months without a serde churn. Typed state is used only where state is
genuinely needed: `AnthropicStream`, `ToolCallState`, `NameMap`, `TransformPlan`.

## 2. Modified files (minimal diff description)

| File | Change |
|---|---|
| `src/services.rs` | +5 lines: `pub mod anthropic; pub mod body_transform; pub mod sse; pub mod tool_names; pub mod transforms;` |
| `src/routes.rs` | +1 line: `pub mod anthropic;` |
| `src/main.rs` | +1 route on `protected`, placed **before** the `/v1/{*path}` catch-all and before the `.layer(...)` auth call: `.route("/v1/messages", post(routes::anthropic::messages))`. Nothing else. |
| `src/consts.rs` | +~12 consts (below). No existing const changes. |
| `src/routes/proxy.rs` | Four edits, described below. Net ~35 added lines, no deletions of behavior. |
| `src/services/stats.rs` | `RequestOutcome` gains `ttfc_ms: Option<u64>`; `EntryStats` gains a second histogram; `EntrySnapshot` gains `ttfc_p50_ms` / `ttfc_p95_ms`. **In-memory only** — `UsageRow` and the sqlite/JSON store are untouched, so there is no store migration. |
| `src/configs/hub_config.rs` | +1 field `stream_role_inject: bool` (env `LLM_HUB_STREAM_ROLE`, default `true`) + its parse line + one unit test. |
| `README.md` | New "Anthropic Messages endpoint" and "Request transforms" sections; three new rows in the header table and one in the env table. |

### 2.1 `src/routes/proxy.rs` — the exact edits

**(a)** `proxy()` — one changed call, passing the route kind:

```rust
None => attempt_loop(&state, parts.method, &path_and_query, &parts.headers, buffered,
                     RouteKind::OpenAi).await,
```

**(b)** `attempt_loop` becomes `pub(crate)`, gains `route: RouteKind`, and gains three statements.
Everything else in the function is byte-identical:

```rust
    let mut json_body: Option<Value> = /* unchanged */;

    // NEW — translate first so everything downstream sees exactly one shape.
    if route == RouteKind::Anthropic {
        json_body = Some(anthropic::request::to_openai(json_body.as_ref())?);
    }

    let primary = /* unchanged */;
    let primary_id = parse_model_or_400(&config, &primary)?;
    inject_stream_usage(&mut json_body);

    // NEW — the single transform seam (features 2, 3, 4, 5).
    let plan = transforms::apply_request(&mut json_body, headers);

    // NEW — the Anthropic route always targets chat/completions.
    let upstream_path = match route {
        RouteKind::OpenAi => path_and_query,
        RouteKind::Anthropic => ANTHROPIC_UPSTREAM_PATH,
    };
```

`send_attempt(...)` inside the loop takes `upstream_path` instead of `path_and_query`.
`build_response(...)` takes two more arguments: `route` and `&plan`.

**(c)** `build_response` gains one parameter and one branch in the unfold:

```rust
fn build_response(
    state: &AppState,
    upstream: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    transform: Option<BodyTransform>,   // NEW — `None` is byte-for-byte today's path
) -> Response
```

Inside the existing `unfold`:

```rust
let out = match transform.as_mut() {
    Some(t) => Ok(t.push(&bytes)),        // may be empty; that is legal for a stream
    None    => chunk,                     // unchanged path, no clone, no copy
};
```

and on stream end, before sending the tail: `if let Some(t) = transform.as_mut() { emit t.finish() }`.
The oneshot payload widens from `Vec<u8>` to a small `StreamSummary { tail, observed_usage, ttfc_ms }`.
The spawned stats task uses `observed_usage` when present and falls back to `scrape_usage(&tail)`
otherwise — so today's OpenAI path is unchanged, and the Anthropic path (which emits
`input_tokens`/`output_tokens`, invisible to `scrape_usage`) still records real numbers.

**(d)** `attempt_single` and `passthrough_oversized` pass `None` for `transform` — unchanged behavior.

`get_model_by_id` and `passthrough_oversized` are otherwise untouched.

### 2.2 `src/consts.rs` additions

```rust
pub const HEADER_SYSTEM_PROMPT_MODE: &str = "x-llm-hub-system-prompt-mode";
pub const HEADER_REASONING_STRIP:    &str = "x-llm-hub-reasoning-strip";
pub const HEADER_TRANSFORMS:         &str = "x-llm-hub-transforms";  // response, diagnostic

/// Cap for `x-llm-hub-system-prompt-mode: truncate`, in CHARACTERS (not bytes
/// — see the UTF-8 note in transforms.rs).
pub const SYSTEM_PROMPT_MAX_CHARS: usize = 1000;

/// OpenAI and Bedrock Converse both reject function names over 64 chars.
pub const MAX_TOOL_NAME_LEN: usize = 64;
pub const TOOL_NAME_HASH_LEN: usize = 8;
/// 64 - 8 - 1 (the '_' separator).
pub const TOOL_NAME_PREFIX_LEN: usize = MAX_TOOL_NAME_LEN - TOOL_NAME_HASH_LEN - 1;

/// Every Anthropic Messages request lands on the upstream's chat-completions
/// endpoint regardless of the client-facing path.
pub const ANTHROPIC_UPSTREAM_PATH: &str = "/v1/chat/completions";
pub const ANTHROPIC_MESSAGE_ID_PREFIX: &str = "msg_";

/// Ceiling for a transform that must buffer a whole (non-SSE) body.
pub const MAX_TRANSFORM_BUFFER_BYTES: usize = 8 * 1024 * 1024;
```

---

## 3. P1 — Anthropic Messages endpoint, non-streaming

### 3.1 Route and handler

```rust
// src/routes/anthropic.rs

/// POST /v1/messages — Anthropic Messages API over an OpenAI-compatible upstream.
///
/// # Errors
/// 400 on a malformed body, an unprefixed model, an unsupported image source,
/// or a body over `LLM_HUB_MAX_REPLAY_BYTES`; 502 when every attempt fails.
pub async fn messages(State(state): State<AppState>, req: Request) -> Result<Response, AppError>
```

Steps:

1. Reject `content-type: multipart/*` (same guard as `proxy`).
2. `read_body_capped(body, config.max_replay_bytes)`. **Overflow is a 400**, not a passthrough:
   translation needs the whole document, so there is no honest oversized mode here. Message:
   `"anthropic request body exceeds LLM_HUB_MAX_REPLAY_BYTES; raise it or use /v1/chat/completions"`.
3. `strip_anthropic_request_headers(&mut headers)` — removes `anthropic-version`, `anthropic-beta`,
   `anthropic-dangerous-direct-browser-access`. Done **here, not in `utils/headers.rs`**, so the
   existing `/v1/{*path}` passthrough (which might legitimately front a real Anthropic-shaped
   upstream) keeps forwarding them. Zero change to existing routes.
4. `attempt_loop(&state, Method::POST, "/v1/messages", &headers, body, RouteKind::Anthropic)`.

Errors from this route are rendered in **Anthropic** error shape, not OpenAI shape, via a thin
newtype at the handler boundary:

```rust
// src/schemas/anthropic_error.rs
/// Wraps an `AppError` so the Anthropic route answers in Anthropic's error
/// envelope. The Anthropic SDKs branch on `error.type`, so the OpenAI-shaped
/// `{"error":{"message","type"}}` is not interchangeable.
pub struct AnthropicError(pub AppError);
// IntoResponse -> {"type":"error","error":{"type":<mapped>,"message":<msg>}}
```

### 3.2 Request translation (`services/anthropic/request.rs`)

`pub fn to_openai(body: Option<&Value>) -> Result<Value, AppError>`

| Anthropic | OpenAI | Note |
|---|---|---|
| `model` | `model` | passed through verbatim — `attempt_loop` reads it |
| `system` (string) | first `{"role":"system","content":<s>}` message | |
| `system` (blocks) | same, `text` blocks joined with `\n\n`; non-text blocks skipped | |
| `messages[].role` | unchanged (`user` / `assistant`) | |
| content: string | `content` string | |
| content block `text` | `{"type":"text","text":…}` part | |
| content block `image`, `source.type == "base64"` | `{"type":"image_url","image_url":{"url":"data:<media_type>;base64,<data>"}}` | |
| content block `image`, `source.type == "url"` | `{"type":"image_url","image_url":{"url":<url>}}` | |
| content block `image`, any other source | **400** `"unsupported image source type: <t> (expected base64 or url)"` | the "reject" half of passthrough-or-reject |
| content block `tool_use` (assistant) | `tool_calls[{id, type:"function", function:{name, arguments: <input serialized to a JSON string>}}]` on that assistant message | |
| content block `tool_result` (user) | a separate `{"role":"tool","tool_call_id":<tool_use_id>,"content":<text>}` message | |
| content block `thinking` / `redacted_thinking` | dropped | not representable; documented |
| `tools[]` `{name, description, input_schema}` | `{"type":"function","function":{name, description, parameters: input_schema}}` | |
| `tools[]` server tools (`web_search_*`, `computer_*`, …) | skipped + `tracing::warn!` | no OpenAI equivalent |
| `tool_choice {"type":"auto"}` | `"auto"` | |
| `tool_choice {"type":"any"}` | `"required"` | |
| `tool_choice {"type":"none"}` | `"none"` | |
| `tool_choice {"type":"tool","name":X}` | `{"type":"function","function":{"name":X}}` | |
| `max_tokens` | `max_tokens` | omitted if absent (lenient, fail-open) |
| `temperature`, `top_p` | unchanged | |
| `top_k` | dropped | no OpenAI equivalent |
| `stop_sequences` | `stop` | |
| `stream` | `stream` | |
| `metadata.user_id` | `user` | |
| `thinking` | dropped | see feature 5 note |

**Message ordering rule (the non-obvious part).** Anthropic puts `tool_result` blocks inside a *user*
message; OpenAI needs them as standalone `tool` messages. For each user message containing
`tool_result` blocks: emit the `tool` messages first, in block order, then a `user` message carrying
whatever non-`tool_result` blocks remain — omitted entirely when nothing remains. For each assistant
message containing `tool_use` blocks: emit one assistant message whose `content` is the joined text
(or `null` when there is none) plus a `tool_calls` array.

Hard errors (`AppError::BadRequest`): `messages` missing or not an array; a message that is not an
object; an unsupported image source. Everything else is fail-open.

### 3.3 Response translation (`services/anthropic/response.rs`)

`pub fn to_anthropic(status: u16, body: &[u8], served_model: &str, names: &NameMap) -> Vec<u8>`

Success (`status < 400`):

```json
{ "id": "msg_<openai id, prefixed if it is not already>",
  "type": "message", "role": "assistant",
  "model": "<profile>/<model>",
  "content": [ {"type":"text","text":"…"},
               {"type":"tool_use","id":"call_…","name":"…","input":{…}} ],
  "stop_reason": "end_turn", "stop_sequence": null,
  "usage": {"input_tokens": N, "output_tokens": M} }
```

- `choices[0].message.content` → one `text` block; omitted when null or empty.
- `choices[0].message.tool_calls[]` → `tool_use` blocks in order.
  `input = serde_json::from_str(arguments).unwrap_or(json!({}))` — a malformed `arguments` string
  never errors the response (fail-open; a 200 with a degraded block beats a 502).
- `model` is the **qualified served** id, matching `x-llm-hub-model` — so a client that fell over to
  a fallback can see it in the body, not just in a header.

`map_stop_reason` (`schemas/stop_reason.rs`):

| OpenAI `finish_reason` | Anthropic `stop_reason` |
|---|---|
| `stop` | `end_turn` |
| `length` | `max_tokens` |
| `tool_calls`, `function_call` | `tool_use` |
| `content_filter` | `end_turn` |
| absent / unknown | `end_turn` |

`stop_sequence` is always `null` and `stop_reason: "stop_sequence"` is unreachable — the OpenAI wire
format does not report *which* stop string matched. Documented in the module comment rather than
faked.

Errors (`status >= 400`): `{"type":"error","error":{"type":<mapped>,"message":<upstream message>}}`,
with the upstream status preserved. Non-JSON upstream bodies become the message verbatim.

| status | anthropic error type |
|---|---|
| 400 | `invalid_request_error` |
| 401 | `authentication_error` |
| 403 | `permission_error` |
| 404 | `not_found_error` |
| 413 | `request_too_large` |
| 429 | `rate_limit_error` |
| 529 | `overloaded_error` |
| other 5xx | `api_error` |

---

## 4. P2 — Anthropic streaming

### 4.1 The SSE event-ordering contract

This is the normative part. `AnthropicStream` must emit exactly this, and must emit a **well-formed**
stream even when the upstream is truncated mid-flight.

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_…","type":"message","role":"assistant",
       "model":"<profile>/<model>","content":[],"stop_reason":null,"stop_sequence":null,
       "usage":{"input_tokens":0,"output_tokens":0}}}

  (per content block, indices 0,1,2,… assigned in emission order)
event: content_block_start
data: {"type":"content_block_start","index":I,"content_block":{"type":"text","text":""}}
   or {"type":"content_block_start","index":I,"content_block":{"type":"tool_use","id":"call_…","name":"<restored>","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":I,"delta":{"type":"text_delta","text":"…"}}
   or {"type":"content_block_delta","index":I,"delta":{"type":"input_json_delta","partial_json":"…"}}

event: content_block_stop
data: {"type":"content_block_stop","index":I}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},
       "usage":{"input_tokens":N,"output_tokens":M}}

event: message_stop
data: {"type":"message_stop"}
```

Invariants — each is a test:

1. **Exactly one `message_start`**, emitted on the first upstream `data:` frame that parses as a
   chat-completion chunk (that frame carries `id`). If the upstream stream produces zero parseable
   frames, the Anthropic stream is empty — the translator never fabricates content it did not see.
2. `usage.input_tokens` in `message_start` is `0`. OpenAI only reports usage in its terminal chunk
   (which is why `inject_stream_usage` sets `stream_options.include_usage`), so the real numbers can
   only appear in `message_delta`. Documented deviation; Anthropic clients read the final numbers
   from `message_delta`.
3. **No interleaving.** `content_block_start(I)` precedes every `content_block_delta(I)`;
   `content_block_stop(I)` precedes `content_block_start(I+1)`. Opening a new block closes the
   currently-open one. OpenAI is permitted to interleave `tool_calls` indices; Anthropic's protocol
   is not, and a client cannot represent it — so the translator serializes. Documented deviation.
4. **Text block** opens lazily on the first non-empty `choices[0].delta.content`.
5. **Tool block** opens on the first `choices[0].delta.tool_calls[k]` fragment that carries
   `function.name` (or `id`). Argument fragments seen for index `k` *before* the name arrives are
   queued in `ToolCallState::pending_args` and flushed as `input_json_delta` immediately after the
   `content_block_start`. Subsequent `function.arguments` fragments stream straight through as
   `input_json_delta.partial_json` — **verbatim, never re-parsed**, because a fragment is a partial
   JSON string by construction.
6. `tool_use.name` is passed through `NameMap::restore_str` at `content_block_start` time (P4).
7. **Exactly one `message_delta`**, on the first frame carrying a non-null `finish_reason`, or at
   `finish()` if none ever arrives. Usage numbers are whatever has been observed.
8. **Exactly one `message_stop`**, last, after any open block has been closed.
9. Upstream `data: [DONE]` is consumed and never forwarded — Anthropic streams have no `[DONE]`
   sentinel.
10. A `data:` frame that fails to parse is ignored. A parse failure never aborts the stream.
11. On `finish()` with the upstream cut mid-stream: close any open block, then emit `message_delta`
    (`stop_reason: "end_turn"`) and `message_stop`. **The emitted stream is always well-formed.**
12. Every emitted frame is `event: <type>\ndata: <json>\n\n` — the `event:` line is mandatory; the
    Anthropic SDKs dispatch on it.
13. `ping` events are not synthesized. Optional in Anthropic's protocol; omitted deliberately.

### 4.2 State

```rust
// src/services/anthropic/stream.rs
pub struct AnthropicStream {
    frames: SseFrames,                       // incremental "\n\n" splitter
    names: NameMap,
    served_model: String,
    message_id: Option<String>,
    started: bool,
    finished: bool,                          // message_delta + message_stop already emitted
    next_index: u32,
    open: Option<OpenBlock>,
    tools: HashMap<u32, ToolCallState>,      // OpenAI tool_call index -> our state
    stop_reason: Option<&'static str>,
    usage: (u64, u64),
}

enum OpenBlock { Text(u32), Tool { index: u32, openai_index: u32 } }

struct ToolCallState { id: String, name: String, block_index: Option<u32>, pending_args: String }
```

### 4.3 Content-encoding hazard (must be handled, or every transform is broken)

`reqwest` is built with `default-features = false` (no `gzip`/`brotli`), so it neither advertises nor
decodes compression. But the caller's `accept-encoding` **is** forwarded — `filter_request_headers` is
allow-by-default. Today that is harmless: the body is passed through opaquely and the client
decompresses. The moment a transform touches the body, it would be rewriting compressed bytes.

Fix, scoped so the existing path is untouched: when — and only when — a transform is active for this
request, `send_attempt` sets `accept-encoding: identity` on the upstream request, and
`build_response` strips `content-encoding` from the response headers. Passthrough requests
(`transform == None`) keep today's behavior exactly.

Mechanically this is one extra `bool` on the call: `send_attempt(..., force_identity_encoding: bool)`,
derived from `plan.wants_transform()`. This is decidable before the loop because transforms are
header-driven (§0.3).

---

## 5. P3 — the transforms module

### 5.1 The seam

```rust
// src/services/transforms.rs

/// Which client-facing API shape a request arrived in. Decides body
/// translation and the response transform; the fallback chain, retry policy,
/// timeout handling and stats are identical for both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteKind { OpenAi, Anthropic }

/// What `apply_request` did, and what the response side therefore needs.
#[derive(Debug, Default)]
pub struct TransformPlan {
    /// alias -> original. Empty in the common case, which is what keeps the
    /// default path free of any response-side work.
    pub names: NameMap,
    /// Inject `role:"assistant"` into the first SSE delta that lacks it.
    pub inject_stream_role: bool,
    /// The request asked for a stream.
    pub is_stream: bool,
    /// Human-readable trail for the `x-llm-hub-transforms` response header.
    pub applied: Vec<String>,
}

/// The single request-side seam. Mutates the OpenAI-shaped body in place and
/// returns what the response side needs. Called exactly once per request,
/// from `attempt_loop`, before the fallback chain is built.
pub fn apply_request(body: &mut Option<Value>, headers: &HeaderMap) -> TransformPlan;

/// Builds the response-side transform. `None` means byte-for-byte passthrough
/// — the default OpenAI non-streaming path must always land here.
pub fn response_transform(
    route: RouteKind, plan: &TransformPlan, served_model: &str,
    status: u16, is_sse: bool,
) -> Option<BodyTransform>;
```

Decision table for `response_transform`:

| route | SSE? | `names` empty? | result |
|---|---|---|---|
| `OpenAi` | no | yes | **`None`** — today's path, byte-identical |
| `OpenAi` | no | no | `Buffered(RestoreNames)` |
| `OpenAi` | yes | yes | `Sse { inject_role: true }` (disengages to raw after frame 1) |
| `OpenAi` | yes | no | `Sse { inject_role: true, names }` |
| `Anthropic` | no | * | `Buffered(AnthropicMessage { status, served_model, names })` |
| `Anthropic` | yes | * | `AnthropicStream { … }` |

`is_sse` comes from the upstream `content-type` starting with `text/event-stream`, not from the
request's `stream` flag — an upstream that answers a `stream: true` request with a JSON error must
not be fed to the SSE machinery.

### 5.2 Buffering rule (the streaming invariant, restated precisely)

> A transform may buffer a **non-SSE** body in full. It must never hold an **SSE** body beyond a
> single frame.

`Buffered` is only ever selected for a non-SSE response. `SseRewrite` and `AnthropicStream` hold at
most one incomplete frame. `MAX_TRANSFORM_BUFFER_BYTES` (8 MiB) caps the buffered case:

- `BufferedOp::RestoreNames` overflow → emit the raw body untransformed + `tracing::warn!`
  (fail-open: unrestored names beat a failed request).
- `BufferedOp::AnthropicMessage` overflow → an Anthropic-shaped `api_error`. Emitting an
  OpenAI-shaped body to an Anthropic client would be worse than an honest error.

### 5.3 Feature 2 — system-prompt trim

Header `x-llm-hub-system-prompt-mode`. Value parsed case-insensitively after trimming:
`none` | `truncate` | `drop`. **Anything unrecognized, or the header absent, leaves the body
untouched** — no error, no log at warn level. Fail-open and opt-in, per Bifrost.

Because the seam runs post-translation (§0.1), the implementation only ever sees OpenAI messages:

- `drop` — remove every message whose `role` is `system` or `developer`. Guard: if that would leave
  `messages` empty, the drop is skipped and a `warn!` is logged. (**Deliberate deviation from
  Bifrost**, which has no such guard; an empty `messages` array is a guaranteed upstream 400, and
  fail-open means not manufacturing one.)
- `truncate` — for each such message, truncate its text to `SYSTEM_PROMPT_MAX_CHARS` **characters**.
  String content is truncated directly; array content is truncated cumulatively across `text` parts,
  with parts past the budget removed.
- `none` — explicit no-op (useful to override a client-side default).

**UTF-8 note — the deliberate fix to Bifrost's bug.** Bifrost's `sysprompttrim` does `s[:1000]`, a
*byte* slice with no rune-boundary care, inconsistent with its own `toolnametrunc`. In Rust the same
code panics on a non-`char_boundary` index, so the port truncates by `char_indices()` — 1000
characters, not 1000 bytes. `MAX_TOOL_NAME_LEN` stays a *byte* limit because that is what the
provider actually enforces.

### 5.4 Feature 5 — reasoning strip

Header `x-llm-hub-reasoning-strip`. Truthy values (case-insensitive, trimmed): `true`, `1`, `yes`,
`on`. Anything else, and absence, is off.

When on, removes these top-level keys from the OpenAI-shaped body: `reasoning`, `reasoning_effort`,
`thinking`. Rationale, one comment in-file: local servers (llama.cpp, LM Studio) and some
OpenAI-compatible corporate gateways 400 on parameters they do not model, and the caller often cannot
remove them (an SDK adds them).

Header-driven rather than model-conditional, for the reason in §0.3.

### 5.5 Feature 4 — stream role injection

Always on, matching Bifrost's `streamrole` (the clients this fixes — LangChain's
`get_final_completion()` with `response_format` — cannot set a header). Kill switch:
`LLM_HUB_STREAM_ROLE=false`.

Four guards, all must pass — a direct port:

1. this is the **first** `data:` frame of the stream;
2. it parses as JSON with a non-empty `choices` array;
3. `choices[0].delta` exists;
4. `choices[0].delta.role` is **absent** (an existing role is never overwritten).

Then `delta.role = "assistant"`. Only `choices[0]` is touched. A missing first chunk is never
synthesized. After the first frame — and if `names` is empty — `SseRewrite` flips to raw passthrough
and stops splitting frames for the rest of the stream.

**Called out for review:** this is the one change that alters existing-route behavior without a
request header, which is in tension with the "passthrough unchanged when no new header present"
constraint. The argument for keeping it unconditional is that it is a *spec-conformance fix* (the
OpenAI spec requires `role` in the first delta) that only ever adds a field the upstream should have
sent. The `LLM_HUB_STREAM_ROLE=false` env exists so the tension can be resolved the other way without
a code change.

### 5.6 Extra (feature 6) — TTFC and the transforms header

Two low-risk additions, both borrowed from the reference scan:

**TTFC** (`gwoverhead`'s `stream_ttfc_seconds`). `build_response`'s unfold already runs per chunk;
recording `Instant` on the first chunk is ~5 lines. `RequestOutcome` gains
`ttfc_ms: Option<u64>`, `EntryStats` a second hdrhistogram, `EntrySnapshot`
`ttfc_p50_ms`/`ttfc_p95_ms`. For a llama.cpp or llama-swap upstream, where prompt-eval and model-swap
dominate, time-to-first-token is the number the user actually feels — and the existing `latency_ms`
measures to *end of stream*, so it cannot show it. **In-memory only**: `UsageRow` and the sqlite/JSON
store are not touched, so there is no store migration and no `/api/usage` change.

**`x-llm-hub-transforms`** response header, populated from `TransformPlan::applied`, e.g.
`system-prompt=truncate, tool-names=3, reasoning-strip`. Omitted entirely when nothing fired. Mirrors
the existing `x-llm-hub-attempts` affordance: the whole feature set becomes debuggable with `curl -i`
and no log access.

Explicitly deferred (listed so the reader knows they were considered, not missed): `reqenrich`
(`x-agent-*` log enrichment — no consumer here), `fallback`'s circuit breaker (a real feature, but it
changes routing behavior and belongs in its own package), vendor auth-header normalization (already
covered — `require_master_key` reads `x-api-key`), `usagelabels`' cardinality cap (already
implemented as `STATS_MAX_MODEL_KEYS`).

---

## 6. P4 — tool-name truncation

Direct port of Bifrost's `toolnametrunc`, whose algorithm is worth keeping exactly because
determinism is load-bearing.

### 6.1 Alias

```rust
// src/services/tool_names.rs

/// Deterministic 64-char-safe alias for an over-long tool name.
///
/// Names <= 64 bytes are returned unchanged. Longer ones become
/// `<prefix>_<sha256[..8]>`, where the prefix is cut at 55 bytes and walked
/// back to the nearest char boundary. Determinism is what keeps a multi-turn
/// conversation consistent with zero cross-request state, and what keeps a
/// forced `tool_choice` pointing at the same alias the `tools` array now
/// carries — for free, with no extra bookkeeping.
///
/// The char-boundary walk is not cosmetic: a split rune re-encodes as U+FFFD
/// on the next serialize, so the alias would stop byte-matching its own
/// mapping key and restoration would silently fail.
#[must_use]
pub fn short_name(name: &str) -> Cow<'_, str>;
```

`cut = TOOL_NAME_PREFIX_LEN; while !name.is_char_boundary(cut) { cut -= 1; }` — 64 bytes exactly for
ASCII, `<= 64` for multibyte.

### 6.2 Request rewriting

`pub fn truncate_in_body(body: &mut Value) -> NameMap`, over the OpenAI-shaped body (so, again, one
implementation covers both routes):

- `tools[].function.name`, with a fallback to a bare `tools[].name`.
- `messages[].tool_calls[].function.name`.
- `tool_choice` object forms: `{"type":"function","function":{"name":…}}` and
  `{"type":"function","name":…}`. String forms (`"auto"`, `"none"`, `"required"`) pass through
  untouched.

Anthropic-native shapes (`content[].type == "tool_use"`, `tool_result`) need no special handling: by
the time this runs they are already OpenAI `tool_calls` and `tool` messages.

Fail-open at every level: a non-object body, a missing key, a wrong-typed value → that item is
skipped, the rest still processed, the body otherwise byte-identical.

### 6.3 Restoration

Uniform for all four combinations (OpenAI/Anthropic × stream/non-stream): the alias→original
substitution is the **last stage of every response transform**, as a blunt quoted byte replace —
`"alias"` → `"original"`, including the surrounding JSON quotes, so a partial substring inside a
larger value is never clobbered. That single rule covers the OpenAI `tool_calls[].function.name`, the
Anthropic `tool_use.name` the translator emits, and any echo of the name elsewhere in the payload.

For streams the substitution runs **per SSE frame**, not per raw chunk — an alias can straddle a TCP
chunk boundary but never a frame boundary. This is why `SseFrames` exists and why the SSE rewriter
keeps splitting frames for the whole stream whenever `names` is non-empty.

### 6.4 Mapping storage

Bifrost devotes ~20 lines of package doc to justifying context-keyed storage over a process-global map
keyed by `x-request-id` (leaks on short-circuit/panic/timeout; two concurrent requests reusing a
client-supplied id clobber each other). **Rust gives us that property for free**: the `NameMap` is
owned by the `TransformPlan`, moved into the `BodyTransform`, moved into the response stream, and
dropped when the stream ends. There is no map, no key, no eviction, no cross-request path — so the
entire class of bug the Go comment describes cannot be expressed. Worth one comment in-file saying
exactly that, so a future reader does not reintroduce a registry.

### 6.5 Idempotency

A client echoing an alias back on the next turn: `short_name(alias)` is a no-op (64 bytes, `<= 64`),
and the alias is not a key in the fresh map, so nothing is "restored" onto it. Meanwhile the `tools[]`
entry carrying the original long name re-truncates to the *same* alias, so the two stay in sync. This
is a direct consequence of determinism, and gets its own test.

---

## 7. Test list, ordered

Every function is testable without network. Unit tests live in an in-file `#[cfg(test)] mod tests`;
HTTP-level behavior gets an integration test that spawns the real binary against wiremock. Per
existing convention, no `#[cfg(test)]` module is added to `routes/proxy.rs` or `routes/anthropic.rs`.

### P1 — Anthropic non-streaming

`src/services/anthropic/request.rs`
1. `system_string_becomes_first_system_message`
2. `system_blocks_join_with_blank_line`
3. `user_text_blocks_become_content_parts`
4. `plain_string_content_passes_through`
5. `base64_image_becomes_data_url`
6. `url_image_becomes_image_url`
7. `unsupported_image_source_is_bad_request`
8. `assistant_tool_use_becomes_tool_calls_with_string_arguments`
9. `user_tool_result_hoists_to_tool_role_message_before_remaining_text`
10. `tool_result_only_user_message_emits_no_user_message`
11. `tools_become_openai_function_tools_with_parameters`
12. `server_tool_entries_are_skipped`
13. `tool_choice_auto_any_none_and_named_map_correctly`
14. `stop_sequences_becomes_stop_and_top_k_is_dropped`
15. `thinking_and_redacted_thinking_blocks_are_dropped`
16. `model_is_preserved_verbatim_for_routing`
17. `missing_messages_is_bad_request`

`src/services/anthropic/response.rs`
18. `text_only_completion_becomes_single_text_block`
19. `tool_calls_become_tool_use_blocks_with_parsed_input`
20. `malformed_tool_arguments_degrade_to_empty_object`
21. `empty_content_emits_no_text_block`
22. `finish_reason_maps_stop_length_and_tool_calls`
23. `unknown_finish_reason_defaults_to_end_turn`
24. `usage_maps_prompt_and_completion_to_input_and_output`
25. `missing_usage_becomes_zeros`
26. `id_is_prefixed_msg_when_upstream_id_is_not`
27. `model_reports_the_qualified_served_id`
28. `openai_error_body_becomes_anthropic_error_envelope`
29. `non_json_error_body_becomes_message_verbatim`
30. `status_to_error_type_map_covers_400_401_403_404_413_429_529_5xx`

`tests/anthropic_messages.rs`
31. `messages_non_stream_round_trip` — Anthropic in, OpenAI upstream, Anthropic out
32. `messages_honours_fallback_chain_and_sets_attempts_header`
33. `messages_upstream_400_returns_anthropic_error_shape_and_status`
34. `messages_unprefixed_model_is_400_listing_profiles`
35. `messages_oversized_body_is_400_not_passthrough`
36. `anthropic_version_header_is_not_forwarded_upstream`

### P2 — Anthropic streaming

`src/services/sse.rs`
37. `splits_frames_on_blank_line`
38. `holds_partial_frame_until_terminator`
39. `handles_crlf_and_multiple_frames_in_one_chunk`
40. `format_event_emits_event_and_data_lines`

`src/services/anthropic/stream.rs`
41. `emits_message_start_exactly_once_on_first_chunk`
42. `text_only_stream_emits_full_canonical_sequence` (asserts the exact event order of §4.1)
43. `content_block_start_precedes_every_delta_for_that_index`
44. `tool_call_stream_emits_tool_use_start_then_input_json_deltas`
45. `arguments_fragments_are_forwarded_verbatim_without_reparsing`
46. `args_arriving_before_name_are_queued_and_flushed_after_start`
47. `second_tool_call_closes_the_first_block_before_opening`
48. `text_then_tool_call_assigns_indices_zero_then_one`
49. `finish_reason_emits_message_delta_once_with_usage`
50. `done_sentinel_is_consumed_not_forwarded`
51. `unparseable_frame_is_ignored_and_stream_continues`
52. `truncated_upstream_still_closes_block_and_emits_delta_and_stop`
53. `finish_is_idempotent_and_never_double_emits_message_stop`
54. `observed_usage_is_reported_for_stats`

`tests/anthropic_messages.rs`
55. `messages_stream_round_trip_emits_anthropic_events`
56. `messages_stream_sets_text_event_stream_content_type`

### P3 — transforms

`src/services/transforms.rs`
57. `mode_parses_case_insensitively_and_trims`
58. `unrecognized_or_absent_mode_leaves_body_untouched` (byte-identity assertion)
59. `drop_removes_system_and_developer_messages`
60. `drop_is_skipped_when_it_would_empty_messages`
61. `truncate_caps_string_content_at_1000_chars`
62. `truncate_is_noop_below_the_cap`
63. `truncate_respects_char_boundaries_on_multibyte_text` (the Bifrost bug, fixed)
64. `truncate_caps_array_content_cumulatively_across_text_parts`
65. `none_mode_is_an_explicit_noop`
66. `reasoning_strip_truthy_values_remove_reasoning_keys`
67. `reasoning_strip_absent_or_falsey_leaves_body_untouched`
68. `apply_request_on_a_bare_body_returns_an_empty_plan`
69. `response_transform_returns_none_for_plain_openai_non_stream`
70. `response_transform_selects_each_variant_per_the_decision_table`
71. `applied_trail_lists_only_transforms_that_fired`

`src/services/body_transform.rs`
72. `sse_rewriter_injects_role_into_first_delta_only`
73. `sse_rewriter_never_overwrites_an_existing_role`
74. `sse_rewriter_ignores_frames_without_choices_or_delta`
75. `sse_rewriter_disengages_to_raw_passthrough_after_first_frame_when_no_names`
76. `sse_rewriter_passes_done_sentinel_through`
77. `buffered_transform_emits_nothing_until_finish`
78. `buffered_transform_over_cap_falls_back_to_raw_for_name_restore`
79. `buffered_transform_over_cap_returns_anthropic_api_error_for_translation`

`tests/transforms.rs`
80. `system_prompt_drop_reaches_upstream_without_system_message`
81. `system_prompt_truncate_reaches_upstream_capped`
82. `reasoning_strip_header_removes_reasoning_effort_upstream`
83. `no_transform_headers_leaves_the_upstream_request_byte_identical`
84. `stream_role_is_injected_into_the_first_sse_chunk`
85. `stream_role_env_kill_switch_disables_injection`
86. `transform_active_request_sends_accept_encoding_identity`

### P4 — tool names

`src/services/tool_names.rs`
87. `short_name_is_identity_at_or_below_64_bytes`
88. `short_name_produces_exactly_64_bytes_for_ascii`
89. `short_name_is_deterministic_across_calls`
90. `short_name_respects_char_boundaries_and_stays_within_64_bytes`
91. `distinct_long_names_get_distinct_aliases`
92. `truncate_in_body_rewrites_tools_function_name`
93. `truncate_in_body_rewrites_bare_tools_name_fallback`
94. `truncate_in_body_rewrites_message_tool_calls`
95. `truncate_in_body_rewrites_all_three_tool_choice_object_forms`
96. `string_tool_choice_passes_through`
97. `noop_when_all_names_are_short` (byte-identity assertion)
98. `already_truncated_alias_is_idempotent`
99. `restore_bytes_replaces_quoted_alias_only`
100. `restore_bytes_leaves_a_partial_substring_inside_a_larger_value_alone`
101. `empty_map_restore_is_byte_identity`

`tests/transforms.rs`
102. `long_tool_name_is_truncated_upstream_and_restored_in_the_response`
103. `long_tool_name_is_restored_across_sse_frames_in_a_stream`
104. `tool_choice_alias_matches_the_tools_array_alias_upstream`
105. `anthropic_route_restores_tool_use_name_in_translated_output`

**Regression bar for every package:** the four existing integration tests and all ~54 unit tests keep
passing untouched, `cargo build` and `cargo test --all-targets` stay warning-free, and
`cargo clippy --all-targets -- -D warnings` is clean under the crate's
`deny(clippy::all)` / `warn(clippy::pedantic)` / `deny(rust_2018_idioms)`.

---

## 8. Clippy-pedantic notes for the implementer

The crate baseline is warning-free; these are the pedantic lints this specific change will trip if
written carelessly.

- `clippy::large_enum_variant` — `AnthropicStream` is much bigger than the other `BodyTransform`
  variants. Box it: `AnthropicStream(Box<AnthropicStream>)`.
- `clippy::missing_errors_doc` — every new `pub fn` returning `Result` needs a `/// # Errors`
  section (`to_openai`, `messages`, the `attempt_loop` re-export).
- `clippy::missing_panics_doc` — avoid the issue entirely: no `unwrap`/`expect` on parsed input, and
  no direct string slicing. Use `char_indices` for the 1000-char cap and `is_char_boundary` for the
  55-byte cut, never `&s[..n]`.
- `clippy::needless_pass_by_value` — take `&NameMap`, `&HeaderMap`, `&Value` where the value is not
  consumed.
- `clippy::must_use_candidate` — `#[must_use]` on the pure helpers (`short_name`, `map_stop_reason`,
  `NameMap::is_empty`, `TransformPlan::wants_transform`).
- `clippy::cast_possible_truncation` — block indices are `u32` and token counts `u64`; use
  `u64::try_from(...).unwrap_or(0)` rather than `as`.
- Rust 2024 idioms already in use and expected here: let-chains, let-else, `Cow<'_, str>` returns.

## 9. README additions

New header-table rows:

| Header | Direction | Values | Effect |
|---|---|---|---|
| `X-LLM-Hub-System-Prompt-Mode` | request | `none` \| `truncate` \| `drop` | Trims or drops the system prompt. `truncate` caps at 1000 characters. Unrecognized or absent = untouched. Applies to `/v1/messages` (`system`) and to `system`/`developer` messages on `/v1/chat/completions`. |
| `X-LLM-Hub-Reasoning-Strip` | request | `true` \| `1` \| `yes` \| `on` | Removes `reasoning`, `reasoning_effort` and `thinking` from the request — for upstreams that 400 on parameters they do not model. |
| `X-LLM-Hub-Transforms` | response | e.g. `system-prompt=truncate, tool-names=3` | Diagnostic: which transforms fired. Omitted when none did. |

New env row:

| `LLM_HUB_STREAM_ROLE` | `true` | Inject `role:"assistant"` into the first streamed delta when the upstream omits it (OpenAI spec requires it; some local servers skip it). |

Plus a short "Anthropic Messages endpoint" section: point `ANTHROPIC_BASE_URL` at the hub, use
`<profile>/<model>` as the model id, note that tool names over 64 characters are aliased upstream and
restored on the way back, that `count_tokens` and `GET /v1/messages` are not implemented, and that the
`x-llm-hub-*` fallback/retry/timeout headers work on this route exactly as they do on
`/v1/chat/completions`.
