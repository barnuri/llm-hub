use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// Seed scores per model family from public capability tiers, with no personal telemetry.
/// Pure and allocation-free: one scan of a const table per model.
#[derive(Debug)]
pub struct SmartAutoSuitability;

/// One model family. `parts` must appear in order in the model name, ASCII case ignored.
/// `quality` is 0-10 per role column, `cheapness` and `speed` are 0-10 with 10 the best.
struct FamilySeed {
    parts: &'static [&'static str],
    anchored: bool,
    quality: [u8; 6],
    cheapness: u8,
    speed: u8,
}

/// How much one tier values quality, cheapness and speed.
struct TierWeights {
    quality: u32,
    cheapness: u32,
    speed: u32,
}

impl FamilySeed {
    const fn contains(
        parts: &'static [&'static str],
        quality: [u8; 6],
        cheapness: u8,
        speed: u8,
    ) -> FamilySeed {
        FamilySeed {
            parts,
            anchored: false,
            quality,
            cheapness,
            speed,
        }
    }

    const fn prefix(
        parts: &'static [&'static str],
        quality: [u8; 6],
        cheapness: u8,
        speed: u8,
    ) -> FamilySeed {
        FamilySeed {
            parts,
            anchored: true,
            quality,
            cheapness,
            speed,
        }
    }
}

impl SmartAutoSuitability {
    /// A model whose role quality reaches this floor counts as capable for the role.
    pub const CAPABLE_FLOOR: u8 = 5;
    /// Added to every capable model's score, so any capable model outranks every other.
    pub const CAPABLE_BONUS: u32 = 1_000;
    const PLANNING: usize = 0;
    const INVESTIGATION: usize = 1;
    const IMPLEMENTATION: usize = 2;
    const SCOPED_IMPLEMENTATION: usize = 3;
    const VALIDATION: usize = 4;
    const CONVERSATION: usize = 5;
    const PATH_SEPARATOR: char = '/';
    // A family the table does not know is average at every role and earns no cost or speed credit.
    const UNKNOWN: FamilySeed = FamilySeed::contains(&[], [5, 5, 5, 5, 5, 5], 3, 3);
    // Quality columns: planning, investigation, implementation, plan-backed implementation,
    // validation, conversation. The first matching row wins, so size markers come first.
    const SEEDS: [FamilySeed; 24] = [
        FamilySeed::contains(&["nano"], [2, 3, 4, 6, 3, 6], 10, 10),
        FamilySeed::contains(&["-mini"], [4, 5, 6, 8, 5, 7], 9, 9),
        FamilySeed::contains(&["flash"], [4, 5, 6, 8, 5, 7], 9, 9),
        FamilySeed::contains(&["-luna"], [4, 6, 6, 8, 6, 7], 8, 8),
        FamilySeed::contains(&["haiku"], [4, 6, 6, 8, 6, 7], 8, 9),
        FamilySeed::contains(&["opus"], [10, 10, 10, 9, 10, 9], 1, 3),
        FamilySeed::contains(&["-sol"], [10, 9, 9, 9, 9, 9], 2, 3),
        FamilySeed::contains(&["gemini", "pro"], [9, 8, 8, 8, 8, 8], 4, 5),
        FamilySeed::contains(&["sonnet"], [8, 9, 9, 9, 8, 8], 4, 5),
        FamilySeed::contains(&["-terra"], [8, 8, 8, 8, 8, 8], 4, 5),
        FamilySeed::contains(&["codex"], [7, 8, 9, 9, 7, 6], 4, 5),
        FamilySeed::contains(&["grok"], [8, 8, 8, 8, 7, 8], 4, 6),
        FamilySeed::contains(&["gpt-oss"], [6, 6, 6, 7, 6, 6], 10, 6),
        FamilySeed::prefix(&["gpt-5"], [8, 8, 8, 8, 8, 8], 4, 5),
        FamilySeed::prefix(&["gpt-4"], [6, 6, 6, 7, 6, 7], 6, 7),
        FamilySeed::contains(&["qwen", "coder"], [5, 6, 7, 8, 5, 5], 10, 6),
        FamilySeed::contains(&["devstral"], [5, 6, 7, 8, 5, 5], 10, 6),
        FamilySeed::contains(&["deepseek"], [7, 7, 7, 8, 6, 7], 9, 5),
        FamilySeed::contains(&["qwen"], [5, 5, 6, 7, 5, 6], 10, 6),
        FamilySeed::contains(&["mistral"], [4, 4, 5, 6, 4, 6], 10, 7),
        FamilySeed::contains(&["gemma"], [3, 4, 5, 6, 4, 6], 10, 7),
        FamilySeed::contains(&["llama"], [3, 4, 4, 5, 3, 6], 10, 7),
        FamilySeed::prefix(&["phi"], [2, 3, 4, 5, 3, 5], 10, 8),
        FamilySeed::contains(&["mai-", "code"], [6, 6, 7, 8, 6, 6], 6, 7),
    ];

    /// The weighted seed score of `model_id` for one turn. `model_id` may be qualified,
    /// nested or carry `[1m]`. Capable models add [`Self::CAPABLE_BONUS`].
    #[must_use]
    pub fn score(model_id: &str, role: WorkRole, plan_backed: bool, tier: SmartAutoTier) -> u32 {
        let seed = Self::seed(model_id);
        let quality = seed.quality[Self::column(role, plan_backed)];
        let weights = Self::weights(tier);
        let weighted = weights.quality * u32::from(quality)
            + weights.cheapness * u32::from(seed.cheapness)
            + weights.speed * u32::from(seed.speed);
        if quality >= Self::CAPABLE_FLOOR {
            weighted + Self::CAPABLE_BONUS
        } else {
            weighted
        }
    }

    fn seed(model_id: &str) -> &'static FamilySeed {
        Self::family(model_id).unwrap_or(&Self::UNKNOWN)
    }

    fn family(model_id: &str) -> Option<&'static FamilySeed> {
        let name = strip_1m_suffix(model_id)
            .rsplit(Self::PATH_SEPARATOR)
            .next()
            .unwrap_or_default()
            .as_bytes();
        Self::SEEDS.iter().find(|seed| Self::matches(seed, name))
    }

    /// Each part must follow the previous one. An anchored family's first part starts the name.
    fn matches(seed: &FamilySeed, name: &[u8]) -> bool {
        let mut from = 0;
        for (index, part) in seed.parts.iter().enumerate() {
            let part = part.as_bytes();
            let Some(found) = Self::find(&name[from..], part) else {
                return false;
            };
            if seed.anchored && index == 0 && found != 0 {
                return false;
            }
            from += found + part.len();
        }
        true
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack
            .windows(needle.len())
            .position(|window| window.eq_ignore_ascii_case(needle))
    }

    fn column(role: WorkRole, plan_backed: bool) -> usize {
        match role {
            WorkRole::Planning => Self::PLANNING,
            WorkRole::Investigation => Self::INVESTIGATION,
            WorkRole::Implementation if plan_backed => Self::SCOPED_IMPLEMENTATION,
            WorkRole::Implementation => Self::IMPLEMENTATION,
            WorkRole::Validation => Self::VALIDATION,
            WorkRole::Conversation => Self::CONVERSATION,
        }
    }

    fn weights(tier: SmartAutoTier) -> TierWeights {
        let (quality, cheapness, speed) = match tier {
            SmartAutoTier::Efficiency => (1, 4, 1),
            SmartAutoTier::Balance => (5, 2, 1),
            SmartAutoTier::Intelligence => (6, 0, 1),
            SmartAutoTier::Fast => (1, 1, 4),
        };
        TierWeights {
            quality,
            cheapness,
            speed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(model_id: &str, role: WorkRole, plan_backed: bool) -> u8 {
        SmartAutoSuitability::seed(model_id).quality
            [SmartAutoSuitability::column(role, plan_backed)]
    }

    #[test]
    fn size_marker_beats_the_family_row() {
        assert_eq!(
            SmartAutoSuitability::seed("copilot/gpt-5-mini").cheapness,
            9
        );
        assert_eq!(SmartAutoSuitability::seed("copilot/gpt-5.4").cheapness, 4);
        assert_eq!(
            SmartAutoSuitability::seed("copilot/gpt-5.1-codex-mini").cheapness,
            9
        );
    }

    #[test]
    fn new_versions_inherit_the_family_score() {
        for id in [
            "copilot/claude-opus-4.6",
            "copilot/claude-opus-5-5[1m]",
            "llmgw/bedrock/us.anthropic.Claude-Opus-9",
        ] {
            assert_eq!(quality(id, WorkRole::Planning, false), 10, "{id}");
        }
        assert_eq!(
            quality("copilot/gpt-6.1-sol", WorkRole::Planning, false),
            10
        );
    }

    #[test]
    fn ordered_parts_must_follow_each_other() {
        assert_eq!(
            SmartAutoSuitability::seed("vertex/gemini-3-pro").cheapness,
            4
        );
        assert!(SmartAutoSuitability::family("vertex/pro-gemini").is_none());
    }

    #[test]
    fn a_prefix_family_matches_only_at_the_start() {
        assert_eq!(SmartAutoSuitability::seed("local/phi-4").speed, 8);
        assert!(SmartAutoSuitability::family("local/graphite-7b").is_none());
    }

    #[test]
    fn unknown_model_is_average_and_capable() {
        assert!(SmartAutoSuitability::family("acme/brand-new-model").is_none());
        let score = SmartAutoSuitability::score(
            "acme/brand-new-model",
            WorkRole::Planning,
            false,
            SmartAutoTier::Balance,
        );
        assert_eq!(
            score,
            SmartAutoSuitability::CAPABLE_BONUS + 5 * 5 + 2 * 3 + 3
        );
    }

    #[test]
    fn a_small_model_is_not_capable_for_planning() {
        let haiku = SmartAutoSuitability::score(
            "copilot/claude-haiku-4.5",
            WorkRole::Planning,
            false,
            SmartAutoTier::Efficiency,
        );
        let sonnet = SmartAutoSuitability::score(
            "copilot/claude-sonnet-4.6",
            WorkRole::Planning,
            false,
            SmartAutoTier::Efficiency,
        );
        assert!(haiku < SmartAutoSuitability::CAPABLE_BONUS);
        assert!(sonnet > haiku);
    }

    #[test]
    fn plan_backing_raises_a_small_model_for_implementation() {
        let haiku = "copilot/claude-haiku-4.5";
        assert!(
            quality(haiku, WorkRole::Implementation, true)
                > quality(haiku, WorkRole::Implementation, false)
        );
    }

    #[test]
    fn every_seed_value_stays_on_the_ten_point_scale() {
        for seed in &SmartAutoSuitability::SEEDS {
            assert!(!seed.parts.is_empty());
            assert!(seed.quality.iter().all(|value| *value <= 10));
            assert!(seed.cheapness <= 10 && seed.speed <= 10);
        }
    }
}
