---
name: smart-auto-improve
description: Improve llm-hub Smart Auto from private local telemetry and current public gateway docs. Use for Smart Auto tuning, excessive model switches, weak role selection, cache misses, context filters, or orchestration costs. Recommend Smart Auto Balance unless the user selected another tier.
---

# Improve Smart Auto

Use evidence to improve role selection, cache affinity, and bounded model orchestration.
Recommend Smart Auto Balance by default. Preserve an explicit tier or model selection.

## Operating boundaries

Read the repo instructions. Use the existing learn-from-gateways skill for public research.
Treat local usage data as private, including aggregates.
Read [privacy.md](references/privacy.md) before accessing telemetry.

Start with a diagnostic report. If the invocation requests improvements, implement one justified change through code-gen.
If the invocation requests research or a plan, do not implement.
Never commit, push, or publish without authorization.
Do not schedule repeated runs unless the user requests them.
Do not change unrelated providers, harness settings, or personal orch profiles.

If Smart Auto code or telemetry does not exist, report that limitation.
Do not invent endpoints or claim planned behavior is live.
Use current Auto and insights data only where the fields support the comparison.
Treat an Auto classifier label as a weak signal, not a quality label.

## Improvement procedure

1. Establish coverage and a private baseline.
2. Inspect one failure pattern against current code.
3. Research relevant public behavior through learn-from-gateways.
4. Make one evidence-backed change if authorized.
5. Validate the change and record its result privately.

### 1. Baseline

Resolve the current session through code-gen or session-slug.
Write local evidence under ignored `agent-spec/<session>/smart-auto-improve/`.
Use existing authenticated aggregate APIs first.
Avoid raw prompt, response, tool-argument, or credential logging.
If structured events are necessary, extract an explicit field allowlist locally before analysis.
Never read whole raw logs or production transcripts into context.
If no safe structured export exists, report a telemetry gap instead.

Read the API response schema before choosing fields.
Aggregate by generic work role, tier, provider type, and canonical public model.
Keep private deployment names and session correlation keys out of tracked output.
Distinguish absent cache fields from reported zero.
Keep input, cached input, cache writes, and output semantics explicit per provider.

Compute only metrics the export supports:

| Objective | Metric |
|---|---|
| Cache stability | Main-model soft/forced switches and observed cache reads before/after switches |
| Speed | Time to first byte, final-answer latency, throughput, and stage timeouts |
| Efficiency | Calls, tokens, and cost per completed task, including discarded drafts |
| Correctness | Test outcomes, verified tool results, repairs, and explicit user feedback |
| Policy | Constraint failures, unknown capacities, budget stops, and recursive orchestration attempts |

An HTTP 200 is transport success, not task correctness.
Cheap token prices alone do not establish cheap successful execution.
Model price and name do not establish quality.
Do not compare role/model outcomes across materially different work without recording that limitation.
If samples or quality labels are insufficient, recommend instrumentation rather than a score update.

### 2. Role and orchestration diagnosis

Distinguish planning, investigation, implementation, validation, and conversation.
For planning, require model capacity for the full design context.
For investigation, require tool support and evidence-handling reliability.
For simple implementation, test whether the worker received a complete plan, scoped files, and acceptance checks.
A plan or skill can reduce uncertainty. Its presence alone does not prove low difficulty.
Escalate repeated verified failures instead of permanently choosing the cheapest model.

Check cache affinity per worker and role.
Check role/tier effort selection against each model's supported effort values.
Preserve an explicit effort override. Do not silently forward an unsupported setting.
Compare actual outgoing effort with selection events and status display.
Check that Auto and Smart Auto show the actual effort once, inside the resolved-model label.
Check that the status line shows no separate effort chip for these modes. Report each gap.
Mark unknown effort as unknown.
Do not combine unrelated subagents because they share a parent session.
Check inherited tier, include/exclude rules, minimum context, and resource budgets.
Apply restrictions to every solver, judge, critic, repair, retry, and fallback.
Check local-model residency locks before enabling parallel stages.

Validate resolved-model display and private selection history in hub, orch, and ignored session documents.
Check worker isolation, event ordering, stale states, and generated Markdown boundaries.
Never insert usage history into tracked plan or task files.

Review extra calls by value delivered, not by stage count.
Skip review for work with sufficient independent verification when policy allows.
Keep judges and critics tool-free.
Never replay a side-effecting tool call to obtain another opinion.
Do not recursively orchestrate internal stages.
Do not treat a judge's self-reported confidence as calibrated probability.

### 3. Public research

Invoke learn-from-gateways with the observed generic problem.
Exclude telemetry, prompts, private model aliases, IDs, and account details from documentation queries.
Re-read public source relevant to the hypothesis.
For routing, compare Bifrost, LiteLLM, OpenRouter, and models.dev/OpenCode.
For orchestration, compare Copilot Fusion and Switchyard.
Study orch's public code patterns when the skill is available.
Do not run its dispatcher or open personal state to research its design.

### 4. Controlled improvement

State the defect, evidence, expected effect, and a measurable rejection criterion.
Change one policy dimension per experiment.
Prefer local role signals, thresholds, telemetry fixes, or constraint corrections.
Do not add remote routing classifiers, extra proxy hops, or request-time disk reads.
Keep extra inference calls confined to the tier's authorized orchestration graph.
Do not relax a hard context restriction to make a benchmark pass.

Before editing, define synthetic tests and the current baseline.
Use code-gen for implementation requests.
If the action requires approval, use the ask tool.
Do not declare a recommended change accepted without the user's decision or explicit implementation request.

### 5. Validation and learning

Run scoped tests for the touched logic.
Test ordinary sessions, distinct subagents, and orch jobs with inherited policies.
Test minimum-context and fallback restrictions at every stage.
Test cancellation, deadlines, malformed judge output, stream boundaries, and tool-call non-replay.

Measure routing computation separately from model inference.
Compare single-call tiers with the same direct upstream request.
Compare review tiers with the same model-call graph without the hub.
Report both hub overhead and additional inference latency.
The hub overhead target remains 50 ms. Review latency is not hidden inside that allowance.

Run a live inference probe only when authorized.
Do not send large paid prompts to test a claimed maximum context.
For runtime edits, follow the repo rebuild/reinstall procedure and check the served version.
For skill-only edits, validate structure and local references. Do not claim you tested runtime behavior.

Store the baseline, result, uncertainty, and sample coverage privately.
Track only public design rationale and synthetic tests in git.
A tuning result can be inconclusive. Report that outcome instead of claiming an improvement.

## Output

Report at most five findings or changes with stable codes.
Give the failure pattern, fix or recommendation, evidence level, and validation result.
Print absolute paths for generated files.
End with one concrete action unless the requested work is complete.
