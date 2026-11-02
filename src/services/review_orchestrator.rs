use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::schemas::model_id::ModelId;
use crate::schemas::review_evidence::ReviewEvidence;
use crate::schemas::review_outcome::ReviewOutcome;
use crate::schemas::review_verdict::ReviewVerdict;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::schemas::stage_budget::StageBudget;
use crate::schemas::stage_error::StageError;
use crate::schemas::stage_record::StageRecord;
use crate::schemas::stage_reply::StageReply;
use crate::services::review_graph::ReviewGraph;
use crate::services::review_prompts::ReviewPrompts;
use crate::services::stage_transport::StageTransport;

/// How the reviewers' round ended: every verdict it read, or the caller's abort.
enum JudgeRound {
    Aborted,
    Done {
        verdicts: Vec<ReviewVerdict>,
        /// The first stage that failed or was skipped; the draft then goes out without repair.
        failure: Option<&'static str>,
    },
}

/// Runs the solver, then the review graph's judges or critics, then at most one repair.
#[derive(Debug)]
pub struct ReviewOrchestrator;

impl ReviewOrchestrator {
    /// A stage that failed or was skipped because the budget ran out.
    pub const REASON_BUDGET: &'static str = "budget_exhausted";
    /// A reviewer replied with something other than one verdict object.
    pub const REASON_MALFORMED_VERDICT: &'static str = "malformed_verdict";
    /// The graph allows no repair, so a non-accepting verdict cannot be acted on.
    pub const REASON_REPAIR_UNAVAILABLE: &'static str = "repair_unavailable";
    /// The solver wrote no text to review.
    pub const REASON_NO_DRAFT_TEXT: &'static str = "no_draft_text";
    /// A reviewer rejected the draft as unusable, so the caller gets no repair.
    pub const REASON_DRAFT_REJECTED: &'static str = "draft_rejected";
    /// Every planned reviewer was the answering solver or skipped, so no independent verdict exists.
    pub const REASON_NO_INDEPENDENT_REVIEWER: &'static str = "no_independent_reviewer";
    /// A planned reviewer is the answering solver, which never judges its own draft.
    pub const REASON_REVIEWER_IS_SOLVER: &'static str = "reviewer_is_solver";

    /// Runs the solver under the graph's budget, then reviews its text; tool calls return unreviewed at once.
    /// Dropping the future, or resolving `abort`, cancels every stage still in flight.
    #[allow(clippy::too_many_lines)] // one sequential state machine; splitting it would scatter the budget and abort handling
    pub async fn run<S, T, A>(
        solver: S,
        solver_model: &ModelId,
        graph: &ReviewGraph,
        transport: &T,
        evidence: ReviewEvidence<'_>,
        abort: A,
    ) -> Result<ReviewOutcome, StageError>
    where
        S: Future<Output = Result<StageReply, StageError>>,
        T: StageTransport + ?Sized,
        A: Future<Output = ()>,
    {
        tokio::pin!(abort);
        let mut budget = graph.budget.clone();
        let wall_deadline = Instant::now() + budget.wall_time;
        let mut stages: Vec<StageRecord> = Vec::new();

        if !budget.admit_call() {
            return Err(StageError::Upstream(
                "the review budget is spent before the solver runs".to_owned(),
            ));
        }
        stages.push(Self::record(
            SelectionStage::Solver,
            solver_model,
            SelectionState::Started,
            None,
        ));
        let solver_outcome = tokio::select! {
            biased;
            () = &mut abort => {
                stages.push(Self::record(SelectionStage::Solver, solver_model, SelectionState::Cancelled, None));
                return Ok(Self::aborted(solver_model, stages));
            }
            result = Self::bounded(budget.wall_time, solver) => result,
        };
        let reply = match solver_outcome {
            Ok(reply) => reply,
            Err(error) => {
                stages.push(Self::record(
                    SelectionStage::Solver,
                    solver_model,
                    SelectionState::Rejected,
                    Some(error.reason()),
                ));
                return Err(error);
            }
        };
        stages.push(Self::record(
            SelectionStage::Solver,
            solver_model,
            SelectionState::Completed,
            None,
        ));
        let within_budget = budget.charge(Self::total_tokens(&reply.completion), reply.bytes);
        let completion = reply.completion;

        if Self::has_tool_calls(&completion) {
            return Ok(Self::finished(
                completion,
                solver_model,
                stages,
                None,
                false,
            ));
        }
        let Some(draft) = Self::draft_text(&completion) else {
            return Ok(Self::finished(
                completion,
                solver_model,
                stages,
                Some(Self::REASON_NO_DRAFT_TEXT),
                false,
            ));
        };
        if !within_budget {
            return Ok(Self::finished(
                completion,
                solver_model,
                stages,
                Some(Self::REASON_BUDGET),
                false,
            ));
        }

        let (verdicts, failure) = match Self::judge_all(
            graph,
            solver_model,
            transport,
            evidence,
            &draft,
            &mut budget,
            &mut stages,
            &mut abort,
            wall_deadline,
        )
        .await
        {
            JudgeRound::Aborted => return Ok(Self::aborted(solver_model, stages)),
            JudgeRound::Done { verdicts, failure } => (verdicts, failure),
        };
        // A rejection outranks a failed sibling: the draft is unusable, so no repair is written.
        if verdicts
            .iter()
            .any(|verdict| matches!(verdict, ReviewVerdict::Reject { .. }))
        {
            return Ok(Self::rejected(completion, solver_model, stages));
        }
        if let Some(reason) = failure {
            let reviewed = !verdicts.is_empty();
            return Ok(Self::finished(
                completion,
                solver_model,
                stages,
                Some(reason),
                reviewed,
            ));
        }

        let issues: Vec<String> = verdicts
            .iter()
            .flat_map(|verdict| verdict.issues().iter().cloned())
            .collect();
        if issues.is_empty() {
            return Ok(Self::finished(completion, solver_model, stages, None, true));
        }
        if !graph.repair {
            return Ok(Self::finished(
                completion,
                solver_model,
                stages,
                Some(Self::REASON_REPAIR_UNAVAILABLE),
                true,
            ));
        }
        Self::repair_once(
            solver_model,
            transport,
            evidence,
            &draft,
            &issues,
            completion,
            &mut budget,
            &mut stages,
            &mut abort,
            wall_deadline,
        )
        .await
    }
}

impl ReviewOrchestrator {
    /// Admits and runs every reviewer other than the answering solver in parallel, and reports the first failed stage.
    // Each input feeds a different stage call, so bundling them would only rename the arguments.
    #[allow(clippy::too_many_arguments)]
    async fn judge_all<T, A>(
        graph: &ReviewGraph,
        solver: &ModelId,
        transport: &T,
        evidence: ReviewEvidence<'_>,
        draft: &str,
        budget: &mut StageBudget,
        stages: &mut Vec<StageRecord>,
        abort: &mut Pin<&mut A>,
        wall_deadline: Instant,
    ) -> JudgeRound
    where
        T: StageTransport + ?Sized,
        A: Future<Output = ()>,
    {
        let stage = graph.reviewer_stage;
        let mut admitted: Vec<ModelId> = Vec::new();
        let mut failure: Option<&'static str> = None;
        let mut spare = graph
            .spare
            .as_ref()
            .and_then(|descriptor| ModelId::parse(&descriptor.qualified_id));
        let mut queue: VecDeque<ModelId> = graph
            .reviewers
            .iter()
            .filter_map(|descriptor| ModelId::parse(&descriptor.qualified_id))
            .collect();
        while let Some(model) = queue.pop_front() {
            if model.qualified() == solver.qualified() {
                // The answering solver never judges its own draft, even when it is a fallback.
                stages.push(Self::record(
                    stage,
                    &model,
                    SelectionState::Rejected,
                    Some(Self::REASON_REVIEWER_IS_SOLVER),
                ));
                // The spare candidate takes the seat the answering solver gives up.
                if let Some(replacement) = spare
                    .take()
                    .filter(|replacement| replacement.qualified() != solver.qualified())
                {
                    queue.push_back(replacement);
                }
                continue;
            }
            if budget.admit_call() {
                admitted.push(model);
            } else {
                failure.get_or_insert(Self::REASON_BUDGET);
                stages.push(Self::record(
                    stage,
                    &model,
                    SelectionState::Rejected,
                    Some(Self::REASON_BUDGET),
                ));
            }
        }
        if admitted.is_empty() {
            failure.get_or_insert(Self::REASON_NO_INDEPENDENT_REVIEWER);
            return JudgeRound::Done {
                verdicts: Vec::new(),
                failure,
            };
        }
        for model in &admitted {
            stages.push(Self::record(stage, model, SelectionState::Started, None));
        }
        let gate = Semaphore::new(if graph.serialized { 1 } else { admitted.len() });
        let cap = budget.bytes_left();
        let deadline = Self::stage_limit(budget, wall_deadline);
        let calls = admitted.iter().map(|model| {
            let body = ReviewPrompts::judge(&model.model, evidence, draft);
            Self::judge_once(transport, &gate, model, body, cap, deadline)
        });
        let judged = tokio::select! {
            biased;
            () = abort.as_mut() => {
                for model in &admitted {
                    stages.push(Self::record(stage, model, SelectionState::Cancelled, None));
                }
                return JudgeRound::Aborted;
            }
            results = join_all(calls) => results,
        };
        let mut verdicts = Vec::new();
        for (model, ((tokens, bytes), outcome)) in admitted.iter().zip(judged) {
            budget.charge(tokens, bytes);
            match outcome {
                Ok(verdict) => {
                    if matches!(verdict, ReviewVerdict::Reject { .. }) {
                        stages.push(Self::record(
                            stage,
                            model,
                            SelectionState::Rejected,
                            Some(Self::REASON_DRAFT_REJECTED),
                        ));
                    } else {
                        stages.push(Self::record(stage, model, SelectionState::Completed, None));
                    }
                    verdicts.push(verdict);
                }
                Err(reason) => {
                    failure.get_or_insert(reason);
                    stages.push(Self::record(
                        stage,
                        model,
                        SelectionState::Rejected,
                        Some(reason),
                    ));
                }
            }
        }
        JudgeRound::Done { verdicts, failure }
    }

    /// One reviewer call under the gate and the stage deadline, with its verdict parsed strictly.
    /// Returns the tokens and bytes the reply used, which are charged even when the verdict is malformed.
    async fn judge_once<T>(
        transport: &T,
        gate: &Semaphore,
        model: &ModelId,
        body: Value,
        cap: u64,
        deadline: Duration,
    ) -> ((u64, u64), Result<ReviewVerdict, &'static str>)
    where
        T: StageTransport + ?Sized,
    {
        let _permit = gate.acquire().await.ok();
        let reply = match Self::bounded(deadline, transport.complete(model, body, cap)).await {
            Ok(reply) => reply,
            Err(error) => return ((0, 0), Err(error.reason())),
        };
        let usage = (Self::total_tokens(&reply.completion), reply.bytes);
        let verdict = Self::draft_text(&reply.completion)
            .ok_or(Self::REASON_MALFORMED_VERDICT)
            .and_then(|text| {
                ReviewVerdict::parse(&text).map_err(|_| Self::REASON_MALFORMED_VERDICT)
            });
        (usage, verdict)
    }

    /// Asks the solver for one repair that fixes `issues`, and returns its reply in place of the draft.
    // Each input feeds a different stage call, so bundling them would only rename the arguments.
    #[allow(clippy::too_many_arguments)]
    async fn repair_once<T, A>(
        solver_model: &ModelId,
        transport: &T,
        evidence: ReviewEvidence<'_>,
        draft: &str,
        issues: &[String],
        draft_completion: Value,
        budget: &mut StageBudget,
        stages: &mut Vec<StageRecord>,
        abort: &mut Pin<&mut A>,
        wall_deadline: Instant,
    ) -> Result<ReviewOutcome, StageError>
    where
        T: StageTransport + ?Sized,
        A: Future<Output = ()>,
    {
        if !budget.admit_call() {
            stages.push(Self::record(
                SelectionStage::Repair,
                solver_model,
                SelectionState::Rejected,
                Some(Self::REASON_BUDGET),
            ));
            return Ok(Self::finished(
                draft_completion,
                solver_model,
                std::mem::take(stages),
                Some(Self::REASON_BUDGET),
                true,
            ));
        }
        stages.push(Self::record(
            SelectionStage::Repair,
            solver_model,
            SelectionState::Started,
            None,
        ));
        let body = ReviewPrompts::repair(&solver_model.model, evidence, draft, issues);
        let cap = budget.bytes_left();
        let limit = Self::stage_limit(budget, wall_deadline);
        let repaired = tokio::select! {
            biased;
            () = abort.as_mut() => {
                stages.push(Self::record(SelectionStage::Repair, solver_model, SelectionState::Cancelled, None));
                return Ok(Self::aborted(solver_model, std::mem::take(stages)));
            }
            result = Self::bounded(limit, transport.complete(solver_model, body, cap)) => result,
        };
        match repaired {
            Ok(reply) => {
                budget.charge(Self::total_tokens(&reply.completion), reply.bytes);
                stages.push(Self::record(
                    SelectionStage::Repair,
                    solver_model,
                    SelectionState::Completed,
                    None,
                ));
                Ok(Self::finished(
                    reply.completion,
                    solver_model,
                    std::mem::take(stages),
                    None,
                    true,
                ))
            }
            Err(error) => {
                stages.push(Self::record(
                    SelectionStage::Repair,
                    solver_model,
                    SelectionState::Rejected,
                    Some(error.reason()),
                ));
                Ok(Self::finished(
                    draft_completion,
                    solver_model,
                    std::mem::take(stages),
                    Some(error.reason()),
                    true,
                ))
            }
        }
    }

    /// The longest one stage may run: its own deadline, or the review's remaining wall time if shorter.
    fn stage_limit(budget: &StageBudget, wall_deadline: Instant) -> Duration {
        budget
            .stage_deadline
            .min(wall_deadline.saturating_duration_since(Instant::now()))
    }

    /// Bounds one call by `limit`, reporting an expired limit as `StageError::Timeout`.
    async fn bounded<O, F>(limit: Duration, future: F) -> Result<O, StageError>
    where
        F: Future<Output = Result<O, StageError>>,
    {
        tokio::time::timeout(limit, future)
            .await
            .unwrap_or(Err(StageError::Timeout))
    }

    /// Builds one stage record for `model` in `stage`, in `selection_state`.
    fn record(
        stage: SelectionStage,
        model: &ModelId,
        selection_state: SelectionState,
        reason: Option<&'static str>,
    ) -> StageRecord {
        StageRecord {
            stage,
            model: model.qualified(),
            state: selection_state,
            reason,
        }
    }

    /// Builds the outcome of a review that ran to its end, with the completion it sends.
    fn finished(
        completion: Value,
        served: &ModelId,
        stages: Vec<StageRecord>,
        review_failed: Option<&'static str>,
        reviewed: bool,
    ) -> ReviewOutcome {
        ReviewOutcome {
            completion,
            served: served.clone(),
            stages,
            review_failed,
            reviewed,
            rejected: false,
            aborted: false,
        }
    }

    /// Builds the outcome of a draft a reviewer rejected: it gets no repair and is not sent.
    fn rejected(completion: Value, served: &ModelId, stages: Vec<StageRecord>) -> ReviewOutcome {
        ReviewOutcome {
            rejected: true,
            ..Self::finished(completion, served, stages, None, true)
        }
    }

    /// Builds the outcome of a review the caller dropped, with no completion to send.
    fn aborted(served: &ModelId, stages: Vec<StageRecord>) -> ReviewOutcome {
        ReviewOutcome {
            aborted: true,
            ..Self::finished(Value::Null, served, stages, None, false)
        }
    }

    /// The completion's reported total tokens, or zero when it reports none.
    fn total_tokens(completion: &Value) -> u64 {
        completion
            .pointer("/usage/total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// True when the completion's first choice asks for tool calls.
    fn has_tool_calls(completion: &Value) -> bool {
        completion
            .pointer("/choices/0/message/tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
    }

    /// The text of a chat completion's first choice, `None` when it is empty or absent.
    fn draft_text(completion: &Value) -> Option<String> {
        completion
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::schemas::evidence_source::EvidenceSource;
    use crate::schemas::limit_evidence::LimitEvidence;
    use crate::schemas::model_descriptor::ModelDescriptor;
    use crate::schemas::smart_auto_tier::SmartAutoTier;

    /// Answers each stage call from a script, counting calls and recording their bodies.
    struct Scripted {
        replies: Mutex<VecDeque<Result<StageReply, StageError>>>,
        calls: AtomicUsize,
        bodies: Mutex<Vec<Value>>,
        delay: Duration,
    }

    impl Scripted {
        fn new(replies: Vec<Result<StageReply, StageError>>, delay: Duration) -> Scripted {
            Scripted {
                replies: Mutex::new(replies.into()),
                calls: AtomicUsize::new(0),
                bodies: Mutex::new(Vec::new()),
                delay,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl StageTransport for Scripted {
        fn complete<'a>(
            &'a self,
            _model: &'a ModelId,
            body: Value,
            _cap_bytes: u64,
        ) -> BoxFuture<'a, Result<StageReply, StageError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.bodies.lock().unwrap().push(body);
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Err(StageError::Upstream("no scripted reply".to_owned())))
            })
        }
    }

    fn completion(text: &str) -> Value {
        json!({
            "id": "chatcmpl-1",
            "model": "copilot/solver",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
    }

    #[allow(clippy::unnecessary_wraps)] // the stage transport returns this Result shape
    fn reply(text: &str) -> Result<StageReply, StageError> {
        Ok(StageReply {
            completion: completion(text),
            bytes: 64,
        })
    }

    fn judge_descriptor(name: &str) -> ModelDescriptor {
        ModelDescriptor {
            endpoints: vec!["/chat/completions".to_owned()],
            context: Some(LimitEvidence::new(
                200_000,
                EvidenceSource::Listed,
                "copilot",
                0.6,
            )),
            ..ModelDescriptor::for_test(&format!("copilot/{name}"))
        }
    }

    fn graph(tier: SmartAutoTier, judges: usize) -> ReviewGraph {
        ReviewGraph {
            reviewers: (0..judges)
                .map(|index| judge_descriptor(&format!("judge-{index}")))
                .collect(),
            reviewer_stage: if tier == SmartAutoTier::Intelligence {
                SelectionStage::Critic
            } else {
                SelectionStage::Judge
            },
            spare: None,
            repair: true,
            serialized: false,
            budget: StageBudget::for_tier(tier).expect("reviewed tier"),
        }
    }

    fn solver() -> ModelId {
        ModelId::parse("copilot/solver").expect("parses")
    }

    fn evidence() -> ReviewEvidence<'static> {
        ReviewEvidence {
            task: Some("Sum 2 and 3."),
        }
    }

    fn never() -> impl Future<Output = ()> {
        std::future::pending::<()>()
    }

    fn text_of(outcome: &ReviewOutcome) -> &str {
        outcome.completion["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
    }

    fn stage_states(outcome: &ReviewOutcome, stage: SelectionStage) -> Vec<SelectionState> {
        outcome
            .stages
            .iter()
            .filter(|record| record.stage == stage)
            .map(|record| record.state)
            .collect()
    }

    #[tokio::test]
    async fn accept_returns_the_draft_after_one_judge() {
        let transport = Scripted::new(vec![reply(r#"{"verdict":"accept"}"#)], Duration::ZERO);
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5.");
        assert_eq!(outcome.served, solver());
        assert!(outcome.reviewed && outcome.review_failed.is_none());
        assert_eq!(transport.calls(), 1);
        assert_eq!(
            stage_states(&outcome, SelectionStage::Judge),
            [SelectionState::Started, SelectionState::Completed]
        );
    }

    #[tokio::test]
    async fn repair_writes_one_fixed_answer_from_the_issues() {
        let transport = Scripted::new(
            vec![
                reply(r#"{"verdict":"repair","issues":["misses the carry"]}"#),
                reply("The sum is 5 exactly."),
            ],
            Duration::ZERO,
        );
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5 exactly.");
        assert!(outcome.review_failed.is_none());
        assert_eq!(transport.calls(), 2);
        let repair_body = transport.bodies.lock().unwrap()[1].to_string();
        assert!(repair_body.contains("misses the carry"));
    }

    #[tokio::test]
    async fn a_malformed_verdict_returns_the_draft_with_a_failure() {
        let transport = Scripted::new(vec![reply("looks fine to me")], Duration::ZERO);
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5.");
        assert_eq!(
            outcome.review_failed,
            Some(ReviewOrchestrator::REASON_MALFORMED_VERDICT)
        );
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn a_judge_past_its_deadline_returns_the_draft_with_a_timeout() {
        let transport = Scripted::new(
            vec![reply(r#"{"verdict":"accept"}"#)],
            Duration::from_secs(5),
        );
        let mut judged = graph(SmartAutoTier::Balance, 1);
        judged.budget.stage_deadline = Duration::from_millis(10);
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &judged,
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5.");
        assert_eq!(outcome.review_failed, Some("timeout"));
    }

    #[tokio::test]
    async fn tool_calls_return_at_once_without_any_judge() {
        let transport = Scripted::new(vec![reply(r#"{"verdict":"accept"}"#)], Duration::ZERO);
        let tool_reply = Ok(StageReply {
            completion: json!({
                "choices": [{ "index": 0, "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{ "id": "call_1", "type": "function",
                        "function": { "name": "read_file", "arguments": "{}" } }]
                }, "finish_reason": "tool_calls" }]
            }),
            bytes: 80,
        });
        let outcome = ReviewOrchestrator::run(
            std::future::ready(tool_reply),
            &solver(),
            &graph(SmartAutoTier::Intelligence, 2),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert!(!outcome.reviewed);
        assert!(outcome.completion["choices"][0]["message"]["tool_calls"].is_array());
        assert_eq!(transport.calls(), 0);
    }

    #[tokio::test]
    async fn abort_cancels_the_in_flight_judge() {
        let transport = Scripted::new(
            vec![reply(r#"{"verdict":"accept"}"#)],
            Duration::from_secs(30),
        );
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await
        .expect("solver succeeds");
        assert!(outcome.aborted);
        assert_eq!(outcome.completion, Value::Null);
        assert_eq!(
            stage_states(&outcome, SelectionStage::Judge).last(),
            Some(&SelectionState::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_spent_budget_skips_the_judge_and_returns_the_draft() {
        let transport = Scripted::new(vec![reply(r#"{"verdict":"accept"}"#)], Duration::ZERO);
        let mut spent = graph(SmartAutoTier::Balance, 1);
        // One call is left and the solver takes it, so no judge call is admitted.
        assert!(spent.budget.admit_call() && spent.budget.admit_call());
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &spent,
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5.");
        assert_eq!(
            outcome.review_failed,
            Some(ReviewOrchestrator::REASON_BUDGET)
        );
        assert_eq!(transport.calls(), 0);
    }

    #[tokio::test]
    async fn a_failed_solver_is_an_error_for_the_caller_to_fall_back_from() {
        let transport = Scripted::new(Vec::new(), Duration::ZERO);
        let result = ReviewOrchestrator::run(
            std::future::ready(Err(StageError::Upstream("HTTP 503".to_owned()))),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            never(),
        )
        .await;
        assert!(matches!(result, Err(StageError::Upstream(_))));
        assert_eq!(transport.calls(), 0);
    }

    #[tokio::test]
    async fn intelligence_repairs_once_after_a_critic_asks_for_repair() {
        let transport = Scripted::new(
            vec![
                reply(r#"{"verdict":"accept"}"#),
                reply(r#"{"verdict":"repair","issues":["wrong unit"]}"#),
                reply("The sum is 5 units."),
            ],
            Duration::ZERO,
        );
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &graph(SmartAutoTier::Intelligence, 2),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5 units.");
        assert_eq!(transport.calls(), 3);
        assert_eq!(
            stage_states(&outcome, SelectionStage::Critic),
            [
                SelectionState::Started,
                SelectionState::Started,
                SelectionState::Completed,
                SelectionState::Completed,
            ]
        );
        assert_eq!(
            stage_states(&outcome, SelectionStage::Repair),
            [SelectionState::Started, SelectionState::Completed]
        );
    }

    #[tokio::test]
    async fn a_rejected_draft_is_not_repaired() {
        let transport = Scripted::new(
            vec![reply(r#"{"verdict":"reject","issues":["wrong answer"]}"#)],
            Duration::ZERO,
        );
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 6.")),
            &solver(),
            &graph(SmartAutoTier::Balance, 1),
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert!(
            outcome.review_failed.is_none(),
            "a rejection is not a failed review"
        );
        assert!(outcome.rejected && outcome.reviewed && !outcome.aborted);
        assert_eq!(transport.calls(), 1);
        assert!(stage_states(&outcome, SelectionStage::Repair).is_empty());
        assert_eq!(
            stage_states(&outcome, SelectionStage::Judge),
            [SelectionState::Started, SelectionState::Rejected]
        );
        assert_eq!(
            outcome.stages.last().and_then(|record| record.reason),
            Some(ReviewOrchestrator::REASON_DRAFT_REJECTED)
        );
    }

    #[tokio::test]
    async fn a_fallback_answer_hands_its_reviewer_seat_to_the_spare() {
        let transport = Scripted::new(vec![reply(r#"{"verdict":"accept"}"#)], Duration::ZERO);
        let mut seated = graph(SmartAutoTier::Balance, 0);
        // The planned reviewer is the model that answered, so the spare judges instead.
        seated.reviewers = vec![judge_descriptor("solver")];
        seated.spare = Some(judge_descriptor("judge-0"));
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &seated,
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert!(outcome.reviewed && outcome.review_failed.is_none());
        assert_eq!(transport.calls(), 1);
        assert_eq!(
            stage_states(&outcome, SelectionStage::Judge),
            [
                SelectionState::Rejected,
                SelectionState::Started,
                SelectionState::Completed
            ]
        );
        let judges: Vec<&str> = outcome
            .stages
            .iter()
            .filter(|record| record.stage == SelectionStage::Judge)
            .map(|record| record.model.as_str())
            .collect();
        assert_eq!(
            judges,
            ["copilot/solver", "copilot/judge-0", "copilot/judge-0"]
        );
    }

    #[tokio::test]
    async fn the_answering_solver_is_never_its_own_reviewer() {
        let transport = Scripted::new(vec![reply(r#"{"verdict":"accept"}"#)], Duration::ZERO);
        let mut mixed = graph(SmartAutoTier::Balance, 0);
        mixed.reviewers = vec![judge_descriptor("solver"), judge_descriptor("judge-0")];
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &mixed,
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert!(outcome.reviewed && outcome.review_failed.is_none());
        assert_eq!(transport.calls(), 1);
        assert_eq!(
            stage_states(&outcome, SelectionStage::Judge),
            [
                SelectionState::Rejected,
                SelectionState::Started,
                SelectionState::Completed
            ]
        );
    }

    #[tokio::test]
    async fn a_draft_with_no_independent_reviewer_goes_out_unreviewed() {
        let transport = Scripted::new(Vec::new(), Duration::ZERO);
        let mut only_self = graph(SmartAutoTier::Balance, 0);
        only_self.reviewers = vec![judge_descriptor("solver")];
        let outcome = ReviewOrchestrator::run(
            std::future::ready(reply("The sum is 5.")),
            &solver(),
            &only_self,
            &transport,
            evidence(),
            never(),
        )
        .await
        .expect("solver succeeds");
        assert_eq!(text_of(&outcome), "The sum is 5.");
        assert_eq!(
            outcome.review_failed,
            Some(ReviewOrchestrator::REASON_NO_INDEPENDENT_REVIEWER)
        );
        assert!(!outcome.reviewed);
        assert_eq!(transport.calls(), 0);
    }
}
