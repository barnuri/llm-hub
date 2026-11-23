//! Which extensions and rules are off: by `LLM_HUB_EXTENSIONS_DISABLED`, or from the UI. The hub
//! applies them when it builds the extension set, so a request never checks them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ext::ExtensionSet;
use crate::ext::builtin::builtin_phases;
use crate::schemas::extension_status::{DisabledBy, ExtensionKind, ExtensionStatus, RuleStatus};
use crate::services::store::Store;
use crate::utils::glob::glob_matches;

/// The prefix of an `LLM_HUB_EXTENSIONS_DISABLED` entry that names a rule of the rules file.
pub(crate) const RULE_PREFIX: &str = "rules:";
/// The name the HTTP hooks of the rules file go by.
pub(crate) const HOOKS_NAME: &str = "hooks";
/// The install setting that holds the UI choices.
const STORE_KEY: &str = "extensions_disabled";

/// What a toggle call turns on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleTarget {
    Extension,
    Rule,
}

/// Why a toggle call changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToggleError {
    /// No extension or rule has that name.
    Unknown(String),
    /// `LLM_HUB_EXTENSIONS_DISABLED` turns it off.
    SetByEnv(String),
    /// The store could not save the choice.
    Store(String),
}

/// The names and globs of `LLM_HUB_EXTENSIONS_DISABLED`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EnvDisabled {
    extensions: Vec<String>,
    rules: Vec<String>,
}

impl EnvDisabled {
    /// Splits `entries` into extension globs and, after `rules:`, rule globs.
    fn parse(entries: &[String]) -> EnvDisabled {
        let mut disabled = EnvDisabled::default();
        for entry in entries.iter().map(|entry| entry.trim()) {
            match entry.strip_prefix(RULE_PREFIX).map(str::trim) {
                Some("") => {}
                Some(rule) => disabled.rules.push(rule.to_owned()),
                None if entry.is_empty() => {}
                None => disabled.extensions.push(entry.to_owned()),
            }
        }
        disabled
    }
}

/// The names the UI turned off, saved in the store as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct UiDisabled {
    #[serde(default)]
    extensions: BTreeSet<String>,
    #[serde(default)]
    rules: BTreeSet<String>,
}

impl UiDisabled {
    fn load(store: Option<&Store>) -> UiDisabled {
        let Some(store) = store else {
            return UiDisabled::default();
        };
        let saved = store.install_setting(STORE_KEY).and_then(|text| {
            text.map_or(Ok(UiDisabled::default()), |text| {
                serde_json::from_str(&text).map_err(|e| e.to_string())
            })
        });
        saved.unwrap_or_else(|e| {
            tracing::warn!("The hub ignored the saved extension toggles: {e}");
            UiDisabled::default()
        })
    }

    fn save(&self, store: Option<&Store>) -> Result<(), String> {
        let Some(store) = store else {
            return Ok(());
        };
        let text = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_install_setting(STORE_KEY, &text)
    }

    fn names_mut(&mut self, target: ToggleTarget) -> &mut BTreeSet<String> {
        match target {
            ToggleTarget::Extension => &mut self.extensions,
            ToggleTarget::Rule => &mut self.rules,
        }
    }
}

/// The extensions and rules that are off, and what turned each off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExtensionToggles {
    env: EnvDisabled,
    ui: UiDisabled,
}

impl ExtensionToggles {
    /// The toggles of `env_entries` (`LLM_HUB_EXTENSIONS_DISABLED`) and the choices saved in `store`.
    pub(crate) fn load(env_entries: &[String], store: Option<&Store>) -> ExtensionToggles {
        ExtensionToggles {
            env: EnvDisabled::parse(env_entries),
            ui: UiDisabled::load(store),
        }
    }

    /// The same UI choices with new `LLM_HUB_EXTENSIONS_DISABLED` entries.
    pub(crate) fn with_env(&self, env_entries: &[String]) -> ExtensionToggles {
        ExtensionToggles {
            env: EnvDisabled::parse(env_entries),
            ui: self.ui.clone(),
        }
    }

    /// True when nothing is off.
    pub(crate) fn is_empty(&self) -> bool {
        self.env.extensions.is_empty()
            && self.env.rules.is_empty()
            && self.ui.extensions.is_empty()
            && self.ui.rules.is_empty()
    }

    /// What turned the extension `name` off. The env wins over the UI.
    pub(crate) fn extension(&self, name: &str) -> Option<DisabledBy> {
        disabled_by(&self.env.extensions, &self.ui.extensions, name)
    }

    /// What turned the rule `name` off. The env wins over the UI.
    pub(crate) fn rule(&self, name: &str) -> Option<DisabledBy> {
        disabled_by(&self.env.rules, &self.ui.rules, name)
    }

    /// The toggles with `name` turned on or off from the UI, saved to `store` first.
    ///
    /// # Errors
    /// [`ToggleError::SetByEnv`] when the env turns `name` off, and [`ToggleError::Store`] when
    /// the save fails.
    pub(crate) fn toggled(
        &self,
        target: ToggleTarget,
        name: &str,
        enabled: bool,
        store: Option<&Store>,
    ) -> Result<ExtensionToggles, ToggleError> {
        let current = match target {
            ToggleTarget::Extension => self.extension(name),
            ToggleTarget::Rule => self.rule(name),
        };
        if current == Some(DisabledBy::Env) {
            return Err(ToggleError::SetByEnv(name.to_owned()));
        }
        let mut next = self.clone();
        let names = next.ui.names_mut(target);
        let changed = if enabled {
            names.remove(name)
        } else {
            names.insert(name.to_owned())
        };
        if changed {
            next.ui.save(store).map_err(ToggleError::Store)?;
        }
        Ok(next)
    }
}

fn disabled_by(env: &[String], ui: &BTreeSet<String>, name: &str) -> Option<DisabledBy> {
    if env.iter().any(|glob| glob_matches(glob, name)) {
        Some(DisabledBy::Env)
    } else if ui.contains(name) {
        Some(DisabledBy::Ui)
    } else {
        None
    }
}

/// True when `set` has an extension, or its rules file a rule, named `name`.
pub(crate) fn knows(set: &ExtensionSet, target: ToggleTarget, name: &str) -> bool {
    match target {
        ToggleTarget::Extension => {
            set.names().contains(&name) || (name == HOOKS_NAME && set.hooks().is_some())
        }
        ToggleTarget::Rule => set
            .rules()
            .is_some_and(|rules| rules.rule_list().any(|(rule, _, _)| rule == name)),
    }
}

/// Every extension of `set`, the full set before toggles, with whether it runs.
pub(crate) fn statuses(set: &ExtensionSet, toggles: &ExtensionToggles) -> Vec<ExtensionStatus> {
    let mut statuses: Vec<ExtensionStatus> = set
        .kinds()
        .into_iter()
        .map(|(name, kind)| {
            let (phases, rules) = match kind {
                ExtensionKind::Rules => set.rules().map_or((Vec::new(), None), |rules| {
                    let phases = rules
                        .phases()
                        .iter()
                        .map(|p| p.as_str().to_owned())
                        .collect();
                    let list = rules
                        .rule_list()
                        .map(|(rule, phase, shadow)| {
                            let disabled_by = toggles.rule(rule);
                            RuleStatus {
                                name: rule.to_owned(),
                                phase: phase.as_str().to_owned(),
                                mode: mode_name(shadow).to_owned(),
                                enabled: disabled_by.is_none(),
                                disabled_by,
                            }
                        })
                        .collect();
                    (phases, Some(list))
                }),
                _ => (
                    builtin_phases(name)
                        .unwrap_or_default()
                        .iter()
                        .map(|phase| (*phase).to_owned())
                        .collect(),
                    None,
                ),
            };
            status(name, kind, phases, toggles, rules)
        })
        .collect();
    if let Some(hooks) = set.hooks() {
        let phases = hooks
            .phases()
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        statuses.push(status(
            HOOKS_NAME,
            ExtensionKind::Hook,
            phases,
            toggles,
            None,
        ));
    }
    statuses
}

fn status(
    name: &str,
    kind: ExtensionKind,
    phases: Vec<String>,
    toggles: &ExtensionToggles,
    rules: Option<Vec<RuleStatus>>,
) -> ExtensionStatus {
    let disabled_by = toggles.extension(name);
    ExtensionStatus {
        name: name.to_owned(),
        kind,
        phases,
        enabled: disabled_by.is_none(),
        disabled_by,
        rules,
    }
}

fn mode_name(shadow: bool) -> &'static str {
    if shadow { "shadow" } else { "enforce" }
}

/// A library extension named `team-gate`, for tests.
#[cfg(test)]
struct TeamGate;

#[cfg(test)]
impl crate::ext::Extension for TeamGate {
    fn name(&self) -> &'static str {
        "team-gate"
    }
}

/// Built-ins, the library extension `team-gate`, the rules `trial` (request, shadow) and `tag`
/// (attempt, sets `x-team: blue`), and one request hook.
#[cfg(test)]
pub(crate) fn sample_set() -> ExtensionSet {
    let env = |_: &str| None;
    let mut rules = crate::ext::rules::parse(
        "rules:\n  - {name: tag, phase: attempt, actions: [{set_header: {x-team: blue}}]}\n  - {name: trial, phase: request, mode: shadow, actions: [{reject: {status: 403, message: no}}]}\nhooks:\n  - {name: guard, phase: request, url: 'http://localhost:9/x'}\n",
        false,
        &env,
    )
    .unwrap_or_default();
    let hooks = rules.take_hooks();
    let mut all = crate::ext::builtin::builtins();
    all.push(std::sync::Arc::new(TeamGate));
    ExtensionSet::new(all)
        .with_rules(Some(rules))
        .with_hooks(hooks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::builtin::builtins;

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|entry| (*entry).to_owned()).collect()
    }

    fn env(list: &[&str]) -> ExtensionToggles {
        ExtensionToggles::load(&entries(list), None)
    }

    #[test]
    fn env_entries_split_into_extension_and_rule_globs() {
        let parsed = EnvDisabled::parse(&entries(&[
            "upstream-limits",
            " local-* ",
            "rules: no-*",
            "rules:",
            "",
        ]));

        assert_eq!(parsed.extensions, ["upstream-limits", "local-*"]);
        assert_eq!(parsed.rules, ["no-*"]);
    }

    #[test]
    fn env_names_and_globs_turn_extensions_and_rules_off() {
        let toggles = env(&["upstream-limits", "local-*", "rules:tr*"]);

        assert!(!toggles.is_empty());
        assert_eq!(toggles.extension("upstream-limits"), Some(DisabledBy::Env));
        assert_eq!(toggles.extension("local-not-ready"), Some(DisabledBy::Env));
        assert_eq!(toggles.extension("payload-too-large"), None);
        assert_eq!(toggles.extension("trial"), None);
        assert_eq!(toggles.rule("trial"), Some(DisabledBy::Env));
        assert_eq!(toggles.rule("tag"), None);
        assert!(env(&[]).is_empty());
    }

    #[test]
    fn a_ui_toggle_saves_and_the_env_wins_over_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.json");
        let store = Store::open("json", path.to_str()).unwrap();
        let toggles = env(&["upstream-limits"]);

        let off = toggles
            .toggled(ToggleTarget::Rule, "tag", false, Some(&store))
            .unwrap()
            .toggled(ToggleTarget::Extension, "team-gate", false, Some(&store))
            .unwrap();
        assert_eq!(off.rule("tag"), Some(DisabledBy::Ui));
        assert_eq!(off.extension("team-gate"), Some(DisabledBy::Ui));
        assert_eq!(ExtensionToggles::load(&[], Some(&store)), off.with_env(&[]));

        let error = off
            .toggled(
                ToggleTarget::Extension,
                "upstream-limits",
                true,
                Some(&store),
            )
            .unwrap_err();
        assert_eq!(error, ToggleError::SetByEnv("upstream-limits".into()));

        let on = off
            .toggled(ToggleTarget::Rule, "tag", true, Some(&store))
            .unwrap();
        assert_eq!(on.rule("tag"), None);
        let reloaded = ExtensionToggles::load(&entries(&["rules:tag"]), Some(&store));
        assert_eq!(reloaded.rule("tag"), Some(DisabledBy::Env));
        assert_eq!(reloaded.extension("team-gate"), Some(DisabledBy::Ui));
    }

    #[test]
    fn an_unchanged_toggle_saves_nothing_and_no_store_keeps_it_in_memory() {
        let toggles = env(&[]);

        let same = toggles
            .toggled(ToggleTarget::Extension, "team-gate", true, None)
            .unwrap();
        let off = toggles
            .toggled(ToggleTarget::Extension, "team-gate", false, None)
            .unwrap();

        assert!(same.is_empty());
        assert_eq!(off.extension("team-gate"), Some(DisabledBy::Ui));
    }

    #[test]
    fn unreadable_saved_toggles_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.json");
        let store = Store::open("json", path.to_str()).unwrap();
        store.set_install_setting(STORE_KEY, "not json").unwrap();

        assert!(ExtensionToggles::load(&[], Some(&store)).is_empty());
    }

    #[test]
    fn knows_names_every_extension_the_hooks_and_every_rule() {
        let set = sample_set();

        for name in ["upstream-limits", "team-gate", "rules", HOOKS_NAME] {
            assert!(knows(&set, ToggleTarget::Extension, name), "{name}");
        }
        assert!(!knows(&set, ToggleTarget::Extension, "nope"));
        assert!(knows(&set, ToggleTarget::Rule, "trial"));
        assert!(!knows(&set, ToggleTarget::Rule, "upstream-limits"));
        let bare = ExtensionSet::new(builtins());
        assert!(!knows(&bare, ToggleTarget::Extension, HOOKS_NAME));
        assert!(!knows(&bare, ToggleTarget::Rule, "trial"));
    }

    #[test]
    fn statuses_list_kinds_phases_rules_and_what_turned_each_off() {
        let toggles = env(&["upstream-limits", "rules:trial"])
            .toggled(ToggleTarget::Extension, HOOKS_NAME, false, None)
            .unwrap();

        let statuses = statuses(&sample_set(), &toggles);

        let names: Vec<(&str, ExtensionKind)> = statuses
            .iter()
            .map(|status| (status.name.as_str(), status.kind))
            .collect();
        assert_eq!(
            names,
            [
                ("copilot-context-code", ExtensionKind::Builtin),
                ("payload-too-large", ExtensionKind::Builtin),
                ("local-not-ready", ExtensionKind::Builtin),
                ("upstream-limits", ExtensionKind::Builtin),
                ("team-gate", ExtensionKind::Library),
                ("rules", ExtensionKind::Rules),
                ("hooks", ExtensionKind::Hook),
            ]
        );
        let limits = &statuses[3];
        assert_eq!(limits.phases, ["response"]);
        assert!(!limits.enabled);
        assert_eq!(limits.disabled_by, Some(DisabledBy::Env));
        assert!(statuses[4].phases.is_empty() && statuses[4].enabled);
        let rules = &statuses[5];
        assert_eq!(rules.phases, ["request", "attempt"]);
        assert_eq!(
            rules.rules.as_deref().unwrap(),
            [
                RuleStatus {
                    name: "trial".into(),
                    phase: "request".into(),
                    mode: "shadow".into(),
                    enabled: false,
                    disabled_by: Some(DisabledBy::Env),
                },
                RuleStatus {
                    name: "tag".into(),
                    phase: "attempt".into(),
                    mode: "enforce".into(),
                    enabled: true,
                    disabled_by: None,
                },
            ]
        );
        let hooks = &statuses[6];
        assert_eq!(hooks.phases, ["request"]);
        assert_eq!(hooks.disabled_by, Some(DisabledBy::Ui));
        assert!(hooks.rules.is_none());
    }
}
