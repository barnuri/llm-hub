use crate::schemas::role_decision::RoleDecision;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::turn_signals::TurnSignals;
use crate::schemas::work_role::WorkRole;

/// A role cue: single words, and multi-word phrases matched on consecutive words.
struct Cue {
    role: WorkRole,
    reason: RoleReason,
    words: &'static [&'static str],
    phrases: &'static [&'static [&'static str]],
}

/// Classifies the work role of a user turn with keyword and shape rules only, no I/O.
#[derive(Debug)]
pub struct SmartAutoRoles;

impl SmartAutoRoles {
    const CUES: [Cue; 4] = [
        Cue {
            role: WorkRole::Planning,
            reason: RoleReason::PlanningCue,
            words: &[
                "plan",
                "planning",
                "design",
                "architecture",
                "spec",
                "specs",
            ],
            phrases: &[&["how", "should", "we"], &["how", "should", "i"]],
        },
        Cue {
            role: WorkRole::Investigation,
            reason: RoleReason::InvestigationCue,
            words: &[
                "why",
                "investigate",
                "find",
                "debug",
                "diagnose",
                "logs",
                "search",
            ],
            phrases: &[&["root", "cause"]],
        },
        Cue {
            role: WorkRole::Implementation,
            reason: RoleReason::ImplementationCue,
            words: &["implement", "write", "fix", "add", "refactor"],
            phrases: &[],
        },
        Cue {
            role: WorkRole::Validation,
            reason: RoleReason::ValidationCue,
            words: &["test", "tests", "review", "verify", "check"],
            phrases: &[],
        },
    ];
    const LONGEST_PHRASE: usize = 3;
    const PLAN_MARKERS: [&str; 4] = ["plan.md", "tasks.md", "agent-spec/", "<command-name>"];
    const REMINDER_OPEN: &str = "<system-reminder>";
    const REMINDER_CLOSE: &str = "</system-reminder>";

    /// The role of the turn, or `None` to keep the previous role on a tool continuation.
    /// A caller hint wins over every rule, a tool continuation included.
    #[must_use]
    #[allow(dead_code)] // Only the role tests call this.
    pub fn classify(signals: &TurnSignals, hint: Option<WorkRole>) -> Option<RoleDecision> {
        if hint.is_none() && signals.tool_continuation {
            return None;
        }
        Some(Self::classify_user_text(signals, hint))
    }

    /// The role of the turn from the hint or the latest user text, even on a tool continuation.
    #[must_use]
    pub fn classify_user_text(signals: &TurnSignals, hint: Option<WorkRole>) -> RoleDecision {
        let text = signals
            .last_user_text
            .as_deref()
            .map(Self::visible_text)
            .unwrap_or_default();
        let plan_backed = Self::has_plan_marker(&text);
        let (role, reason) = if let Some(role) = hint {
            (role, RoleReason::CallerHint)
        } else if text.trim().is_empty() {
            (WorkRole::Conversation, RoleReason::NoUserText)
        } else if let Some(cue) = Self::earliest_cue(&text) {
            (cue.role, cue.reason)
        } else if plan_backed {
            (WorkRole::Implementation, RoleReason::PlanMarker)
        } else {
            (WorkRole::Conversation, RoleReason::NoCue)
        };
        RoleDecision {
            role,
            plan_backed,
            reason,
        }
    }

    /// The ASCII-lowercased text without system reminder blocks, which carry no user intent.
    pub(crate) fn visible_text(text: &str) -> String {
        let lower = text.to_ascii_lowercase();
        if !lower.contains(Self::REMINDER_OPEN) {
            return lower;
        }
        let mut visible = String::with_capacity(lower.len());
        let mut rest = lower.as_str();
        while let Some(start) = rest.find(Self::REMINDER_OPEN) {
            visible.push_str(&rest[..start]);
            rest = rest[start..]
                .find(Self::REMINDER_CLOSE)
                .map_or("", |end| &rest[start + end + Self::REMINDER_CLOSE.len()..]);
        }
        visible.push_str(rest);
        visible
    }

    /// A plan or tasks file, a spec folder, a command tag, or a leading `/skill` token.
    fn has_plan_marker(text: &str) -> bool {
        let skill_invocation = text
            .trim_start()
            .strip_prefix('/')
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|token| {
                token
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase())
                    && !token.contains('/')
            });
        skill_invocation
            || Self::PLAN_MARKERS
                .iter()
                .any(|marker| text.contains(marker))
    }

    /// The first cue in reading order: a request opens with its intent, and later words
    /// name its objects, as in "implement the plan" or "fix the failing test".
    fn earliest_cue(text: &str) -> Option<&'static Cue> {
        let mut recent: [&str; Self::LONGEST_PHRASE] = [""; Self::LONGEST_PHRASE];
        for word in Self::words(text) {
            recent.rotate_left(1);
            recent[Self::LONGEST_PHRASE - 1] = word;
            let cue = Self::CUES.iter().find(|cue| {
                cue.words.contains(&word)
                    || cue.phrases.iter().any(|phrase| recent.ends_with(phrase))
            });
            if cue.is_some() {
                return cue;
            }
        }
        None
    }

    /// Words split on anything but letters, digits and path characters, so `plan.md` and
    /// `agent-spec/x` stay whole; edge path characters are trimmed, so `/plan` reads `plan`.
    fn words(text: &str) -> impl Iterator<Item = &str> {
        let is_path_char = |c: char| matches!(c, '.' | '/' | '-' | '_');
        text.split(move |c: char| !(c.is_ascii_alphanumeric() || is_path_char(c)))
            .map(move |word| word.trim_matches(is_path_char))
            .filter(|word| !word.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::role_source::RoleSource;

    fn signals(text: Option<&str>, tool_continuation: bool) -> TurnSignals {
        TurnSignals {
            estimated_prompt_tokens: 100,
            tool_continuation,
            last_user_text: text.map(str::to_string),
            has_tools: true,
            ..TurnSignals::for_test()
        }
    }

    fn classify(text: &str) -> RoleDecision {
        SmartAutoRoles::classify(&signals(Some(text), false), None).unwrap()
    }

    fn assert_role(text: &str, role: WorkRole, reason: RoleReason) {
        let decision = classify(text);
        assert_eq!((decision.role, decision.reason), (role, reason), "{text}");
        assert_eq!(decision.source(), RoleSource::Rule, "{text}");
    }

    #[test]
    fn planning_cues_classify_as_planning() {
        for text in [
            "How should we split the proxy module?",
            "how should i structure this",
            "Design the cache layer",
            "outline the architecture",
            "draft a spec for retries",
        ] {
            assert_role(text, WorkRole::Planning, RoleReason::PlanningCue);
        }
    }

    #[test]
    fn investigation_cues_classify_as_investigation() {
        for text in [
            "Why does the stream stall after 30s?",
            "what is the root cause of the 502s",
            "please debug the login flow",
            "grep the logs for timeouts",
            "find where the pool is built",
        ] {
            assert_role(text, WorkRole::Investigation, RoleReason::InvestigationCue);
        }
    }

    #[test]
    fn implementation_cues_classify_as_implementation_without_a_plan() {
        for text in [
            "fix the null check in proxy.rs",
            "Refactor the store module",
            "add a retry",
        ] {
            assert_role(
                text,
                WorkRole::Implementation,
                RoleReason::ImplementationCue,
            );
            assert!(!classify(text).plan_backed, "{text}");
        }
    }

    #[test]
    fn validation_cues_classify_as_validation() {
        for text in [
            "review my diff",
            "verify the build passes",
            "Check the output",
        ] {
            assert_role(text, WorkRole::Validation, RoleReason::ValidationCue);
        }
    }

    #[test]
    fn chit_chat_defaults_to_conversation() {
        let decision = classify("thanks, that helps!");

        assert_eq!(decision.role, WorkRole::Conversation);
        assert_eq!(decision.reason, RoleReason::NoCue);
        assert_eq!(decision.source(), RoleSource::Default);
        assert!(!decision.plan_backed);
    }

    #[test]
    fn missing_or_blank_text_defaults_to_conversation() {
        for text in [None, Some(""), Some("   \n")] {
            let decision = SmartAutoRoles::classify(&signals(text, false), None).unwrap();

            assert_eq!(decision.role, WorkRole::Conversation, "{text:?}");
            assert_eq!(decision.reason, RoleReason::NoUserText, "{text:?}");
            assert_eq!(decision.source(), RoleSource::Default, "{text:?}");
        }
    }

    #[test]
    fn caller_hint_overrides_the_rules() {
        let decision = SmartAutoRoles::classify(
            &signals(Some("design the cache, see plan.md"), false),
            Some(WorkRole::Validation),
        )
        .unwrap();

        assert_eq!(decision.role, WorkRole::Validation);
        assert_eq!(decision.reason, RoleReason::CallerHint);
        assert_eq!(decision.source(), RoleSource::Hint);
        assert!(decision.plan_backed);
    }

    #[test]
    fn tool_continuation_keeps_the_previous_role() {
        let continuation = signals(Some("design the cache"), true);

        assert_eq!(SmartAutoRoles::classify(&continuation, None), None);
    }

    #[test]
    fn caller_hint_applies_on_a_tool_continuation() {
        let continuation = signals(Some("design the cache"), true);

        let decision =
            SmartAutoRoles::classify(&continuation, Some(WorkRole::Implementation)).unwrap();

        assert_eq!(decision.role, WorkRole::Implementation);
        assert_eq!(decision.source(), RoleSource::Hint);
    }

    #[test]
    fn plan_and_tasks_file_references_mark_the_turn_plan_backed() {
        for text in [
            "Implement task 3 from agent-spec/2026-11-01-x/plan.md",
            "fix the item in TASKS.md",
            "implement the next step in agent-spec/foo/",
        ] {
            let decision = classify(text);

            assert_eq!(decision.role, WorkRole::Implementation, "{text}");
            assert!(decision.plan_backed, "{text}");
        }
    }

    #[test]
    fn plan_file_name_is_not_a_planning_cue() {
        let decision = classify("see plan.md and implement it");

        assert_eq!(decision.role, WorkRole::Implementation);
        assert_eq!(decision.reason, RoleReason::ImplementationCue);
    }

    #[test]
    fn skill_invocation_marks_the_turn_plan_backed() {
        for text in [
            "/code-gen continue",
            "<command-message>code-gen</command-message>\n<command-name>/code-gen</command-name>",
        ] {
            let decision = classify(text);

            assert_eq!(decision.role, WorkRole::Implementation, "{text}");
            assert_eq!(decision.reason, RoleReason::PlanMarker, "{text}");
            assert!(decision.plan_backed, "{text}");
        }
        let skill_with_cue = classify("/review the last commit");
        assert_eq!(skill_with_cue.role, WorkRole::Validation);
        assert!(skill_with_cue.plan_backed);
    }

    #[test]
    fn absolute_path_mid_text_is_not_a_skill_invocation() {
        assert!(!classify("fix the config in /etc/hosts").plan_backed);
    }

    #[test]
    fn leading_path_is_not_a_skill_invocation() {
        let decision = classify("/api/users returns 500, fix it");

        assert!(!decision.plan_backed);
        assert_ne!(decision.reason, RoleReason::PlanMarker);
    }

    #[test]
    fn earliest_cue_wins_on_mixed_keywords() {
        for (text, role) in [
            ("fix the failing test", WorkRole::Implementation),
            ("implement the plan", WorkRole::Implementation),
            ("review the plan", WorkRole::Validation),
            (
                "why does the test fail, then fix it",
                WorkRole::Investigation,
            ),
            ("plan how to test and fix this", WorkRole::Planning),
        ] {
            assert_eq!(classify(text).role, role, "{text}");
        }
    }

    #[test]
    fn system_reminders_are_ignored() {
        let text = "<system-reminder>Plan first. Skills: /review. See plan.md</system-reminder>\
                    thanks<system-reminder>debug";

        let decision = classify(text);

        assert_eq!(decision.role, WorkRole::Conversation);
        assert_eq!(decision.reason, RoleReason::NoCue);
        assert!(!decision.plan_backed);
    }

    #[test]
    fn cue_words_inside_longer_words_do_not_match() {
        assert_eq!(
            classify("the planet addresses").role,
            WorkRole::Conversation
        );
    }
}
