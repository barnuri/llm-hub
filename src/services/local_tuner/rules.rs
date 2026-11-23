//! Rules that turn probe evidence into tuning proposals.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "Token counts, bytes and milliseconds stay far below 2^52, and float-to-int casts saturate"
)]

use std::collections::HashMap;

use super::prefill::PrefillFit;
use crate::schemas::notification::{
    FlagDiff, Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};

/// The `source` of every notification these rules raise.
pub const SOURCE: &str = "local_tuner";

const RULE_KV_CACHE_Q8: &str = "kv_cache_q8";
const RULE_CACHE_REUSE: &str = "cache_reuse_unsupported";
const RULE_CTX_TOO_LARGE: &str = "ctx_too_large";
const RULE_TOOL_SCHEMA_HEAVY: &str = "tool_schema_heavy";
const RULE_HARNESS_ADVICE: &str = "harness_advice";
const RULE_DOES_NOT_FIT: &str = "does_not_fit";
const RULE_STREAM_STALL: &str = "stream_stall";

const CACHE_TYPE_K: [&str; 2] = ["--cache-type-k", "-ctk"];
const CACHE_TYPE_V: [&str; 2] = ["--cache-type-v", "-ctv"];
const CACHE_REUSE: [&str; 1] = ["--cache-reuse"];
const KV_QUANT: &str = "q8_0";
const KV_UNQUANTIZED: &str = "f16";

const CTX_STEP: u64 = 4_096;
const CTX_FLOOR: u64 = 8_192;
const TOOL_SHARE_LIMIT: f64 = 0.5;
const FAILURE_SHARE_LIMIT: f64 = 0.3;
/// Fewer calls than this from one client are not enough for harness advice.
const MIN_CLIENT_REQUESTS: u64 = 5;
/// A bare agent harness prompt: a model that cannot prefill this within budget is chat-only.
const AGENT_PROMPT_TOKENS: u64 = 20_000;

const MS_PER_SECOND: f64 = 1_000.0;
const MS_PER_MINUTE: u64 = 60_000;

/// Recent traffic from one client harness to one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientStats {
    /// The client signature, for example `opencode/2.3.0`.
    pub client: String,
    pub requests: u64,
    pub median_prompt_tokens: u64,
    pub median_ttft_ms: Option<u64>,
    /// Share of requests, from 0 to 1, that failed, timed out or were cancelled.
    pub failed_or_timeout_share: f64,
}

/// Everything the rules know about one local model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleInput {
    pub profile: String,
    pub model: String,
    /// The prefill budget in milliseconds. 0 means no budget, which turns off the time rules.
    pub budget_ms: u64,
    pub fit: Option<PrefillFit>,
    /// Effective llama-server flags: flag name to its last value, `None` for a bare flag.
    pub flags: HashMap<String, Option<String>>,
    /// True when the server log shows that the model does not support `--cache-reuse`.
    pub cache_reuse_unsupported: bool,
    pub ctx: Option<u64>,
    /// Share of recent prompt bytes, from 0 to 1, that are tool definitions.
    pub tool_share: Option<f64>,
    pub clients: Vec<ClientStats>,
    /// Other local models with their prefill fits.
    pub faster_models: Vec<(String, PrefillFit)>,
    /// The native process reported a stalled response stream.
    pub stream_stalled: bool,
}

/// Runs every rule and returns pending notifications with ids `tuner:<rule>:<profile>/<model>`.
/// A "does not fit" verdict suppresses the per-client harness advice for that model.
#[must_use]
pub fn evaluate(input: &RuleInput, now_ms: u64) -> Vec<Notification> {
    let mut out: Vec<Notification> = [
        kv_cache_q8(input),
        cache_reuse_unsupported(input),
        ctx_too_large(input),
        tool_schema_heavy(input),
        stream_stall(input),
    ]
    .into_iter()
    .flatten()
    .collect();
    match does_not_fit(input) {
        Some(verdict) => out.push(verdict),
        None => out.extend(
            input
                .clients
                .iter()
                .filter_map(|c| harness_advice(input, c)),
        ),
    }
    for notification in &mut out {
        notification.created_ms = now_ms;
        notification.updated_ms = now_ms;
    }
    out
}

fn stream_stall(input: &RuleInput) -> Option<Notification> {
    if !input.stream_stalled {
        return None;
    }
    let body = format!(
        "{} has a stalled response stream. Turning reasoning off may allow coding-agent streams to answer.",
        input.model
    );
    let diff = FlagDiff {
        add: vec!["--reasoning".to_owned(), "off".to_owned()],
        ..FlagDiff::default()
    };
    Some(proposal(
        input,
        RULE_STREAM_STALL,
        "Turn off reasoning",
        body,
        diff,
    ))
}

fn kv_cache_q8(input: &RuleInput) -> Option<Notification> {
    let k = flag_value(&input.flags, &CACHE_TYPE_K);
    let v = flag_value(&input.flags, &CACHE_TYPE_V);
    if !is_unquantized(k) && !is_unquantized(v) {
        return None;
    }
    let body = format!(
        "{} runs with --cache-type-k {} and --cache-type-v {}. A {KV_QUANT} KV cache needs about \
         half the memory of {KV_UNQUANTIZED}, so a larger ctx fits.",
        input.model,
        k.unwrap_or("unset"),
        v.unwrap_or("unset"),
    );
    let diff = FlagDiff {
        add: tokens(&[CACHE_TYPE_K[0], KV_QUANT, CACHE_TYPE_V[0], KV_QUANT]),
        ..FlagDiff::default()
    };
    Some(proposal(
        input,
        RULE_KV_CACHE_Q8,
        "Quantize the KV cache",
        body,
        diff,
    ))
}

fn cache_reuse_unsupported(input: &RuleInput) -> Option<Notification> {
    let reuse = flag_value(&input.flags, &CACHE_REUSE)?
        .parse::<u64>()
        .ok()?;
    if reuse == 0 || !input.cache_reuse_unsupported {
        return None;
    }
    let body = format!(
        "{} runs with --cache-reuse {reuse}, but the server log says the model does not \
         support cache reuse. Setting it to 0 removes the warning and the wasted work.",
        input.model
    );
    let diff = FlagDiff {
        add: tokens(&[CACHE_REUSE[0], "0"]),
        ..FlagDiff::default()
    };
    Some(proposal(
        input,
        RULE_CACHE_REUSE,
        "Turn off cache reuse",
        body,
        diff,
    ))
}

fn ctx_too_large(input: &RuleInput) -> Option<Notification> {
    let ctx = input.ctx?;
    let fit = confident_fit(input.fit.as_ref())?;
    if input.budget_ms == 0 || fit.estimate_ms(ctx) <= input.budget_ms {
        return None;
    }
    let new_ctx = fit
        .max_tokens_within(input.budget_ms, CTX_STEP)
        .max(CTX_FLOOR);
    if new_ctx >= ctx {
        return None;
    }
    let body = format!(
        "A full {} token prompt on {} needs {} to prefill at {} tok/s, over the {} budget. \
         {} tokens need {}.",
        thousands(ctx),
        input.model,
        human_duration(fit.estimate_ms(ctx)),
        avg_tok_per_s(fit, ctx),
        human_duration(input.budget_ms),
        thousands(new_ctx),
        human_duration(fit.estimate_ms(new_ctx)),
    );
    let diff = FlagDiff {
        ctx: Some(new_ctx),
        old_ctx: Some(ctx),
        ..FlagDiff::default()
    };
    Some(proposal(
        input,
        RULE_CTX_TOO_LARGE,
        "Lower the context size",
        body,
        diff,
    ))
}

fn tool_schema_heavy(input: &RuleInput) -> Option<Notification> {
    let share = input.tool_share.filter(|s| *s > TOOL_SHARE_LIMIT)?;
    let body = format!(
        "{}% of recent prompt bytes for {} are tool schemas. Use fewer MCP tools or the \
         client's bare mode.",
        (share * 100.0).round() as u64,
        input.model
    );
    Some(advisory(
        input,
        RULE_TOOL_SCHEMA_HEAVY,
        None,
        NotificationSeverity::Info,
        "Prompts are mostly tool schemas",
        body,
    ))
}

fn does_not_fit(input: &RuleInput) -> Option<Notification> {
    let fit = confident_fit(input.fit.as_ref()).filter(|fit| {
        input.budget_ms > 0 && fit.estimate_ms(AGENT_PROMPT_TOKENS) > input.budget_ms
    })?;
    let body = format!(
        "{} does not fit this machine for agent use: a {} token prompt needs {} at {} tok/s. \
         Use it for short chat only, or remove it.",
        input.model,
        thousands(AGENT_PROMPT_TOKENS),
        human_duration(fit.estimate_ms(AGENT_PROMPT_TOKENS)),
        avg_tok_per_s(fit, AGENT_PROMPT_TOKENS),
    );
    Some(advisory(
        input,
        RULE_DOES_NOT_FIT,
        None,
        NotificationSeverity::Warning,
        "Model does not fit this machine",
        body,
    ))
}

fn harness_advice(input: &RuleInput, client: &ClientStats) -> Option<Notification> {
    let tokens = client.median_prompt_tokens;
    // Prompt advice needs a measured prompt and enough calls to tell a pattern from one failure.
    if tokens == 0 || client.requests < MIN_CLIENT_REQUESTS {
        return None;
    }
    let fit = confident_fit(input.fit.as_ref()).filter(|_| input.budget_ms > 0);
    let expected_ms = fit.map(|f| f.estimate_ms(tokens));
    let too_slow = expected_ms.is_some_and(|ms| ms > input.budget_ms);
    let failing = client.failed_or_timeout_share >= FAILURE_SHARE_LIMIT;
    if !too_slow && !failing {
        return None;
    }
    let mut facts = vec![format!(
        "{} sends {} a median prompt of {} tokens.",
        client.client,
        input.model,
        thousands(tokens)
    )];
    if let (Some(fit), Some(ms)) = (fit, expected_ms) {
        facts.push(format!(
            "Expected prefill is {} at {} tok/s, against a {} budget.",
            human_duration(ms),
            avg_tok_per_s(fit, tokens),
            human_duration(input.budget_ms)
        ));
    }
    if failing {
        facts.push(format!(
            "{}% of its {} requests failed or timed out.",
            (client.failed_or_timeout_share * 100.0).round() as u64,
            client.requests
        ));
    }
    let mut advice = vec![format!(
        "send a smaller prompt (fewer MCP tools or the bare mode of {})",
        client.client
    )];
    if let Some(other) = smaller_client(&input.clients, client) {
        advice.push(format!(
            "use {}, which sends this model a median of {} tokens",
            other.client,
            thousands(other.median_prompt_tokens)
        ));
    }
    if let Some((name, fit)) = fastest_model_within(input, tokens) {
        advice.push(format!(
            "switch to {name}, which prefills {} tokens in {}",
            thousands(tokens),
            human_duration(fit.estimate_ms(tokens))
        ));
    }
    let body = format!("{} Try: {}.", facts.join(" "), advice.join("; or "));
    Some(advisory(
        input,
        RULE_HARNESS_ADVICE,
        Some(&client.client),
        NotificationSeverity::Info,
        "Client prompts are too large for this model",
        body,
    ))
}

fn smaller_client<'a>(
    clients: &'a [ClientStats],
    current: &ClientStats,
) -> Option<&'a ClientStats> {
    clients
        .iter()
        .filter(|c| c.client != current.client)
        .filter(|c| c.median_prompt_tokens < current.median_prompt_tokens)
        .min_by_key(|c| c.median_prompt_tokens)
}

fn fastest_model_within(input: &RuleInput, tokens: u64) -> Option<(&str, &PrefillFit)> {
    if input.budget_ms == 0 {
        return None;
    }
    input
        .faster_models
        .iter()
        .filter(|(name, fit)| *name != input.model && !fit.low_confidence)
        .filter(|(_, fit)| fit.estimate_ms(tokens) <= input.budget_ms)
        .min_by_key(|(_, fit)| fit.estimate_ms(tokens))
        .map(|(name, fit)| (name.as_str(), fit))
}

fn confident_fit(fit: Option<&PrefillFit>) -> Option<&PrefillFit> {
    fit.filter(|f| !f.low_confidence)
}

/// The value of the first alias present in `flags`; a bare flag reads as `None`.
fn flag_value<'a>(flags: &'a HashMap<String, Option<String>>, aliases: &[&str]) -> Option<&'a str> {
    aliases
        .iter()
        .find_map(|alias| flags.get(*alias))
        .and_then(|value| value.as_deref())
}

fn is_unquantized(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.eq_ignore_ascii_case(KV_UNQUANTIZED))
}

fn tokens(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn notification_id(input: &RuleInput, rule: &str, client: Option<&str>) -> String {
    let base = format!("tuner:{rule}:{}/{}", input.profile, input.model);
    match client {
        Some(client) => format!("{base}:{client}"),
        None => base,
    }
}

fn proposal(
    input: &RuleInput,
    rule: &str,
    title: &str,
    body: String,
    diff: FlagDiff,
) -> Notification {
    let mut notification = base(input, rule, None, NotificationKind::Proposal, title, body);
    notification.severity = NotificationSeverity::Warning;
    notification.diff = Some(diff);
    notification
}

fn advisory(
    input: &RuleInput,
    rule: &str,
    client: Option<&str>,
    severity: NotificationSeverity,
    title: &str,
    body: String,
) -> Notification {
    let mut notification = base(input, rule, client, NotificationKind::Advisory, title, body);
    notification.severity = severity;
    notification
}

fn base(
    input: &RuleInput,
    rule: &str,
    client: Option<&str>,
    kind: NotificationKind,
    title: &str,
    body: String,
) -> Notification {
    Notification {
        id: notification_id(input, rule, client),
        kind,
        source: SOURCE.to_string(),
        severity: NotificationSeverity::Info,
        title: title.to_string(),
        body,
        profile: Some(input.profile.clone()),
        model: Some(input.model.clone()),
        rule: Some(rule.to_string()),
        diff: None,
        actions: Notification::default_actions(kind),
        status: NotificationStatus::Pending,
        created_ms: 0,
        updated_ms: 0,
        result: None,
        muted: false,
    }
}

/// Average prefill speed over a `tokens` prompt, so the text matches its own time estimate.
fn avg_tok_per_s(fit: &PrefillFit, tokens: u64) -> u64 {
    let ms = fit.estimate_ms(tokens).max(1) as f64;
    (tokens as f64 * MS_PER_SECOND / ms).round() as u64
}

fn human_duration(ms: u64) -> String {
    if ms < MS_PER_MINUTE {
        return format!("~{} s", (ms as f64 / MS_PER_SECOND).round() as u64);
    }
    format!("~{:.1} min", ms as f64 / MS_PER_MINUTE as f64)
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET_MS: u64 = 300_000;
    const NOW: u64 = 1_700;

    fn linear(ms_per_token: f64) -> PrefillFit {
        PrefillFit {
            a: ms_per_token,
            b: 0.0,
            low_confidence: false,
            max_tokens_seen: 20_000,
        }
    }

    fn flags(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.map(str::to_string)))
            .collect()
    }

    fn tuned_flags() -> HashMap<String, Option<String>> {
        flags(&[
            ("--cache-type-k", Some("q8_0")),
            ("--cache-type-v", Some("q8_0")),
        ])
    }

    fn client(name: &str, median: u64, failed: f64) -> ClientStats {
        ClientStats {
            client: name.into(),
            requests: 10,
            median_prompt_tokens: median,
            median_ttft_ms: None,
            failed_or_timeout_share: failed,
        }
    }

    /// A model tuned so that no rule fires.
    fn quiet() -> RuleInput {
        RuleInput {
            profile: "local".into(),
            model: "big-model".into(),
            budget_ms: BUDGET_MS,
            fit: Some(linear(10.0)),
            flags: tuned_flags(),
            ctx: Some(16_384),
            ..RuleInput::default()
        }
    }

    fn by_rule<'a>(out: &'a [Notification], rule: &str) -> Vec<&'a Notification> {
        out.iter()
            .filter(|n| n.rule.as_deref() == Some(rule))
            .collect()
    }

    fn only<'a>(out: &'a [Notification], rule: &str) -> &'a Notification {
        let found = by_rule(out, rule);
        assert_eq!(found.len(), 1, "{rule}: {out:#?}");
        found[0]
    }

    #[test]
    fn tuned_model_raises_nothing() {
        assert!(evaluate(&quiet(), NOW).is_empty());
    }

    #[test]
    fn stream_stall_proposes_reasoning_off() {
        let input = RuleInput {
            stream_stalled: true,
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_STREAM_STALL);
        assert_eq!(n.id, "tuner:stream_stall:local/big-model");
        assert_eq!(
            n.diff.as_ref().map(|d| d.add.clone()),
            Some(vec!["--reasoning".to_owned(), "off".to_owned()])
        );
    }

    #[test]
    fn no_stream_stall_keeps_the_default_quiet() {
        let input = RuleInput {
            stream_stalled: false,
            ..quiet()
        };
        assert!(by_rule(&evaluate(&input, NOW), RULE_STREAM_STALL).is_empty());
    }

    #[test]
    fn missing_kv_cache_type_proposes_q8() {
        let input = RuleInput {
            flags: flags(&[("-ctk", Some("q8_0"))]),
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_KV_CACHE_Q8);
        assert_eq!(n.id, "tuner:kv_cache_q8:local/big-model");
        assert_eq!(n.kind, NotificationKind::Proposal);
        assert_eq!(n.severity, NotificationSeverity::Warning);
        assert_eq!(n.source, SOURCE);
        assert_eq!(n.status, NotificationStatus::Pending);
        assert_eq!((n.created_ms, n.updated_ms), (NOW, NOW));
        assert_eq!(
            n.diff.as_ref().map(|d| d.add.clone()),
            Some(tokens(&[
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0"
            ]))
        );
        assert!(
            n.body
                .contains("--cache-type-k q8_0 and --cache-type-v unset")
        );
    }

    #[test]
    fn f16_or_bare_kv_cache_flag_counts_as_unquantized() {
        for value in [Some("F16"), None] {
            let input = RuleInput {
                flags: flags(&[("--cache-type-k", value), ("-ctv", Some("q8_0"))]),
                ..quiet()
            };
            assert_eq!(by_rule(&evaluate(&input, NOW), RULE_KV_CACHE_Q8).len(), 1);
        }
    }

    #[test]
    fn other_quantized_kv_cache_is_left_alone() {
        let input = RuleInput {
            flags: flags(&[("-ctk", Some("q4_0")), ("-ctv", Some("q4_0"))]),
            ..quiet()
        };
        assert!(evaluate(&input, NOW).is_empty());
    }

    #[test]
    fn cache_reuse_with_log_evidence_proposes_zero() {
        let mut input = quiet();
        input
            .flags
            .insert("--cache-reuse".into(), Some("256".into()));
        input.cache_reuse_unsupported = true;
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_CACHE_REUSE);
        assert_eq!(n.id, "tuner:cache_reuse_unsupported:local/big-model");
        assert_eq!(n.severity, NotificationSeverity::Warning);
        assert_eq!(
            n.diff.as_ref().map(|d| d.add.clone()),
            Some(tokens(&["--cache-reuse", "0"]))
        );
        assert!(n.body.contains("--cache-reuse 256"));
    }

    #[test]
    fn cache_reuse_needs_log_evidence_and_a_positive_value() {
        let mut no_evidence = quiet();
        no_evidence
            .flags
            .insert("--cache-reuse".into(), Some("256".into()));
        assert!(evaluate(&no_evidence, NOW).is_empty());

        for value in [Some("0"), Some("abc"), None] {
            let mut input = quiet();
            input
                .flags
                .insert("--cache-reuse".into(), value.map(str::to_string));
            input.cache_reuse_unsupported = true;
            assert!(evaluate(&input, NOW).is_empty(), "{value:?}");
        }
    }

    #[test]
    fn ctx_over_budget_proposes_largest_fitting_step() {
        let input = RuleInput {
            ctx: Some(131_072),
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_CTX_TOO_LARGE);
        assert_eq!(n.id, "tuner:ctx_too_large:local/big-model");
        // 300 s at 10 ms/token fits 30,000 tokens; the last 4,096 step below is 28,672.
        assert_eq!(n.diff.as_ref().and_then(|d| d.ctx), Some(28_672));
        assert!(n.diff.as_ref().is_some_and(|d| d.add.is_empty()));
        assert_eq!(
            n.body,
            "A full 131,072 token prompt on big-model needs ~21.8 min to prefill at 100 tok/s, \
             over the ~5.0 min budget. 28,672 tokens need ~4.8 min."
        );
    }

    #[test]
    fn ctx_proposal_never_goes_below_the_floor() {
        let input = RuleInput {
            ctx: Some(65_536),
            fit: Some(linear(100.0)),
            clients: Vec::new(),
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_CTX_TOO_LARGE);
        assert_eq!(n.diff.as_ref().and_then(|d| d.ctx), Some(CTX_FLOOR));
    }

    #[test]
    fn ctx_rule_skips_without_confident_fit_budget_or_gain() {
        let low = PrefillFit {
            low_confidence: true,
            ..linear(10.0)
        };
        let cases = [
            RuleInput {
                ctx: Some(131_072),
                fit: None,
                ..quiet()
            },
            RuleInput {
                ctx: Some(131_072),
                fit: Some(low),
                ..quiet()
            },
            RuleInput {
                ctx: Some(131_072),
                budget_ms: 0,
                ..quiet()
            },
            RuleInput {
                ctx: None,
                ..quiet()
            },
            // The floor of 8,192 tokens cannot lower a ctx of 8,192.
            RuleInput {
                ctx: Some(8_192),
                fit: Some(linear(100.0)),
                ..quiet()
            },
        ];
        for input in cases {
            let out = evaluate(&input, NOW);
            assert!(by_rule(&out, RULE_CTX_TOO_LARGE).is_empty(), "{input:?}");
        }
    }

    #[test]
    fn tool_heavy_prompts_raise_an_info_advisory() {
        let input = RuleInput {
            tool_share: Some(0.734),
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_TOOL_SCHEMA_HEAVY);
        assert_eq!(n.id, "tuner:tool_schema_heavy:local/big-model");
        assert_eq!(n.kind, NotificationKind::Advisory);
        assert_eq!(n.severity, NotificationSeverity::Info);
        assert!(n.diff.is_none());
        assert_eq!(
            n.actions,
            Notification::default_actions(NotificationKind::Advisory)
        );
        assert!(
            n.body
                .starts_with("73% of recent prompt bytes for big-model")
        );
    }

    #[test]
    fn tool_share_at_or_below_half_is_quiet() {
        for share in [Some(0.5), Some(0.1), None] {
            let input = RuleInput {
                tool_share: share,
                ..quiet()
            };
            assert!(evaluate(&input, NOW).is_empty());
        }
    }

    #[test]
    fn slow_client_gets_advice_with_numbers_and_alternatives() {
        let input = RuleInput {
            clients: vec![
                client("opencode/2.3.0", 94_000, 0.4),
                client("bare-cli/1.0", 22_500, 0.0),
            ],
            faster_models: vec![
                ("big-model".into(), linear(1.0)),
                ("mid-model".into(), linear(3.0)),
                ("small-model".into(), linear(2.0)),
                (
                    "unsure-model".into(),
                    PrefillFit {
                        low_confidence: true,
                        ..linear(0.5)
                    },
                ),
                ("slow-model".into(), linear(50.0)),
            ],
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_HARNESS_ADVICE);
        assert_eq!(n.id, "tuner:harness_advice:local/big-model:opencode/2.3.0");
        assert_eq!(n.severity, NotificationSeverity::Info);
        assert_eq!(
            n.body,
            "opencode/2.3.0 sends big-model a median prompt of 94,000 tokens. Expected prefill \
             is ~15.7 min at 100 tok/s, against a ~5.0 min budget. 40% of its 10 requests \
             failed or timed out. Try: send a smaller prompt (fewer MCP tools or the bare mode \
             of opencode/2.3.0); or use bare-cli/1.0, which sends this model a median of 22,500 \
             tokens; or switch to small-model, which prefills 94,000 tokens in ~3.1 min."
        );
    }

    #[test]
    fn harness_advice_needs_a_measured_prompt_and_enough_calls() {
        let mut few = client("curl/8.7.1", 4_000, 1.0);
        few.requests = MIN_CLIENT_REQUESTS - 1;
        let input = RuleInput {
            fit: None,
            clients: vec![client("bun/1.4", 0, 0.4), few],
            ..quiet()
        };
        assert!(by_rule(&evaluate(&input, NOW), RULE_HARNESS_ADVICE).is_empty());
    }

    #[test]
    fn failing_client_without_fit_gets_advice_without_time_numbers() {
        let input = RuleInput {
            fit: None,
            clients: vec![client("opencode/2.3.0", 4_000, 0.3)],
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_HARNESS_ADVICE);
        assert_eq!(
            n.body,
            "opencode/2.3.0 sends big-model a median prompt of 4,000 tokens. 30% of its 10 \
             requests failed or timed out. Try: send a smaller prompt (fewer MCP tools or the \
             bare mode of opencode/2.3.0)."
        );
    }

    #[test]
    fn healthy_clients_get_no_advice() {
        let input = RuleInput {
            clients: vec![client("opencode/2.3.0", 20_000, 0.29)],
            ..quiet()
        };
        assert!(evaluate(&input, NOW).is_empty());
        let no_budget = RuleInput {
            budget_ms: 0,
            clients: vec![client("opencode/2.3.0", 94_000, 0.0)],
            faster_models: vec![("small-model".into(), linear(1.0))],
            ..quiet()
        };
        assert!(evaluate(&no_budget, NOW).is_empty());
    }

    #[test]
    fn slow_model_does_not_fit_and_suppresses_client_advice() {
        let input = RuleInput {
            fit: Some(linear(13.7)),
            budget_ms: 240_000,
            clients: vec![client("opencode/2.3.0", 94_000, 0.9)],
            ..quiet()
        };
        let out = evaluate(&input, NOW);
        let n = only(&out, RULE_DOES_NOT_FIT);
        assert!(by_rule(&out, RULE_HARNESS_ADVICE).is_empty());
        assert_eq!(n.id, "tuner:does_not_fit:local/big-model");
        assert_eq!(n.kind, NotificationKind::Advisory);
        assert_eq!(n.severity, NotificationSeverity::Warning);
        assert_eq!(
            n.body,
            "big-model does not fit this machine for agent use: a 20,000 token prompt needs \
             ~4.6 min at 73 tok/s. Use it for short chat only, or remove it."
        );
    }

    #[test]
    fn model_that_fits_raises_no_verdict() {
        let low = PrefillFit {
            low_confidence: true,
            ..linear(100.0)
        };
        let cases = [
            quiet(),
            RuleInput {
                fit: Some(low),
                ..quiet()
            },
            RuleInput {
                fit: Some(linear(100.0)),
                budget_ms: 0,
                ..quiet()
            },
        ];
        for input in cases {
            assert!(by_rule(&evaluate(&input, NOW), RULE_DOES_NOT_FIT).is_empty());
        }
    }

    #[test]
    fn ids_stay_stable_across_runs() {
        let input = RuleInput {
            flags: HashMap::new(),
            ctx: Some(131_072),
            tool_share: Some(0.9),
            clients: vec![client("opencode/2.3.0", 94_000, 0.0)],
            ..quiet()
        };
        let ids = |now| {
            evaluate(&input, now)
                .into_iter()
                .map(|n| n.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(1), ids(2));
        assert_eq!(
            ids(1),
            vec![
                "tuner:kv_cache_q8:local/big-model",
                "tuner:ctx_too_large:local/big-model",
                "tuner:tool_schema_heavy:local/big-model",
                "tuner:harness_advice:local/big-model:opencode/2.3.0",
            ]
        );
    }

    #[test]
    fn formatting_helpers_render_readable_numbers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(human_duration(54_000), "~54 s");
        assert_eq!(human_duration(60_000), "~1.0 min");
        assert_eq!(avg_tok_per_s(&linear(0.0), 10), 10_000);
    }
}
