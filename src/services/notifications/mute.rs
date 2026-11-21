//! Mute and snooze rules: env rules from `LLM_HUB_NOTIFY_MUTE` plus the rules users add in the UI.

use std::fmt;

use crate::schemas::notification::{
    MuteMatch, MuteOrigin, MuteScope, Notification, NotificationMute,
};
use crate::utils::glob::glob_matches;

/// The most mute rules the set holds, env and UI rules together.
pub const MAX_MUTE_RULES: usize = 200;
/// The longest glob a mute rule field takes, in characters.
pub const MAX_GLOB_CHARS: usize = 256;

/// Why the set refused to add or remove a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuteError {
    /// The set already holds [`MAX_MUTE_RULES`] rules.
    TooMany,
    /// A glob of the named field is longer than [`MAX_GLOB_CHARS`].
    GlobTooLong(&'static str),
    /// The rule sets no field, so it would mute everything.
    EmptyMatch,
    /// The rule with this id comes from `LLM_HUB_NOTIFY_MUTE`, which the UI cannot change.
    EnvRule(String),
}

impl fmt::Display for MuteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuteError::TooMany => write!(f, "the limit of {MAX_MUTE_RULES} mute rules is reached"),
            MuteError::GlobTooLong(field) => write!(
                f,
                "the {field} glob is longer than {MAX_GLOB_CHARS} characters"
            ),
            MuteError::EmptyMatch => write!(f, "a mute rule needs at least one field to match"),
            MuteError::EnvRule(id) => write!(
                f,
                "mute rule {id} comes from LLM_HUB_NOTIFY_MUTE. Change it there"
            ),
        }
    }
}

impl std::error::Error for MuteError {}

/// The env and UI mute rules. Env rules mute forever with scope `all`.
#[derive(Clone, Debug, Default)]
pub struct MuteSet {
    env: Vec<NotificationMute>,
    ui: Vec<NotificationMute>,
}

impl MuteSet {
    /// A set with the env matchers as forever `all` rules (ids `env-0`, `env-1`, ...) and the saved
    /// UI rules. Matchers that set no field are skipped, and rules past the cap are dropped.
    #[must_use]
    pub fn new(env: Vec<MuteMatch>, ui: Vec<NotificationMute>) -> MuteSet {
        let env: Vec<NotificationMute> = env
            .into_iter()
            .filter(|matcher| field_globs(matcher).any(|(_, glob)| glob.is_some()))
            .take(MAX_MUTE_RULES)
            .enumerate()
            .map(|(index, matcher)| NotificationMute {
                id: format!("env-{index}"),
                matcher,
                until_ms: None,
                scope: MuteScope::All,
                note: None,
                origin: MuteOrigin::Env,
                created_ms: 0,
            })
            .collect();
        let room = MAX_MUTE_RULES - env.len();
        let ui = ui
            .into_iter()
            .take(room)
            .map(|rule| NotificationMute {
                origin: MuteOrigin::Ui,
                ..rule
            })
            .collect();
        MuteSet { env, ui }
    }

    /// The scope of the live rules that match `notification` at `now_ms`, `All` over `Sinks`.
    #[must_use]
    pub fn matches(&self, notification: &Notification, now_ms: u64) -> Option<MuteScope> {
        let mut found = None;
        for rule in self.env.iter().chain(&self.ui) {
            if !is_live(rule, now_ms) || !matcher_matches(&rule.matcher, notification) {
                continue;
            }
            if rule.scope == MuteScope::All {
                return Some(MuteScope::All);
            }
            found = Some(rule.scope);
        }
        found
    }

    /// Adds `rule` as a UI rule, or replaces the UI rule with the same id.
    ///
    /// # Errors
    /// [`MuteError`] when the rule sets no field, a glob is too long, the id names an env rule, or
    /// the set is full.
    pub fn add_ui(&mut self, rule: NotificationMute) -> Result<(), MuteError> {
        validate_matcher(&rule.matcher)?;
        if self.env.iter().any(|env| env.id == rule.id) {
            return Err(MuteError::EnvRule(rule.id));
        }
        let rule = NotificationMute {
            origin: MuteOrigin::Ui,
            ..rule
        };
        if let Some(existing) = self.ui.iter_mut().find(|ui| ui.id == rule.id) {
            *existing = rule;
            return Ok(());
        }
        if self.env.len() + self.ui.len() >= MAX_MUTE_RULES {
            return Err(MuteError::TooMany);
        }
        self.ui.push(rule);
        Ok(())
    }

    /// Removes the UI rule `id`. Returns false when no rule has that id.
    ///
    /// # Errors
    /// [`MuteError::EnvRule`] when `id` names an env rule.
    pub fn remove(&mut self, id: &str) -> Result<bool, MuteError> {
        if self.env.iter().any(|env| env.id == id) {
            return Err(MuteError::EnvRule(id.to_owned()));
        }
        let before = self.ui.len();
        self.ui.retain(|ui| ui.id != id);
        Ok(self.ui.len() != before)
    }

    /// Drops the UI rules whose snooze ended by `now_ms` and returns their ids.
    pub fn prune_expired(&mut self, now_ms: u64) -> Vec<String> {
        let (live, expired): (Vec<_>, Vec<_>) = std::mem::take(&mut self.ui)
            .into_iter()
            .partition(|rule| is_live(rule, now_ms));
        self.ui = live;
        expired.into_iter().map(|rule| rule.id).collect()
    }

    /// The UI rule `id`, when the set holds one.
    #[must_use]
    pub fn ui_rule(&self, id: &str) -> Option<&NotificationMute> {
        self.ui.iter().find(|rule| rule.id == id)
    }

    /// Every rule, env rules first, then UI rules in the order they were added.
    #[must_use]
    pub fn list(&self) -> Vec<NotificationMute> {
        self.env.iter().chain(&self.ui).cloned().collect()
    }
}

/// Checks that `matcher` sets at least one field and that no glob is too long.
///
/// # Errors
/// [`MuteError::EmptyMatch`] or [`MuteError::GlobTooLong`].
pub fn validate_matcher(matcher: &MuteMatch) -> Result<(), MuteError> {
    let mut any = false;
    for (field, glob) in field_globs(matcher) {
        let Some(glob) = glob else { continue };
        if glob.chars().count() > MAX_GLOB_CHARS {
            return Err(MuteError::GlobTooLong(field));
        }
        any = true;
    }
    if any {
        Ok(())
    } else {
        Err(MuteError::EmptyMatch)
    }
}

fn is_live(rule: &NotificationMute, now_ms: u64) -> bool {
    rule.until_ms.is_none_or(|until| until > now_ms)
}

fn field_globs(matcher: &MuteMatch) -> impl Iterator<Item = (&'static str, Option<&str>)> {
    [
        ("source", matcher.source.as_deref()),
        ("kind", matcher.kind.as_deref()),
        ("rule", matcher.rule.as_deref()),
        ("profile", matcher.profile.as_deref()),
        ("model", matcher.model.as_deref()),
        ("title", matcher.title.as_deref()),
    ]
    .into_iter()
}

/// True when every set glob matches. A glob on a field the notification lacks does not match.
fn matcher_matches(matcher: &MuteMatch, notification: &Notification) -> bool {
    let values = [
        Some(notification.source.as_str()),
        Some(notification.kind.as_str()),
        notification.rule.as_deref(),
        notification.profile.as_deref(),
        notification.model.as_deref(),
        Some(notification.title.as_str()),
    ];
    field_globs(matcher)
        .zip(values)
        .all(|((_, glob), value)| match (glob, value) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(glob), Some(value)) => glob_matches(glob, value),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::notification::{
        NotificationKind, NotificationSeverity, NotificationStatus,
    };

    fn notification() -> Notification {
        Notification {
            id: "n1".into(),
            kind: NotificationKind::Proposal,
            source: "local_tuner".into(),
            severity: NotificationSeverity::Warning,
            title: "Quantize the KV cache".into(),
            body: "body".into(),
            profile: Some("local".into()),
            model: Some("local/gemma-4-31b".into()),
            rule: Some("kv_cache_q8".into()),
            diff: None,
            actions: Notification::default_actions(NotificationKind::Proposal),
            status: NotificationStatus::Pending,
            created_ms: 1,
            updated_ms: 1,
            result: None,
            muted: false,
        }
    }

    fn rule(
        id: &str,
        matcher: MuteMatch,
        scope: MuteScope,
        until_ms: Option<u64>,
    ) -> NotificationMute {
        NotificationMute {
            id: id.into(),
            matcher,
            until_ms,
            scope,
            note: None,
            origin: MuteOrigin::Env,
            created_ms: 5,
        }
    }

    fn source(glob: &str) -> MuteMatch {
        MuteMatch {
            source: Some(glob.into()),
            ..MuteMatch::default()
        }
    }

    #[test]
    fn env_rules_mute_everything_they_match_forever() {
        let set = MuteSet::new(vec![source("local_*")], Vec::new());
        assert_eq!(set.matches(&notification(), u64::MAX), Some(MuteScope::All));
        let listed = set.list();
        assert_eq!(listed[0].id, "env-0");
        assert_eq!(listed[0].origin, MuteOrigin::Env);
        assert_eq!(listed[0].until_ms, None);
    }

    #[test]
    fn every_given_field_must_match() {
        let mut matcher = MuteMatch {
            source: Some("local_tuner".into()),
            kind: Some("proposal".into()),
            rule: Some("kv_*".into()),
            profile: Some("local".into()),
            model: Some("*/gemma-*".into()),
            title: Some("Quantize*".into()),
        };
        assert!(matcher_matches(&matcher, &notification()));
        matcher.kind = Some("guard".into());
        assert!(!matcher_matches(&matcher, &notification()));
    }

    #[test]
    fn a_glob_on_a_missing_field_does_not_match() {
        let mut n = notification();
        n.model = None;
        let matcher = MuteMatch {
            model: Some("*".into()),
            ..MuteMatch::default()
        };
        assert!(!matcher_matches(&matcher, &n));
    }

    #[test]
    fn all_wins_over_sinks_in_any_order() {
        let mut set = MuteSet::default();
        set.add_ui(rule("a", source("local_tuner"), MuteScope::Sinks, None))
            .unwrap();
        assert_eq!(set.matches(&notification(), 0), Some(MuteScope::Sinks));
        set.add_ui(rule("b", source("*"), MuteScope::All, None))
            .unwrap();
        assert_eq!(set.matches(&notification(), 0), Some(MuteScope::All));
        set.add_ui(rule("c", source("*"), MuteScope::Sinks, None))
            .unwrap();
        assert_eq!(set.matches(&notification(), 0), Some(MuteScope::All));
    }

    #[test]
    fn no_rule_matching_returns_none() {
        let set = MuteSet::new(vec![source("errors")], Vec::new());
        assert_eq!(set.matches(&notification(), 0), None);
    }

    #[test]
    fn expired_rules_stop_matching_and_are_pruned() {
        let mut set = MuteSet::new(
            Vec::new(),
            vec![rule("snooze", source("*"), MuteScope::All, Some(100))],
        );
        assert_eq!(set.list()[0].origin, MuteOrigin::Ui);
        assert_eq!(set.matches(&notification(), 99), Some(MuteScope::All));
        assert_eq!(set.matches(&notification(), 100), None);
        assert!(set.prune_expired(99).is_empty());
        assert_eq!(set.prune_expired(100), vec!["snooze".to_owned()]);
        assert!(set.list().is_empty());
    }

    #[test]
    fn add_ui_rejects_empty_and_long_globs() {
        let mut set = MuteSet::default();
        let empty = rule("e", MuteMatch::default(), MuteScope::All, None);
        assert_eq!(set.add_ui(empty), Err(MuteError::EmptyMatch));
        let long = MuteMatch {
            title: Some("x".repeat(MAX_GLOB_CHARS + 1)),
            ..MuteMatch::default()
        };
        assert_eq!(
            set.add_ui(rule("l", long, MuteScope::All, None)),
            Err(MuteError::GlobTooLong("title"))
        );
        let fits = source(&"x".repeat(MAX_GLOB_CHARS));
        assert!(set.add_ui(rule("f", fits, MuteScope::All, None)).is_ok());
    }

    #[test]
    fn add_ui_caps_the_total_and_replaces_by_id() {
        let env = vec![source("errors"); 10];
        let mut set = MuteSet::new(env, Vec::new());
        for index in 0..MAX_MUTE_RULES - 10 {
            set.add_ui(rule(
                &format!("u{index}"),
                source("*"),
                MuteScope::Sinks,
                None,
            ))
            .unwrap();
        }
        assert_eq!(
            set.add_ui(rule("over", source("*"), MuteScope::All, None)),
            Err(MuteError::TooMany)
        );
        set.add_ui(rule("u0", source("api"), MuteScope::All, None))
            .unwrap();
        let listed = set.list();
        assert_eq!(listed.len(), MAX_MUTE_RULES);
        let replaced = listed.iter().find(|r| r.id == "u0").unwrap();
        assert_eq!(replaced.matcher, source("api"));
        assert_eq!(replaced.origin, MuteOrigin::Ui);
    }

    #[test]
    fn new_skips_empty_env_matchers_and_caps_loaded_rules() {
        let ui: Vec<_> = (0..MAX_MUTE_RULES)
            .map(|index| rule(&format!("u{index}"), source("*"), MuteScope::All, None))
            .collect();
        let set = MuteSet::new(vec![MuteMatch::default(), source("errors")], ui);
        let listed = set.list();
        assert_eq!(listed.len(), MAX_MUTE_RULES);
        assert_eq!(listed[0].id, "env-0");
        assert_eq!(listed[0].matcher, source("errors"));
    }

    #[test]
    fn env_rules_cannot_be_removed_or_replaced() {
        let mut set = MuteSet::new(vec![source("errors")], Vec::new());
        assert_eq!(set.remove("env-0"), Err(MuteError::EnvRule("env-0".into())));
        assert_eq!(
            set.add_ui(rule("env-0", source("api"), MuteScope::All, None)),
            Err(MuteError::EnvRule("env-0".into()))
        );
        set.add_ui(rule("u", source("api"), MuteScope::All, None))
            .unwrap();
        assert_eq!(set.ui_rule("u").map(|r| r.origin), Some(MuteOrigin::Ui));
        assert!(set.ui_rule("env-0").is_none());
        assert_eq!(set.remove("u"), Ok(true));
        assert!(set.ui_rule("u").is_none());
        assert_eq!(set.remove("u"), Ok(false));
    }

    #[test]
    fn errors_explain_themselves() {
        assert!(MuteError::TooMany.to_string().contains("200"));
        assert!(
            MuteError::GlobTooLong("model")
                .to_string()
                .contains("model")
        );
        assert!(MuteError::EmptyMatch.to_string().contains("at least one"));
        assert!(
            MuteError::EnvRule("env-1".into())
                .to_string()
                .contains("LLM_HUB_NOTIFY_MUTE")
        );
    }
}
