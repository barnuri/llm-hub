# Copilot HydraFusion

This page records what we know about GitHub Copilot's HydraFusion, so that llm-hub can add
support later. llm-hub does not support HydraFusion today. For Copilot Auto and the hydra
classifier, see [copilot-auto.md](copilot-auto.md). llm-hub's own Smart Auto tiers also review
answers with other models. That is a separate feature, described in [smart-auto.md](smart-auto.md).

Facts below come from three sources: the open-source VS Code Copilot code, a live Copilot CLI
run on 2026-11-01, and direct calls to the Copilot API. Each section says which.

## What HydraFusion is

HydraFusion is a model that Copilot's client runtime builds from several model calls in one
turn. A turn runs in phases. One phase writes a primary answer, and later phases judge,
critique, repair or revise it. The runtime then returns one answer.

It is a different thing from hydra. hydra is the classifier behind Copilot Auto: it scores a
prompt by task type, and Auto picks one model. HydraFusion runs several models on one turn.

## The two models

| Model id | Name in the picker | Notes |
|---|---|---|
| `hydrafusion` | HydraFusion | The default tier |
| `hydrafusion-max` | HydraFusion Max | Offered only with HydraFusion v2 |

Source: `COPILOT_HYDRA_FUSION_MODEL_ID` and `COPILOT_HYDRA_FUSION_MAX_MODEL_ID` in
`src/vs/platform/agentHost/common/copilotCliConfig.ts`.

## How a turn runs

The runtime chooses a workflow pattern for each turn, then runs phases in that pattern.

| Field | Known values |
|---|---|
| Pattern | `single`, `critique`, `cascade` |
| Phase kind | `primary`, `judge`, `repair`, `draft`, `critic`, `revision`, `follow_up` |
| Role | `solver`, `judge` |
| Conversation scope | the root conversation, or an isolated review conversation |
| Presentation | inline in the root transcript, or in its own child chat |
| Status | `succeeded`, `failed`, `cancelled` |

Each phase records its own cost in nano-AIU. That cost is already part of the root turn's
billed total.

The runtime emits these session events:

- `assistant.fusion_phase_started`
- `assistant.fusion_phase_completed`
- `assistant.fusion_phase_failed`

Source: `src/vs/platform/agentHost/node/copilot/copilotFusionPhaseTelemetry.ts` (event
classification comments) and `copilotFusionProgress.ts` in the same folder.

## How it connects to Auto tiers

When Copilot's Fusion policy applies, Auto tiers map to models like this:

| Fusion tier | Routes to |
|---|---|
| `standard` | Auto tier `balance` |
| `high` | `hydrafusion`, else Auto tier `intelligence` when HydraFusion is not offered |
| `max` | `hydrafusion-max`, with no fallback |

A `fusionTier` setting replaces the older `autoTier` setting. With dynamic Auto tiers turned on,
the service sends the tier list, HydraFusion tiers included.

Source: `fusionTierRoutingIds` in
`src/vs/platform/agentHost/node/copilot/copilotManagedModelConfiguration.ts`, and
`src/vs/workbench/contrib/chat/browser/widget/input/modelPicker/modelPickerPresentation.ts`.

## Switches that control it

VS Code agent host settings (in `copilotCliConfig.ts`):

| Key | What it does |
|---|---|
| `hydraFusion` | Turns on the experimental HydraFusion model. Off by default |
| `hydraFusionV2` | Uses HydraFusion v2. Needs `hydraFusion` |
| `hydraFusionMultiRoot` | Asks the service to keep a conversation's HydraFusion root model across turns. Needs `hydraFusionV2` |
| `dynamicAutoTiers` | Starts the runtime with `DYNAMIC_AUTO_TIERS`, so Auto tiers come from the service |

Copilot CLI feature flags seen in a live log on 2026-11-01 (values for this account):

| Flag | Value |
|---|---|
| `HYDRAFUSION` | `true` |
| `HYDRAFUSION_ROLLOUT` | `false` |
| `HYDRAFUSION_PLAN_V2` | `false` |
| `HYDRAFUSION_STICKY_ROOT` | `false` |
| `HYDRAFUSION_SENT_HISTORY_ADOPTION` | `false` |
| `HYDRAFUSION_INTERRUPTED_COMMIT` | `false` |
| `HYDRAFUSION_REPAIR_WRITE_RECEIPTS` | `false` |
| `HYDRAFUSION_BACKGROUND_YIELD_DISABLED` | `false` |
| `DYNAMIC_AUTO_TIERS` | `false` |

## How to use it today

Use the Copilot CLI directly. llm-hub is not involved.

```sh
copilot --model hydrafusion
copilot --model hydrafusion-max
```

Measured on 2026-11-01 with a one-word reply (`-p "reply with exactly: ok"`):

| Model | Input tokens | AI credits |
|---|---|---|
| `hydrafusion` | 23.7k | 0.21 |
| `hydrafusion-max` | 40.4k | 20.22 |

The log of that run shows `assistant.fusion_phase_started`, `assistant.fusion_phase_completed`
and one `hydrafusion_route_failed` telemetry event. What failed in that event is not known.

## Why llm-hub cannot serve it

The Copilot API does not serve HydraFusion. On 2026-11-01, both ids returned
`400 model_not_supported` on `/responses` and on `/chat/completions`. The phases run inside the
Copilot client runtime, not on the server.

`POST /auto` accepted `"tier": "hydrafusion"` with a 200, but it picked an ordinary model
(`gpt-6.1-sol`). There is no evidence that this tier value changes anything.

## Options for a future implementation

1. **Runtime bridge.** llm-hub starts a Copilot runtime session per conversation through the
   Copilot SDK (`@github/copilot-sdk`, the package the VS Code agent host imports), with the
   model set to `hydrafusion`. It registers the caller's tools in the session. When the runtime
   asks for a tool, llm-hub returns that call to the caller as a tool request, waits for the
   result on the next request, and resumes the session. This gives the real HydraFusion. It is
   stateful, adds latency to each turn, and depends on an internal interface that can change.
2. **llm-hub's own fusion.** llm-hub runs its own pattern over ordinary Copilot models, for
   example draft, then critique, then revise, and exposes it as a model such as
   `copilot/fusion`. It works from every harness. It multiplies tokens and latency per turn, and
   its quality compared with HydraFusion is unknown.
3. **Launcher only.** An `llm-hub hydra` command that starts the Copilot CLI with
   `--model hydrafusion`. This adds no new capability over running `copilot` directly.

Before choosing, check the cost: `hydrafusion-max` cost about 100 times as much as
`hydrafusion` for the same one-word reply.

## References

VS Code (MIT license), `microsoft/vscode` on GitHub:

- `src/vs/platform/agentHost/common/copilotCliConfig.ts`: model ids, picker names, and the
  `hydraFusion*` settings
- `src/vs/platform/agentHost/node/copilot/copilotFusionPhaseTelemetry.ts`: phase events and
  their fields
- `src/vs/platform/agentHost/node/copilot/copilotFusionProgress.ts`: how the runtime presents phases
- `src/vs/platform/agentHost/node/copilot/copilotManagedModelConfiguration.ts`: Fusion tier
  routing
- `src/vs/workbench/contrib/chat/browser/widget/input/modelPicker/modelPickerPresentation.ts`:
  how the model picker labels HydraFusion
- `src/vs/platform/agentHost/common/autoModeTiers.ts`: Auto tiers in the agent host
- `extensions/copilot/src/platform/endpoint/common/autoModeTiers.ts`: Auto tier names
- `extensions/copilot/src/platform/endpoint/node/automodeService.ts` and `autoV2Fetcher.ts`: the
  `POST /auto` client

Fetch any of them with:

```sh
ghx api repos/microsoft/vscode/contents/<path> --jq .content | base64 -d
```

The Copilot agent runtime itself (`github/copilot-agent-runtime`, named in a VS Code code
comment) is not public. The phase prompts and the pattern selection rules live there.
