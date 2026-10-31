# Copilot Auto

Copilot Auto lets Copilot pick the model for each prompt. A native Copilot profile lists it as
the model `<profile>/auto`, for example `copilot/auto`. Auto needs
[native auth](github-copilot.md#native-auth).

```sh
llm-hub claude copilot/auto
```

## Two words to know

- **Auto** is Copilot's model router. You send a prompt. Auto answers with the model that it
  thinks fits the task, and with a short reason.
- **hydra** is the task classifier behind Auto. It scores the prompt per task type: `code_gen`,
  `debugging`, `reasoning` and `tool_use`. These are scores for kinds of work, not scores for
  models. Auto picks the model from them.

## HydraFusion

HydraFusion is a different feature with a similar name: a model that Copilot's client runtime
builds from several model calls per turn. llm-hub cannot serve it. See
[hydrafusion.md](hydrafusion.md).

## What happens on a request

1. A request names `copilot/auto`.
2. On a routing turn, llm-hub calls Copilot's `POST {api}/auto` with the prompt. It sends the
   header `X-GitHub-Api-Version: 2026-08-01`.
3. The reply holds these fields:

   | Field | Meaning |
   |---|---|
   | `selected_model` | The model Auto picked. |
   | `selection_reason` | Why, for example "Auto selected GPT-6 Luna since the task has low complexity." |
   | `session_token` | A token valid for about 24 h. |
   | `hydra_scores` | The task-type scores from hydra. |
   | `multi_turn` | The schedule that decides when to route again. |

4. llm-hub sends the request to the picked model. The request carries the session token as
   `Copilot-Session-Token`. That token gives a 10% Auto discount on the picked model.
5. On turns that do not route, the conversation keeps its model and its session token.

If `/auto` fails and the conversation has no model yet, llm-hub uses the first catalog model that
serves the endpoint, with no session token.

## When llm-hub routes again

Copilot's `multi_turn` schedule decides which user turns route again. llm-hub follows it:

- After a routing turn, llm-hub skips the next `initial_skip` user turns (2).
- Each check that keeps the model doubles the skip window, up to `max_skip` (32).
- A check routes to a new model early when a hydra score moves past `escalate_threshold` times
  that task type's sigma.
- llm-hub also routes again when the session token is less than 5 minutes from expiry.
- Tool-loop follow-ups never call `/auto`. Only user turns can route.

`/auto` costs about 240-290 ms, on routing turns only.

## When the pick does not fit the caller

Auto can pick a model that the caller's API cannot serve, or whose prompt window is too small.
llm-hub then swaps the model:

1. It keeps the models that serve the endpoint and the image, if the prompt has one.
2. It drops models whose learned prompt limit is smaller than the estimated prompt. See
   [Limits learned from rejections](github-copilot.md#limits-learned-from-rejections). A model
   with no known limit always fits. When no model fits, the largest window wins.
3. It takes the model with the closest price to Auto's pick.
4. When `/models` has no prices, it maps the complexity in `selection_reason` to a family: low to
   Haiku, medium to Sonnet, high to Opus. It takes the newest model of that family.

GPT picks work from Claude Code (`/v1/messages`) without a swap, through the
[Responses bridge](github-copilot.md#models-and-endpoints).

It is not verified whether the 10% discount still applies after a swap.

## Seeing the pick

- Response headers:

  | Header | Value |
  |---|---|
  | `X-LLM-Hub-Model` | The model that served the request. |
  | `X-LLM-Hub-Auto-Pick` | Auto's own pick. |
  | `X-LLM-Hub-Auto-Reason` | Why llm-hub served the model it did, at most 200 ASCII characters. |

- `GET /api/copilot/auto/decisions?session=<id>&limit=N` returns recent decisions, newest first.
  Claude Code sends its session id in `x-claude-code-session-id`, so `session` filters one Claude
  Code session. Without `session`, the route returns all sessions. `limit` defaults to 50, with a
  range of 1 to 200. The hub keeps decisions in memory only.
- The **Copilot Auto** tab in the UI shows the same decisions.
- The harness-status-line shows `Auto -> <model> (<complexity>)`.
