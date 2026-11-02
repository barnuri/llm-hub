use sha2::{Digest, Sha256};

use crate::schemas::smart_auto_headers::SmartAutoHeaders;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::turn_signals::TurnSignals;

/// A SHA-256 over one conversation's anchor, its worker, its policy and its endpoint.
/// It holds no request content, and two workers or two restriction sets never share a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AffinityKey([u8; 32]);

impl AffinityKey {
    const VERSION: &[u8] = b"smart-auto-affinity-v1";
    const ANCHOR_SESSION: u8 = 1;
    const ANCHOR_PROMPT_CACHE_KEY: u8 = 2;
    const ANCHOR_CONTENT: u8 = 3;
    const ANCHOR_WORKER: u8 = 4;
    const ABSENT: u8 = 0;
    const PRESENT: u8 = 1;

    /// Builds the key from the first anchor of session id, `prompt_cache_key` and content hash.
    /// Returns `None` when the request has no anchor and names no worker.
    #[must_use]
    pub fn new(
        signals: &TurnSignals,
        caller: &SmartAutoHeaders,
        policy: &SmartAutoPolicy,
        endpoint: &str,
    ) -> Option<AffinityKey> {
        let (anchor_kind, anchor): (u8, &[u8]) = if let Some(session) = &signals.session_id {
            (Self::ANCHOR_SESSION, session.as_bytes())
        } else if let Some(cache_key) = &signals.prompt_cache_key {
            (Self::ANCHOR_PROMPT_CACHE_KEY, cache_key.as_bytes())
        } else if let Some(content) = &signals.content_key {
            (Self::ANCHOR_CONTENT, content.as_slice())
        } else {
            (Self::ANCHOR_WORKER, caller.worker.as_ref()?.as_bytes())
        };
        let mut hasher = Sha256::new();
        Self::part(&mut hasher, Self::VERSION);
        Self::part(&mut hasher, &[anchor_kind]);
        Self::part(&mut hasher, anchor);
        Self::optional(&mut hasher, caller.worker.as_deref());
        Self::optional(&mut hasher, caller.parent.as_deref());
        Self::optional(&mut hasher, caller.policy_handle.as_deref());
        Self::part(&mut hasher, endpoint.as_bytes());
        Self::restrictions(&mut hasher, policy);
        Some(AffinityKey(hasher.finalize().into()))
    }

    /// The role hint is left out: it selects an objective and never changes eligibility.
    fn restrictions(hasher: &mut Sha256, policy: &SmartAutoPolicy) {
        Self::part(hasher, policy.tier.unwrap_or_default().as_str().as_bytes());
        Self::part(hasher, &(policy.include.len() as u64).to_le_bytes());
        for allow_list in &policy.include {
            Self::globs(hasher, allow_list.iter());
        }
        Self::globs(hasher, policy.exclude.iter());
        for minimum in [
            policy.min_context_tokens,
            policy.min_input_tokens,
            policy.min_output_tokens,
        ] {
            Self::part(
                hasher,
                &minimum
                    .map_or(0, |tokens| tokens.saturating_add(1))
                    .to_le_bytes(),
            );
        }
    }

    fn globs<'a>(hasher: &mut Sha256, globs: impl ExactSizeIterator<Item = &'a String>) {
        Self::part(hasher, &(globs.len() as u64).to_le_bytes());
        for glob in globs {
            Self::part(hasher, glob.as_bytes());
        }
    }

    fn optional(hasher: &mut Sha256, value: Option<&str>) {
        match value {
            Some(text) => {
                Self::part(hasher, &[Self::PRESENT]);
                Self::part(hasher, text.as_bytes());
            }
            None => Self::part(hasher, &[Self::ABSENT]),
        }
    }

    fn part(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::schemas::smart_auto_tier::SmartAutoTier;
    use crate::schemas::work_role::WorkRole;

    const MESSAGES: &str = "/v1/messages";

    fn signals(session: Option<&str>, cache_key: Option<&str>, content: Option<u8>) -> TurnSignals {
        TurnSignals {
            session_id: session.map(str::to_string),
            prompt_cache_key: cache_key.map(str::to_string),
            content_key: content.map(|byte| [byte; 32]),
            ..TurnSignals::for_test()
        }
    }

    fn worker(worker: Option<&str>, parent: Option<&str>) -> SmartAutoHeaders {
        SmartAutoHeaders {
            worker: worker.map(str::to_string),
            parent: parent.map(str::to_string),
            ..SmartAutoHeaders::default()
        }
    }

    fn key(
        signals: &TurnSignals,
        caller: &SmartAutoHeaders,
        policy: &SmartAutoPolicy,
    ) -> AffinityKey {
        AffinityKey::new(signals, caller, policy, MESSAGES).unwrap()
    }

    fn plain(signals: &TurnSignals) -> AffinityKey {
        key(
            signals,
            &SmartAutoHeaders::default(),
            &SmartAutoPolicy::default(),
        )
    }

    #[test]
    fn session_id_wins_over_cache_key_and_content() {
        let full = plain(&signals(Some("s1"), Some("c1"), Some(1)));

        assert_eq!(full, plain(&signals(Some("s1"), Some("c2"), Some(2))));
        assert_ne!(full, plain(&signals(Some("s2"), Some("c1"), Some(1))));
    }

    #[test]
    fn cache_key_wins_over_content() {
        let cached = plain(&signals(None, Some("c1"), Some(1)));

        assert_eq!(cached, plain(&signals(None, Some("c1"), Some(2))));
        assert_ne!(cached, plain(&signals(None, Some("c2"), Some(1))));
    }

    #[test]
    fn content_is_the_last_anchor() {
        let content = plain(&signals(None, None, Some(1)));

        assert_eq!(content, plain(&signals(None, None, Some(1))));
        assert_ne!(content, plain(&signals(None, None, Some(2))));
    }

    #[test]
    fn equal_anchor_text_of_different_kinds_gives_different_keys() {
        assert_ne!(
            plain(&signals(Some("same"), None, None)),
            plain(&signals(None, Some("same"), None))
        );
    }

    #[test]
    fn no_anchor_and_no_worker_gives_no_key() {
        let none = signals(None, None, None);
        let policy = SmartAutoPolicy::default();

        assert_eq!(
            AffinityKey::new(&none, &SmartAutoHeaders::default(), &policy, MESSAGES),
            None
        );
        assert!(AffinityKey::new(&none, &worker(Some("w1"), None), &policy, MESSAGES).is_some());
    }

    #[test]
    fn two_workers_under_one_parent_and_session_get_different_keys() {
        let shared = signals(Some("s1"), None, None);
        let policy = SmartAutoPolicy::default();

        let first = key(&shared, &worker(Some("run/a/1"), Some("run")), &policy);
        let second = key(&shared, &worker(Some("run/b/1"), Some("run")), &policy);
        let main = key(&shared, &worker(None, None), &policy);

        assert_ne!(first, second);
        assert_ne!(first, main);
        assert_eq!(
            first,
            key(&shared, &worker(Some("run/a/1"), Some("run")), &policy)
        );
    }

    #[test]
    fn tier_restrictions_and_endpoint_scope_the_key() {
        let shared = signals(Some("s1"), None, None);
        let caller = SmartAutoHeaders::default();
        let balance = SmartAutoPolicy::default();
        let fast = SmartAutoPolicy {
            tier: Some(SmartAutoTier::Fast),
            ..SmartAutoPolicy::default()
        };
        let excluding = SmartAutoPolicy {
            exclude: BTreeSet::from(["copilot/*".to_string()]),
            ..SmartAutoPolicy::default()
        };
        let long = SmartAutoPolicy {
            min_context_tokens: Some(1_000_000),
            ..SmartAutoPolicy::default()
        };
        let base = key(&shared, &caller, &balance);

        for other in [&fast, &excluding, &long] {
            assert_ne!(base, key(&shared, &caller, other));
        }
        assert_ne!(
            base,
            AffinityKey::new(&shared, &caller, &balance, "/v1/chat/completions").unwrap()
        );
    }

    #[test]
    fn role_hint_does_not_change_the_key() {
        let shared = signals(Some("s1"), None, None);
        let planning = SmartAutoPolicy {
            role: Some(WorkRole::Planning),
            ..SmartAutoPolicy::default()
        };

        assert_eq!(
            plain(&shared),
            key(&shared, &SmartAutoHeaders::default(), &planning)
        );
    }
}
