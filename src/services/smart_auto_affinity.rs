use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::turn_signals::TurnSignals;
use crate::schemas::work_role::WorkRole;
use crate::services::copilot_auto::AutoTier;

/// The in-memory Smart Auto model pins, bounded by count and idle time.
/// Every call is O(1) in the map, takes `now` from the caller and holds the lock for no await.
#[derive(Debug)]
pub struct SmartAutoAffinity {
    table: Mutex<PinTable>,
}

#[derive(Debug, Default)]
struct PinTable {
    entries: HashMap<AffinityKey, PinEntry>,
    // Commit order for eviction. A record whose sequence is not its entry's latest is stale.
    order: VecDeque<(AffinityKey, u64)>,
    next_sequence: u64,
}

#[derive(Debug)]
struct PinEntry {
    active: Option<ActivePin>,
    incumbents: Vec<(WorkRole, String)>,
    switch_turns: VecDeque<usize>,
    last_used: Instant,
    sequence: u64,
}

#[derive(Debug)]
struct ActivePin {
    model: String,
    role: WorkRole,
    plan_backed: bool,
    since_turn: usize,
}

/// How much a role needs from its model. A switch toward `Light` never pays for a cold cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleWeight {
    Light,
    Cheap,
    Heavy,
}

impl SmartAutoAffinity {
    const CAPACITY: usize = 1024;
    const IDLE: Duration = Duration::from_mins(10);
    const MIN_DWELL_TURNS: usize = 2;
    const SOFT_SWITCH_BUDGET: usize = 2;
    const SOFT_SWITCH_WINDOW_TURNS: usize = 20;
    const PROFILE_SEPARATOR: char = '/';

    /// An empty store.
    #[must_use]
    pub fn new() -> SmartAutoAffinity {
        SmartAutoAffinity {
            table: Mutex::new(PinTable::default()),
        }
    }

    /// The pinned model, role and plan backing, only while the pin is fresh and in `eligible`.
    /// An idle entry is removed, and an ineligible pin is dropped, so deny always wins.
    #[must_use]
    pub fn lookup(
        &self,
        key: &AffinityKey,
        eligible: &[&ModelDescriptor],
        now: Instant,
    ) -> Option<(String, WorkRole, bool)> {
        let mut table = self.lock();
        let entry = table.live_entry(key, now)?;
        let pin = entry.active.as_ref()?;
        if !Self::is_eligible(&pin.model, eligible) {
            entry.active = None;
            return None;
        }
        Some((pin.model.clone(), pin.role, pin.plan_backed))
    }

    /// The model this conversation last used for `role`, only while it is in `eligible`.
    /// The router reuses it when the conversation returns to that role.
    #[must_use]
    pub fn incumbent(
        &self,
        key: &AffinityKey,
        role: WorkRole,
        eligible: &[&ModelDescriptor],
        now: Instant,
    ) -> Option<String> {
        let mut table = self.lock();
        let entry = table.live_entry(key, now)?;
        entry
            .incumbents
            .iter()
            .find(|(incumbent_role, model)| {
                *incumbent_role == role && Self::is_eligible(model, eligible)
            })
            .map(|(_, model)| model.clone())
    }

    /// Whether an optional switch is allowed at user turn `turn`. It needs the minimum dwell
    /// on the pin and a free slot in the rolling soft-switch budget. A forced switch skips it.
    #[must_use]
    pub fn soft_switch_allowed(&self, key: &AffinityKey, turn: usize, now: Instant) -> bool {
        let mut table = self.lock();
        let Some(entry) = table.live_entry(key, now) else {
            return true;
        };
        let Some(pin) = &entry.active else {
            return true;
        };
        let dwelled = turn.saturating_sub(pin.since_turn) >= Self::MIN_DWELL_TURNS;
        let recent_switches = entry
            .switch_turns
            .iter()
            .filter(|switched| turn.saturating_sub(**switched) < Self::SOFT_SWITCH_WINDOW_TURNS)
            .count();
        dwelled && recent_switches < Self::SOFT_SWITCH_BUDGET
    }

    /// Pins `model` for `role` and its plan backing after a successful response at turn `turn`.
    /// Returns `false` and pins nothing for an Auto alias, a `hub/*` id or an unqualified id.
    pub fn commit(
        &self,
        key: &AffinityKey,
        model: &str,
        role: WorkRole,
        plan_backed: bool,
        turn: usize,
        now: Instant,
    ) -> bool {
        if !Self::is_pinnable(model) {
            return false;
        }
        let mut table = self.lock();
        let sequence = table.record(*key);
        let entry = table.entries.entry(*key).or_insert_with(|| PinEntry {
            active: None,
            incumbents: Vec::new(),
            switch_turns: VecDeque::new(),
            last_used: now,
            sequence,
        });
        entry.last_used = now;
        entry.sequence = sequence;
        let since_turn = match &entry.active {
            Some(pin) if pin.model == model => pin.since_turn,
            Some(_) => {
                Self::record_switch(&mut entry.switch_turns, turn);
                turn
            }
            None => turn,
        };
        entry.active = Some(ActivePin {
            model: model.to_string(),
            role,
            plan_backed,
            since_turn,
        });
        match entry
            .incumbents
            .iter_mut()
            .find(|(incumbent_role, _)| *incumbent_role == role)
        {
            Some((_, incumbent)) => model.clone_into(incumbent),
            None => entry.incumbents.push((role, model.to_string())),
        }
        table.compact();
        table.evict_over_capacity(key);
        true
    }

    /// Whether the main solver leaves its pin for a turn now classified as `new_role`.
    /// A tool continuation stays. Only a material role change moves, and only when `settled`.
    #[must_use]
    pub fn should_switch(
        pinned_role: WorkRole,
        pinned_plan_backed: bool,
        new_role: WorkRole,
        new_plan_backed: bool,
        signals: &TurnSignals,
        settled: bool,
    ) -> bool {
        if signals.tool_continuation || !settled {
            return false;
        }
        let pinned_weight = Self::weight(pinned_role, pinned_plan_backed);
        let new_weight = Self::weight(new_role, new_plan_backed);
        new_weight != pinned_weight && new_weight != RoleWeight::Light
    }

    fn weight(role: WorkRole, plan_backed: bool) -> RoleWeight {
        match role {
            WorkRole::Conversation => RoleWeight::Light,
            WorkRole::Implementation if plan_backed => RoleWeight::Cheap,
            WorkRole::Implementation
            | WorkRole::Planning
            | WorkRole::Investigation
            | WorkRole::Validation => RoleWeight::Heavy,
        }
    }

    fn record_switch(switch_turns: &mut VecDeque<usize>, turn: usize) {
        switch_turns.push_back(turn);
        while switch_turns.len() > Self::SOFT_SWITCH_BUDGET {
            switch_turns.pop_front();
        }
    }

    fn is_eligible(model: &str, eligible: &[&ModelDescriptor]) -> bool {
        eligible
            .iter()
            .any(|descriptor| descriptor.qualified_id == model)
    }

    fn is_pinnable(model: &str) -> bool {
        let Some((profile, bare_id)) = strip_1m_suffix(model).split_once(Self::PROFILE_SEPARATOR)
        else {
            return false;
        };
        profile != HubSelector::NAMESPACE && AutoTier::from_model(bare_id).is_none()
    }

    fn lock(&self) -> MutexGuard<'_, PinTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

impl Default for SmartAutoAffinity {
    fn default() -> SmartAutoAffinity {
        SmartAutoAffinity::new()
    }
}

impl PinTable {
    /// The entry under `key`, removed first when it has been idle longer than the limit.
    fn live_entry(&mut self, key: &AffinityKey, now: Instant) -> Option<&mut PinEntry> {
        let idle = now.saturating_duration_since(self.entries.get(key)?.last_used);
        if idle > SmartAutoAffinity::IDLE {
            self.entries.remove(key);
            return None;
        }
        self.entries.get_mut(key)
    }

    fn record(&mut self, key: AffinityKey) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.order.push_back((key, sequence));
        sequence
    }

    /// Drops stale order records once they outnumber twice the capacity, so commits stay O(1)
    /// amortized. Call it after the committed entry holds its new sequence.
    fn compact(&mut self) {
        if self.order.len() <= 2 * SmartAutoAffinity::CAPACITY {
            return;
        }
        let entries = &self.entries;
        self.order.retain(|(recorded, sequence)| {
            entries
                .get(recorded)
                .is_some_and(|entry| entry.sequence == *sequence)
        });
    }

    /// Removes the least recently committed entries, never `kept`, until the map fits.
    fn evict_over_capacity(&mut self, kept: &AffinityKey) {
        while self.entries.len() > SmartAutoAffinity::CAPACITY {
            let Some((oldest, sequence)) = self.order.pop_front() else {
                return;
            };
            let current = self
                .entries
                .get(&oldest)
                .is_some_and(|entry| entry.sequence == sequence);
            if current && oldest != *kept {
                self.entries.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::smart_auto_headers::SmartAutoHeaders;
    use crate::schemas::smart_auto_policy::SmartAutoPolicy;

    const MESSAGES: &str = "/v1/messages";
    const OPUS: &str = "copilot/claude-opus-4.6";
    const MINI: &str = "copilot/gpt-5-mini";
    const LOCAL: &str = "llama_swap/qwen3-coder";

    fn model(qualified_id: &str) -> ModelDescriptor {
        ModelDescriptor::for_test(qualified_id)
    }

    fn signals(session: &str, tool_continuation: bool) -> TurnSignals {
        TurnSignals {
            tool_continuation,
            session_id: Some(session.to_string()),
            ..TurnSignals::for_test()
        }
    }

    fn key_for(session: &str, worker: Option<&str>) -> AffinityKey {
        let caller = SmartAutoHeaders {
            worker: worker.map(str::to_string),
            parent: worker.map(|_| "run".to_string()),
            ..SmartAutoHeaders::default()
        };
        AffinityKey::new(
            &signals(session, false),
            &caller,
            &SmartAutoPolicy::default(),
            MESSAGES,
        )
        .unwrap()
    }

    fn pinned(model: &str, role: WorkRole, plan_backed: bool) -> (String, WorkRole, bool) {
        (model.to_string(), role, plan_backed)
    }

    struct Pool(Vec<ModelDescriptor>);

    impl Pool {
        fn of(ids: &[&str]) -> Pool {
            Pool(ids.iter().map(|id| model(id)).collect())
        }

        fn refs(&self) -> Vec<&ModelDescriptor> {
            self.0.iter().collect()
        }
    }

    #[test]
    fn commit_pins_and_lookup_replays_it() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();

        assert!(store.commit(&key, OPUS, WorkRole::Planning, false, 1, now));

        assert_eq!(
            store.lookup(&key, &pool.refs(), now),
            Some(pinned(OPUS, WorkRole::Planning, false))
        );
    }

    #[test]
    fn a_failed_attempt_commits_nothing_and_keeps_the_previous_pin() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();

        assert_eq!(store.lookup(&key, &pool.refs(), now), None);
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, now);
        // An attempt on MINI fails, so the proxy never calls commit for it.

        assert_eq!(
            store.lookup(&key, &pool.refs(), now),
            Some(pinned(OPUS, WorkRole::Planning, false))
        );
    }

    #[test]
    fn two_workers_under_one_parent_keep_separate_pins() {
        let store = SmartAutoAffinity::new();
        let first = key_for("s1", Some("run/a/1"));
        let second = key_for("s1", Some("run/b/1"));
        let main = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI, LOCAL]);
        let now = Instant::now();

        store.commit(&first, MINI, WorkRole::Implementation, true, 1, now);
        store.commit(&second, LOCAL, WorkRole::Validation, false, 1, now);

        assert_eq!(
            store.lookup(&first, &pool.refs(), now),
            Some(pinned(MINI, WorkRole::Implementation, true))
        );
        assert_eq!(
            store.lookup(&second, &pool.refs(), now),
            Some(pinned(LOCAL, WorkRole::Validation, false))
        );
        assert_eq!(store.lookup(&main, &pool.refs(), now), None);
    }

    #[test]
    fn lookup_drops_a_pin_that_is_no_longer_eligible() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let now = Instant::now();
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, now);

        assert_eq!(store.lookup(&key, &Pool::of(&[MINI]).refs(), now), None);
        assert_eq!(
            store.lookup(&key, &Pool::of(&[OPUS, MINI]).refs(), now),
            None
        );
    }

    #[test]
    fn incumbent_is_per_role_and_respects_eligibility() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, now);
        store.commit(&key, MINI, WorkRole::Implementation, true, 3, now);

        assert_eq!(
            store.incumbent(&key, WorkRole::Planning, &pool.refs(), now),
            Some(OPUS.to_string())
        );
        assert_eq!(
            store.incumbent(&key, WorkRole::Implementation, &pool.refs(), now),
            Some(MINI.to_string())
        );
        assert_eq!(
            store.incumbent(&key, WorkRole::Validation, &pool.refs(), now),
            None
        );
        assert_eq!(
            store.incumbent(&key, WorkRole::Planning, &Pool::of(&[MINI]).refs(), now),
            None
        );
    }

    #[test]
    fn pin_expires_after_ten_idle_minutes() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS]);
        let start = Instant::now();
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, start);

        assert!(
            store
                .lookup(&key, &pool.refs(), start + SmartAutoAffinity::IDLE)
                .is_some()
        );
        let expired = start + SmartAutoAffinity::IDLE + Duration::from_secs(1);
        assert_eq!(store.lookup(&key, &pool.refs(), expired), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn commit_refreshes_the_idle_timer() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS]);
        let start = Instant::now();
        let later = start + Duration::from_mins(9);
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, start);
        store.commit(&key, OPUS, WorkRole::Planning, false, 2, later);

        assert!(
            store
                .lookup(&key, &pool.refs(), later + Duration::from_mins(9))
                .is_some()
        );
    }

    #[test]
    fn capacity_evicts_the_least_recently_committed_entry() {
        let store = SmartAutoAffinity::new();
        let pool = Pool::of(&[OPUS]);
        let now = Instant::now();
        let keys: Vec<AffinityKey> = (0..=SmartAutoAffinity::CAPACITY)
            .map(|index| key_for(&format!("s{index}"), None))
            .collect();
        for key in &keys[..SmartAutoAffinity::CAPACITY] {
            store.commit(key, OPUS, WorkRole::Planning, false, 1, now);
        }
        // Re-committing the first key makes the second one the oldest.
        store.commit(&keys[0], OPUS, WorkRole::Planning, false, 2, now);

        store.commit(
            &keys[SmartAutoAffinity::CAPACITY],
            OPUS,
            WorkRole::Planning,
            false,
            1,
            now,
        );

        assert_eq!(store.len(), SmartAutoAffinity::CAPACITY);
        assert!(store.lookup(&keys[0], &pool.refs(), now).is_some());
        assert_eq!(store.lookup(&keys[1], &pool.refs(), now), None);
        assert!(
            store
                .lookup(&keys[SmartAutoAffinity::CAPACITY], &pool.refs(), now)
                .is_some()
        );
    }

    #[test]
    fn repeated_commits_keep_the_order_queue_bounded() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let now = Instant::now();

        for turn in 0..5 * SmartAutoAffinity::CAPACITY {
            store.commit(&key, OPUS, WorkRole::Planning, false, turn, now);
        }

        assert!(store.lock().order.len() <= 2 * SmartAutoAffinity::CAPACITY);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn auto_aliases_hub_ids_and_unqualified_ids_are_never_pinned() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let now = Instant::now();

        for model in [
            "copilot/auto",
            "copilot/auto-balance",
            "hub/smart-auto-balance",
            "gpt-5",
        ] {
            assert!(
                !store.commit(&key, model, WorkRole::Planning, false, 1, now),
                "{model}"
            );
        }
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn tool_continuation_replays_the_pin_against_a_new_role() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, now);
        let continuation = signals("s1", true);

        let (model, role, plan_backed) = store.lookup(&key, &pool.refs(), now).unwrap();

        assert_eq!(model, OPUS);
        assert!(!SmartAutoAffinity::should_switch(
            role,
            plan_backed,
            WorkRole::Implementation,
            true,
            &continuation,
            true
        ));
    }

    #[test]
    fn soft_switch_needs_dwell_turns_and_a_free_budget_slot() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let now = Instant::now();

        assert!(store.soft_switch_allowed(&key, 1, now));
        store.commit(&key, OPUS, WorkRole::Planning, false, 1, now);
        assert!(!store.soft_switch_allowed(&key, 2, now));
        assert!(store.soft_switch_allowed(&key, 3, now));

        store.commit(&key, MINI, WorkRole::Implementation, true, 3, now);
        store.commit(&key, OPUS, WorkRole::Validation, false, 5, now);
        assert!(!store.soft_switch_allowed(&key, 7, now));

        let window_end = 3 + SmartAutoAffinity::SOFT_SWITCH_WINDOW_TURNS;
        assert!(store.soft_switch_allowed(&key, window_end, now));
    }

    #[test]
    fn should_switch_matrix() {
        use WorkRole::{Conversation, Implementation, Investigation, Planning, Validation};
        let fresh = signals("s1", false);
        // ((pinned, pinned plan_backed), (new, new plan_backed), expected)
        let cases = [
            ((Planning, false), (Implementation, true), true),
            ((Planning, false), (Implementation, false), false),
            ((Investigation, false), (Implementation, true), true),
            ((Implementation, true), (Planning, false), true),
            ((Implementation, true), (Planning, true), true),
            ((Implementation, false), (Planning, false), false),
            ((Implementation, true), (Validation, true), true),
            ((Implementation, true), (Implementation, true), false),
            ((Implementation, true), (Implementation, false), true),
            ((Implementation, false), (Implementation, true), true),
            ((Planning, false), (Investigation, false), false),
            ((Planning, false), (Validation, false), false),
            ((Planning, false), (Conversation, false), false),
            ((Implementation, true), (Conversation, true), false),
            ((Conversation, false), (Planning, false), true),
            ((Conversation, false), (Implementation, true), true),
            ((Conversation, false), (Implementation, false), true),
            ((Conversation, false), (Conversation, false), false),
        ];

        for ((pinned_role, pinned_plan), (new_role, new_plan), expected) in cases {
            let label = format!("{pinned_role:?}/{pinned_plan} -> {new_role:?}/{new_plan}");
            assert_eq!(
                SmartAutoAffinity::should_switch(
                    pinned_role,
                    pinned_plan,
                    new_role,
                    new_plan,
                    &fresh,
                    true
                ),
                expected,
                "{label}"
            );
            assert!(
                !SmartAutoAffinity::should_switch(
                    pinned_role,
                    pinned_plan,
                    new_role,
                    new_plan,
                    &fresh,
                    false
                ),
                "unsettled {label}"
            );
        }
    }

    #[test]
    fn a_cheap_implementation_pin_leaves_for_plain_planning() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();
        store.commit(&key, MINI, WorkRole::Implementation, true, 1, now);
        let (_, role, plan_backed) = store.lookup(&key, &pool.refs(), now).unwrap();

        let switches = SmartAutoAffinity::should_switch(
            role,
            plan_backed,
            WorkRole::Planning,
            false,
            &signals("s1", false),
            true,
        );

        assert!(switches);
    }

    #[test]
    fn a_heavy_implementation_pin_stays_for_planning() {
        let store = SmartAutoAffinity::new();
        let key = key_for("s1", None);
        let pool = Pool::of(&[OPUS, MINI]);
        let now = Instant::now();
        store.commit(&key, OPUS, WorkRole::Implementation, false, 1, now);
        let (_, role, plan_backed) = store.lookup(&key, &pool.refs(), now).unwrap();

        let switches = SmartAutoAffinity::should_switch(
            role,
            plan_backed,
            WorkRole::Planning,
            false,
            &signals("s1", false),
            true,
        );

        assert!(!switches);
    }
}
