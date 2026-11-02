use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use serde::Serialize;

use crate::schemas::app_error::AppError;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_rules::{SmartAutoRules, SmartAutoRulesMode};
use crate::services::smart_auto_pool::{RuleDecision, SmartAutoPool};

/// The rules file, kept in the same directory as the store.
pub const RULES_FILE: &str = "smart-auto-rules.json";

/// Where the active rules came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RulesSource {
    File,
    Env,
    None,
}

/// The active rules, with the policy they add built once at load time.
#[derive(Debug, Clone)]
pub struct LoadedRules {
    pub source: RulesSource,
    pub rules: SmartAutoRules,
    policy: SmartAutoPolicy,
}

impl LoadedRules {
    fn new(source: RulesSource, rules: SmartAutoRules) -> Self {
        let policy = rules.to_policy();
        Self {
            source,
            rules,
            policy,
        }
    }

    /// True when the environment owns the rules, so the API cannot change them.
    #[must_use]
    pub fn readonly(&self) -> bool {
        self.source == RulesSource::Env
    }

    /// The policy the active rules add, built once at load time.
    #[must_use]
    pub fn policy(&self) -> &SmartAutoPolicy {
        &self.policy
    }

    /// The policy a Smart Auto route filters with: the rules, then the selector, then the request.
    /// An override swaps the rules' include and exclude for the request's, keeping their allow; env is never replaced.
    #[must_use]
    pub fn layer(
        &self,
        mode: SmartAutoRulesMode,
        selector: &SmartAutoPolicy,
        request: &SmartAutoPolicy,
    ) -> SmartAutoPolicy {
        let base = match mode {
            SmartAutoRulesMode::Override if !self.readonly() => SmartAutoPolicy {
                include: request.include.clone(),
                exclude: request.exclude.clone(),
                allow: self.policy.allow.clone(),
                ..SmartAutoPolicy::default()
            },
            _ => self.policy.clone(),
        };
        base.merge(selector).merge(request)
    }
}

/// Why the rules admit or drop one model, in the order the checks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectReason {
    Eligible,
    AllowedOverride,
    ExcludedByGlobal,
    ExcludedByProfile,
    NotInProfileInclude,
    NotInGlobalInclude,
    NotChat,
    AutoAlias,
    HubNamespace,
}

/// One model's rule outcome. `matched` is the glob that decided it, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleEffect {
    pub qualified_id: String,
    pub eligible: bool,
    pub reason: EffectReason,
    pub matched: Option<String>,
}

/// The rule outcome of every descriptor, sorted by qualified id. Pure: no request state.
/// It judges the rules alone, not the endpoint, token minima or prompt fit that also gate a route.
#[must_use]
pub fn rule_effects(
    snapshot: &ModelDescriptorSnapshot,
    policy: &SmartAutoPolicy,
) -> Vec<RuleEffect> {
    let mut rows: Vec<RuleEffect> = snapshot
        .iter()
        .map(|descriptor| rule_effect(descriptor, policy))
        .collect();
    rows.sort_unstable_by(|left, right| left.qualified_id.cmp(&right.qualified_id));
    rows
}

fn rule_effect(descriptor: &ModelDescriptor, policy: &SmartAutoPolicy) -> RuleEffect {
    let (reason, matched) = match SmartAutoPool::rule_decision(descriptor, policy) {
        RuleDecision::NotChat => (EffectReason::NotChat, None),
        RuleDecision::AutoAlias => (EffectReason::AutoAlias, None),
        RuleDecision::HubNamespace => (EffectReason::HubNamespace, None),
        RuleDecision::AllowedOverride(glob) => (EffectReason::AllowedOverride, Some(glob.clone())),
        RuleDecision::Admitted(matched) => (EffectReason::Eligible, matched.cloned()),
        RuleDecision::ExcludedByGlobal(glob) => {
            (EffectReason::ExcludedByGlobal, Some(glob.clone()))
        }
        RuleDecision::ExcludedByProfile(glob) => {
            (EffectReason::ExcludedByProfile, Some(glob.clone()))
        }
        RuleDecision::NotInProfileInclude => (EffectReason::NotInProfileInclude, None),
        RuleDecision::NotInGlobalInclude => (EffectReason::NotInGlobalInclude, None),
    };
    RuleEffect {
        qualified_id: descriptor.qualified_id.clone(),
        eligible: matches!(
            reason,
            EffectReason::Eligible | EffectReason::AllowedOverride
        ),
        reason,
        matched,
    }
}

/// The active Smart Auto rules and the atomic writer of the rules file.
#[derive(Debug)]
pub struct SmartAutoRulesStore {
    path: PathBuf,
    active: ArcSwap<LoadedRules>,
    write_lock: Mutex<()>,
}

impl SmartAutoRulesStore {
    /// Loads the env rules when `env` is set, else the rules file in `dir`. A missing file means no rules.
    ///
    /// # Errors
    /// Returns a message when the env JSON, the file or its read is invalid.
    pub fn open(dir: &Path, env: Option<&str>) -> Result<Self, String> {
        let path = dir.join(RULES_FILE);
        let loaded = match env.map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => {
                let rules = SmartAutoRules::from_json(raw.as_bytes())
                    .map_err(|e| format!("LLM_HUB_SMART_AUTO_RULES: {e}"))?;
                LoadedRules::new(RulesSource::Env, rules)
            }
            None => Self::read_file(&path)?,
        };
        Ok(Self {
            path,
            active: ArcSwap::from_pointee(loaded),
            write_lock: Mutex::new(()),
        })
    }

    fn read_file(path: &Path) -> Result<LoadedRules, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let rules = SmartAutoRules::from_json(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(LoadedRules::new(RulesSource::File, rules))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(LoadedRules::new(
                RulesSource::None,
                SmartAutoRules::default(),
            )),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The active rules. Requests read this snapshot, never the file.
    #[must_use]
    pub fn current(&self) -> Arc<LoadedRules> {
        self.active.load_full()
    }

    /// Validates the JSON in `raw`, saves it atomically and makes it the active rules.
    ///
    /// # Errors
    /// `Conflict` when env owns the rules, `BadRequest` for invalid JSON or globs, `Internal` when the write fails.
    pub fn replace(&self, raw: &[u8]) -> Result<Arc<LoadedRules>, AppError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.current().readonly() {
            return Err(AppError::Conflict(
                "Smart Auto rules come from LLM_HUB_SMART_AUTO_RULES and are read-only".into(),
            ));
        }
        let rules = SmartAutoRules::from_json(raw).map_err(AppError::BadRequest)?;
        let mut bytes = serde_json::to_vec_pretty(&rules)
            .map_err(|e| AppError::Internal(format!("cannot encode the rules: {e}")))?;
        bytes.push(b'\n');
        write_atomically(&self.path, &bytes)
            .map_err(|e| AppError::Internal(format!("cannot save {}: {e}", self.path.display())))?;
        let loaded = Arc::new(LoadedRules::new(RulesSource::File, rules));
        self.active.store(Arc::clone(&loaded));
        Ok(loaded)
    }
}

/// Writes a temp file beside `path`, then renames it over `path`, so readers see old or new, never half.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let temp = dir.join(format!(".{RULES_FILE}.{}.tmp", std::process::id()));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COPILOT_RULES: &[u8] = br#"{"include":["copilot/*"]}"#;

    fn eligible(policy: &SmartAutoPolicy, ids: &[&str]) -> Vec<String> {
        let snapshot = ModelDescriptorSnapshot::new(
            ids.iter().map(|id| ModelDescriptor::for_test(id)).collect(),
        );
        SmartAutoPool::eligible(&snapshot, policy, "/v1/messages", 0)
            .into_iter()
            .map(|descriptor| descriptor.qualified_id.clone())
            .collect()
    }

    fn request_include(glob: &str) -> SmartAutoPolicy {
        SmartAutoPolicy {
            include: vec![[glob.to_string()].into()],
            ..SmartAutoPolicy::default()
        }
    }

    #[test]
    fn missing_file_means_no_rules_and_a_save_writes_it_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        assert_eq!(store.current().source, RulesSource::None);
        assert_eq!(store.current().rules, SmartAutoRules::default());

        let saved = store.replace(COPILOT_RULES).unwrap();

        assert_eq!(saved.source, RulesSource::File);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [RULES_FILE]);
        let reopened = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        assert_eq!(reopened.current().source, RulesSource::File);
        assert_eq!(reopened.current().rules, saved.rules);
    }

    #[test]
    fn a_corrupt_file_fails_startup_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RULES_FILE);
        std::fs::write(&path, b"{not json").unwrap();

        let error = SmartAutoRulesStore::open(dir.path(), None).unwrap_err();

        assert!(error.contains(RULES_FILE), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
    }

    #[test]
    fn env_rules_are_read_only_and_win_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RULES_FILE), COPILOT_RULES).unwrap();
        let env = r#"{"exclude":["*preview*"]}"#;

        let store = SmartAutoRulesStore::open(dir.path(), Some(env)).unwrap();

        assert_eq!(store.current().source, RulesSource::Env);
        assert!(store.current().readonly());
        assert!(store.current().rules.include.is_empty());
        let error = store.replace(COPILOT_RULES).unwrap_err();
        assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
        assert_eq!(
            std::fs::read(dir.path().join(RULES_FILE)).unwrap(),
            COPILOT_RULES
        );
    }

    #[test]
    fn invalid_env_rules_fail_startup() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            SmartAutoRulesStore::open(dir.path(), Some(r#"{"include":[" "]}"#)).unwrap_err();
        assert!(error.starts_with("LLM_HUB_SMART_AUTO_RULES"), "{error}");
    }

    #[test]
    fn bad_bodies_are_bad_requests_and_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        for raw in [
            br#"{"include":[""]}"#.as_slice(),
            br#"{"exclude":["a b"]}"#.as_slice(),
            b"[".as_slice(),
            br#"{"unknown":1}"#.as_slice(),
        ] {
            let error = store.replace(raw).unwrap_err();
            assert!(matches!(error, AppError::BadRequest(_)), "{error:?}");
        }
        assert!(!dir.path().join(RULES_FILE).exists());
        assert_eq!(store.current().source, RulesSource::None);
    }

    #[test]
    fn override_widens_only_without_env_and_merge_only_narrows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RULES_FILE), COPILOT_RULES).unwrap();
        let models = ["copilot/gpt-6-sol", "llama_swap/qwen3.6-35b"];
        let selector = SmartAutoPolicy::default();
        let request = request_include("llama_swap/*");

        let file = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        let merged = file
            .current()
            .layer(SmartAutoRulesMode::Merge, &selector, &request);
        let overridden = file
            .current()
            .layer(SmartAutoRulesMode::Override, &selector, &request);
        assert!(eligible(&merged, &models).is_empty());
        assert_eq!(eligible(&overridden, &models), ["llama_swap/qwen3.6-35b"]);

        let env =
            SmartAutoRulesStore::open(dir.path(), Some(r#"{"include":["copilot/*"]}"#)).unwrap();
        let blocked = env
            .current()
            .layer(SmartAutoRulesMode::Override, &selector, &request);
        assert!(eligible(&blocked, &models).is_empty());
    }

    #[test]
    fn override_replaces_the_profile_rules_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(RULES_FILE),
            br#"{"profiles":{"copilot":{"include":["claude-*"]}}}"#,
        )
        .unwrap();
        let models = ["copilot/claude-opus-4.7", "copilot/gpt-6-sol"];
        let store = SmartAutoRulesStore::open(dir.path(), None).unwrap();

        let merged = store.current().layer(
            SmartAutoRulesMode::Merge,
            &SmartAutoPolicy::default(),
            &SmartAutoPolicy::default(),
        );
        let overridden = store.current().layer(
            SmartAutoRulesMode::Override,
            &SmartAutoPolicy::default(),
            &SmartAutoPolicy::default(),
        );

        assert_eq!(eligible(&merged, &models), ["copilot/claude-opus-4.7"]);
        assert_eq!(eligible(&overridden, &models), models);
    }

    #[test]
    fn deny_wins_over_an_include_in_the_same_layer() {
        let dir = tempfile::tempdir().unwrap();
        let store = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        let request = SmartAutoPolicy {
            include: vec![["copilot/*".to_string()].into()],
            exclude: ["copilot/gpt-6-sol".to_string()].into(),
            ..SmartAutoPolicy::default()
        };

        let policy = store.current().layer(
            SmartAutoRulesMode::Override,
            &SmartAutoPolicy::default(),
            &request,
        );

        assert_eq!(
            eligible(&policy, &["copilot/gpt-6-sol", "copilot/claude-opus-4.7"]),
            ["copilot/claude-opus-4.7"]
        );
    }

    fn effects(policy: &SmartAutoPolicy, models: Vec<ModelDescriptor>) -> Vec<RuleEffect> {
        rule_effects(&ModelDescriptorSnapshot::new(models), policy)
    }

    fn rules_policy(json: &str) -> SmartAutoPolicy {
        SmartAutoRules::from_json(json.as_bytes())
            .unwrap()
            .to_policy()
    }

    fn outcome(rows: &[RuleEffect]) -> Vec<(&str, EffectReason, Option<&str>)> {
        rows.iter()
            .map(|row| {
                (
                    row.qualified_id.as_str(),
                    row.reason,
                    row.matched.as_deref(),
                )
            })
            .collect()
    }

    #[test]
    fn effect_names_the_rule_and_glob_that_decided_each_model() {
        let policy = rules_policy(
            r#"{"include":["copilot/*"],"exclude":["*preview*"],
                "profiles":{"copilot":{"include":["gpt-*"],"exclude":["claude-haiku*"]}}}"#,
        );
        let models = vec![
            ModelDescriptor::for_test("copilot/gpt-6-sol"),
            ModelDescriptor::for_test("copilot/gpt-6-preview"),
            ModelDescriptor::for_test("copilot/claude-haiku-4.5"),
            ModelDescriptor::for_test("copilot/claude-opus-4.7"),
            ModelDescriptor::for_test("copilot/auto"),
            ModelDescriptor::for_test("copilot/auto-fast"),
            ModelDescriptor::for_test("hub/smart-auto-balance"),
            ModelDescriptor::for_test("llama_swap/qwen3.6-35b"),
            ModelDescriptor {
                chat: false,
                ..ModelDescriptor::for_test("ivrit/whisper")
            },
        ];

        let rows = effects(&policy, models);

        assert_eq!(
            outcome(&rows),
            [
                ("copilot/auto", EffectReason::AutoAlias, None),
                ("copilot/auto-fast", EffectReason::AutoAlias, None),
                (
                    "copilot/claude-haiku-4.5",
                    EffectReason::ExcludedByProfile,
                    Some("claude-haiku*")
                ),
                (
                    "copilot/claude-opus-4.7",
                    EffectReason::NotInProfileInclude,
                    None
                ),
                (
                    "copilot/gpt-6-preview",
                    EffectReason::ExcludedByGlobal,
                    Some("*preview*")
                ),
                ("copilot/gpt-6-sol", EffectReason::Eligible, Some("gpt-*")),
                ("hub/smart-auto-balance", EffectReason::HubNamespace, None),
                ("ivrit/whisper", EffectReason::NotChat, None),
                (
                    "llama_swap/qwen3.6-35b",
                    EffectReason::NotInGlobalInclude,
                    None
                ),
            ]
        );
        assert!(rows.iter().all(|row| row.eligible
            == matches!(
                row.reason,
                EffectReason::Eligible | EffectReason::AllowedOverride
            )));
    }

    #[test]
    fn not_chat_then_auto_then_hub_outrank_every_rule() {
        let policy = rules_policy(r#"{"exclude":["*"]}"#);
        let models = vec![
            ModelDescriptor {
                chat: false,
                ..ModelDescriptor::for_test("hub/speech")
            },
            ModelDescriptor {
                chat: false,
                ..ModelDescriptor::for_test("copilot/tts")
            },
            ModelDescriptor::for_test("copilot/auto"),
            ModelDescriptor::for_test("hub/smart-auto-balance"),
            ModelDescriptor::for_test("copilot/gpt-6-sol"),
        ];

        let rows = effects(&policy, models);

        assert_eq!(
            outcome(&rows),
            [
                ("copilot/auto", EffectReason::AutoAlias, None),
                (
                    "copilot/gpt-6-sol",
                    EffectReason::ExcludedByGlobal,
                    Some("*")
                ),
                ("copilot/tts", EffectReason::NotChat, None),
                ("hub/smart-auto-balance", EffectReason::HubNamespace, None),
                ("hub/speech", EffectReason::NotChat, None),
            ]
        );
    }

    #[test]
    fn deny_wins_over_an_include_and_the_1m_alias_is_ignored_on_globs() {
        let policy = rules_policy(r#"{"include":["copilot/*"],"exclude":["claude-opus-4.7[1m]"]}"#);
        let models = vec![
            ModelDescriptor::for_test("copilot/claude-opus-4.7"),
            ModelDescriptor::for_test("copilot/gpt-6-sol"),
        ];

        let rows = effects(&policy, models);

        assert_eq!(
            outcome(&rows),
            [
                (
                    "copilot/claude-opus-4.7",
                    EffectReason::ExcludedByGlobal,
                    Some("claude-opus-4.7[1m]")
                ),
                (
                    "copilot/gpt-6-sol",
                    EffectReason::Eligible,
                    Some("copilot/*")
                ),
            ]
        );
    }

    #[test]
    fn no_rules_make_every_concrete_chat_model_eligible_without_a_glob() {
        let models = vec![
            ModelDescriptor::for_test("llama_swap/qwen3.6-35b"),
            ModelDescriptor::for_test("copilot/gpt-6-sol"),
        ];

        let rows = effects(&SmartAutoPolicy::default(), models);

        assert_eq!(
            outcome(&rows),
            [
                ("copilot/gpt-6-sol", EffectReason::Eligible, None),
                ("llama_swap/qwen3.6-35b", EffectReason::Eligible, None),
            ]
        );
    }

    #[test]
    fn effect_eligibility_matches_the_pool_on_the_same_rules() {
        let models = || {
            vec![
                ModelDescriptor::for_test("copilot/gpt-6-sol"),
                ModelDescriptor::for_test("copilot/gpt-6-preview"),
                ModelDescriptor::for_test("copilot/claude-opus-4.7"),
                ModelDescriptor::for_test("copilot/auto"),
                ModelDescriptor::for_test("llama_swap/gpt-6-sol"),
                ModelDescriptor::for_test("llama_swap/qwen3.6-35b"),
                ModelDescriptor::for_test("hub/smart-auto-balance"),
                ModelDescriptor {
                    chat: false,
                    ..ModelDescriptor::for_test("ivrit/whisper")
                },
            ]
        };
        let policies = [
            SmartAutoPolicy::default(),
            rules_policy(r#"{"include":["copilot/*","llama_swap/*"],"exclude":["*preview*"]}"#),
            rules_policy(
                r#"{"profiles":{"copilot":{"include":["claude-*","gpt-*"],"exclude":["gpt-6-sol"]}}}"#,
            ),
            rules_policy(
                r#"{"include":["*gpt*","*/qwen*"],"profiles":{"llama_swap":{"exclude":["*qwen*"]}}}"#,
            ),
            rules_policy(
                r#"{"exclude":["*preview*"],"allow":["copilot/gpt-6-preview"],"profiles":{"llama_swap":{"exclude":["*qwen*"]}}}"#,
            ),
        ];

        for policy in &policies {
            let snapshot = ModelDescriptorSnapshot::new(models());
            let pooled: Vec<String> = SmartAutoPool::eligible(&snapshot, policy, "/v1/messages", 0)
                .into_iter()
                .map(|descriptor| descriptor.qualified_id.clone())
                .collect();
            let effect: Vec<String> = rule_effects(&snapshot, policy)
                .into_iter()
                .filter(|row| row.eligible)
                .map(|row| row.qualified_id)
                .collect();
            assert_eq!(effect, pooled, "{policy:?}");
        }
    }

    #[test]
    fn allow_beats_global_and_profile_excludes_and_names_its_glob() {
        let policy = rules_policy(
            r#"{"exclude":["*preview*"],"allow":["claude-haiku-4.5","gpt-6-preview"],
                "profiles":{"copilot":{"exclude":["claude-haiku*"]}}}"#,
        );
        let models = vec![
            ModelDescriptor::for_test("copilot/claude-haiku-4.5"),
            ModelDescriptor::for_test("copilot/gpt-6-preview"),
            ModelDescriptor::for_test("copilot/grok-preview"),
        ];

        let rows = effects(&policy, models);

        assert_eq!(
            outcome(&rows),
            [
                (
                    "copilot/claude-haiku-4.5",
                    EffectReason::AllowedOverride,
                    Some("claude-haiku-4.5")
                ),
                (
                    "copilot/gpt-6-preview",
                    EffectReason::AllowedOverride,
                    Some("gpt-6-preview")
                ),
                (
                    "copilot/grok-preview",
                    EffectReason::ExcludedByGlobal,
                    Some("*preview*")
                ),
            ]
        );
        assert_eq!(rows.iter().filter(|row| row.eligible).count(), 2);
    }

    #[test]
    fn allow_outranks_exclude_but_not_not_chat_auto_or_hub() {
        let policy = rules_policy(r#"{"exclude":["*"],"allow":["*"]}"#);
        let models = vec![
            ModelDescriptor {
                chat: false,
                ..ModelDescriptor::for_test("hub/speech")
            },
            ModelDescriptor::for_test("copilot/auto"),
            ModelDescriptor::for_test("hub/smart-auto-balance"),
            ModelDescriptor::for_test("copilot/gpt-6-sol"),
        ];

        let rows = effects(&policy, models);

        assert_eq!(
            outcome(&rows),
            [
                ("copilot/auto", EffectReason::AutoAlias, None),
                (
                    "copilot/gpt-6-sol",
                    EffectReason::AllowedOverride,
                    Some("*")
                ),
                ("hub/smart-auto-balance", EffectReason::HubNamespace, None),
                ("hub/speech", EffectReason::NotChat, None),
            ]
        );
    }

    #[test]
    fn env_rules_replace_the_file_allow_and_an_env_allow_survives_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(RULES_FILE),
            br#"{"exclude":["*preview*"],"allow":["copilot/gpt-6-preview"]}"#,
        )
        .unwrap();
        let env_only =
            SmartAutoRulesStore::open(dir.path(), Some(r#"{"exclude":["*preview*"]}"#)).unwrap();
        assert!(env_only.current().rules.allow.is_empty());
        let models = ["copilot/gpt-6-preview", "copilot/gpt-6-sol"];
        let policy = env_only.current().layer(
            SmartAutoRulesMode::Merge,
            &SmartAutoPolicy::default(),
            &SmartAutoPolicy::default(),
        );
        assert_eq!(eligible(&policy, &models), ["copilot/gpt-6-sol"]);

        let env_allow = SmartAutoRulesStore::open(
            dir.path(),
            Some(r#"{"exclude":["*"],"allow":["copilot/gpt-6-sol"]}"#),
        )
        .unwrap();
        let policy = env_allow.current().layer(
            SmartAutoRulesMode::Override,
            &SmartAutoPolicy::default(),
            &request_include("copilot/*"),
        );
        assert_eq!(
            eligible(&policy, &["copilot/gpt-6-sol", "copilot/claude-opus-4.7"]),
            ["copilot/gpt-6-sol"]
        );
    }

    #[test]
    fn override_keeps_the_rules_allow_so_a_request_exclude_cannot_remove_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(RULES_FILE),
            br#"{"include":["copilot/*"],"exclude":["*preview*"],"allow":["copilot/gpt-6-preview"]}"#,
        )
        .unwrap();
        let store = SmartAutoRulesStore::open(dir.path(), None).unwrap();
        let models = [
            "copilot/gpt-6-preview",
            "copilot/grok-preview",
            "llama_swap/qwen3.6-35b",
        ];
        let request = SmartAutoPolicy {
            include: vec![["*".to_string()].into()],
            exclude: ["copilot/gpt-6-preview".to_string()].into(),
            ..SmartAutoPolicy::default()
        };

        let merged = store.current().layer(
            SmartAutoRulesMode::Merge,
            &SmartAutoPolicy::default(),
            &request,
        );
        let overridden = store.current().layer(
            SmartAutoRulesMode::Override,
            &SmartAutoPolicy::default(),
            &request,
        );

        assert_eq!(eligible(&merged, &models), ["copilot/gpt-6-preview"]);
        assert_eq!(
            eligible(&overridden, &models),
            [
                "copilot/gpt-6-preview",
                "copilot/grok-preview",
                "llama_swap/qwen3.6-35b"
            ]
        );
    }
}
