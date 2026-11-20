# Media fallback

A client such as opencode sends the whole chat history on every turn. When an earlier tool result
holds a screenshot, a text-only local model fails on every later turn with an upstream 500 such as
`image input is not supported`. Media fallback fixes this: before an attempt goes to a model that
cannot read a media part, the hub replaces that part with text. A model that can read it gets the
body unchanged.

Media fallback is off by default. Turn it on with `LLM_HUB_MEDIA_FALLBACK=true`, or with the
"Media fallback" panel on the Models page. [Configuration](configuration.md#media-fallback) lists
every variable.

## What becomes text

| Part | Request shape | Becomes |
|---|---|---|
| `image_url`, `input_image`, `image` with base64 JPEG, PNG, GIF or WebP data | chat completions, Responses, Anthropic messages | `[image: <caption>]`, from the vision model |
| The same parts with an `http(s)` URL | all three | `[image: <url>]`. The hub never fetches a URL |
| `input_audio` (base64 `data` and `format`) | chat completions, Responses | `[audio transcript: <text>]`, from the speech model |
| `file` (`file_data`), `input_file` (`file_data`), Anthropic `document` with a base64 PDF | all three | `[document <name>: <text>]`, the PDF's text layer |
| The same parts with `text/*` data, and Anthropic `text` and `content` documents | all three | `[document <name>: <text>]`, the text itself |
| `input_file` with `file_url`, Anthropic `document` with a URL | Responses, Anthropic messages | `[document: <url>]` |

The hub finds these parts at any depth, so parts inside tool results (`tool` messages,
`tool_result` blocks and `function_call_output`) are converted too. The replacement is a `text`
part (`input_text` on Responses). A document text longer than 100,000 characters is cut, and ends
with `[truncated]`.

The caption prompt is: "Describe this image factually and concisely for a reader who cannot see
it. Include any visible text verbatim." A caption has at most 300 tokens.

The vision and speech models get only the media item: one image with the caption prompt, or one
audio file. They never see the chat, the system prompt or the other parts.

## Skip instead of convert

`LLM_HUB_MEDIA_SKIP=true` (off by default) drops a media part that a text-only model cannot read,
with no model call, and leaves a `<system-reminder>` text part in its place that says the image,
audio or document was removed and asks the model to tell the user if it matters. The Models page
panel has a checkbox for it.

| `LLM_HUB_MEDIA_FALLBACK` | `LLM_HUB_MEDIA_SKIP` | A media part for a text-only model becomes |
|---|---|---|
| `false` | `false` | Unchanged. A model known to refuse images is skipped for an image request |
| `true` | `false` | Its text, or `[<kind> omitted: <reason>]` when no model is set for its kind |
| `false` | `true` | A skip reminder. No model call |
| `true` | `true` | Its text, or a skip reminder when no model is set for its kind |

A conversion that fails (timeout, error, size limit) stays an `[<kind> omitted: <reason>]` part in
both modes.

## Which models get text

A model gets text when one of these holds:

- It matches a glob in `LLM_HUB_MEDIA_FALLBACK_TEXT_ONLY`.
- Its model descriptor reports no vision support.
- It refused an image before. The hub learns this from an upstream 500 that says `image input is
  not supported`, and keeps it in the store when `LLM_HUB_PERSISTENT=true`.

A model that is not known yet and refuses an image gets the attempt once more with its media as
text, and the hub learns the refusal. Later requests convert before the first try.

Every other model gets the original body. Audio and file parts follow the same rule as images:
a model that cannot read images is taken to read no other media either.

With media fallback off, the hub refuses an image request to a known text-only model with a 400,
as before.

## Failures

Conversion is best effort. A part that cannot be converted becomes a placeholder such as
`[image omitted: timeout]`, the request goes on, and the hub logs a warning. The causes are:
a failed or empty model reply, a conversion slower than `LLM_HUB_MEDIA_FALLBACK_TIMEOUT_MS`, an
item over `LLM_HUB_MEDIA_FALLBACK_MAX_ITEM_BYTES`, more conversions than
`LLM_HUB_MEDIA_FALLBACK_MAX_ITEMS`, an unsupported type, an unset vision or speech model, and an
upstream file id, which only the upstream can read.

## PDF text

PDF text extraction is behind the `media-pdf` cargo feature, because the PDF parser adds to the
binary size. Build with `cargo build --release --features media-pdf` to get it. Without the
feature, a PDF becomes `[document omitted: PDF text extraction is not built in ...]`. A scanned PDF
has no text layer and becomes a placeholder too.

## Cache

The hub caches each text under a SHA-256 of the kind, the converting model and the encoded media.
Memory holds the 1,024 most recently used texts. With `LLM_HUB_PERSISTENT=true` the store also
keeps every one (the `media_texts` table in sqlite), so a resent history is not converted again,
even after a restart. A cache
hit costs the hash of the media and no model call. A failure is not cached, so the next request
tries again.

## Cost and logs

Captions and transcripts go through the converting model's profile, with its pooled client and
its auth, not through the hub's own HTTP port. Each call is recorded as a request with the client
`llm-hub/media-fallback`, so cost and insights include it. A local model is free.

## Limits

- Smart Auto (`hub/*`) still prefers models that read images when a request holds one. Media
  fallback applies to the model it picks and to the fallbacks.
- When an unknown model refuses an image, the hub converts the body that the `attempt` phase
  produced and sends it again. The `attempt` phase does not run a second time.
- Audio and file support is not detected per model. The text-only rule above decides.
- An `http(s)` image or document URL is never fetched.

## Request path cost

With media fallback off, the request path does one flag check. With it on, the hub scans the
parsed request body once for media parts. A request with media to a text-only model parses and
rewrites its attempt body.
