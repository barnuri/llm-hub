use bytes::Bytes;
use serde_json::{Map, Value};

use crate::schemas::app_error::AppError;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;
use crate::services::copilot_auto::{CHAT_ENDPOINT, MESSAGES_ENDPOINT, RESPONSES_ENDPOINT};

/// Chooses the reasoning effort sent with a routed model and writes it into the request body.
/// Pure: reads only its arguments, no I/O.
#[derive(Debug)]
pub struct SmartAutoEffort;

/// Where one endpoint's body carries the effort: `parent.key`, or top-level `key`.
struct EffortField {
    parent: Option<&'static str>,
    key: &'static str,
}

impl SmartAutoEffort {
    /// The error code prefix of an effort name that is not an effort level.
    const UNKNOWN_EFFORT: &str = "unknown_effort";
    const CHAT_FIELD: EffortField = EffortField {
        parent: None,
        key: "reasoning_effort",
    };
    const RESPONSES_FIELD: EffortField = EffortField {
        parent: Some("reasoning"),
        key: "effort",
    };
    const MESSAGES_FIELD: EffortField = EffortField {
        parent: Some("output_config"),
        key: "effort",
    };

    /// The effort to send: the caller's level, else the tier and role objective, snapped to the
    /// nearest published level. `None` when `published` holds no selectable level.
    #[must_use]
    pub fn select(
        tier: SmartAutoTier,
        role: WorkRole,
        plan_backed: bool,
        requested: Option<EffortLevel>,
        published: &[String],
    ) -> Option<EffortLevel> {
        let supported: Vec<EffortLevel> = published
            .iter()
            .filter_map(|name| EffortLevel::from_name(name))
            .collect();
        if supported.is_empty() {
            return None;
        }
        match requested {
            Some(requested) if supported.contains(&requested) => Some(requested),
            Some(requested) => Self::snap(requested, &supported),
            None => Self::snap(Self::objective(tier, role, plan_backed), &supported),
        }
    }

    /// The caller's explicit effort in a body for `endpoint`, e.g. chat `reasoning_effort`.
    ///
    /// # Errors
    /// `AppError::BadRequest` when the field holds a name that is not an effort level.
    pub fn requested(endpoint: &str, body: &Value) -> Result<Option<EffortLevel>, AppError> {
        let Some(field) = Self::field(endpoint) else {
            return Ok(None);
        };
        let holder = match field.parent {
            None => Some(body),
            Some(parent) => body.get(parent),
        };
        let Some(name) = holder
            .and_then(|holder| holder.get(field.key))
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        EffortLevel::from_name(name).map(Some).ok_or_else(|| {
            AppError::BadRequest(format!(
                "{}: `{name}` is not an effort level",
                Self::UNKNOWN_EFFORT
            ))
        })
    }

    /// Writes `level` where the `endpoint` body carries effort, keeping sibling keys.
    /// Returns false, leaving the body unchanged, for an unknown endpoint or a non-object body.
    pub fn apply(endpoint: &str, body: &mut Value, level: EffortLevel) -> bool {
        let Some(field) = Self::field(endpoint) else {
            return false;
        };
        let Some(object) = body.as_object_mut() else {
            return false;
        };
        let holder = match field.parent {
            None => object,
            Some(parent) => {
                let entry = object
                    .entry(parent)
                    .or_insert_with(|| Value::Object(Map::new()));
                if !entry.is_object() {
                    *entry = Value::Object(Map::new());
                }
                let Some(holder) = entry.as_object_mut() else {
                    return false;
                };
                holder
            }
        };
        holder.insert(field.key.to_owned(), Value::from(level.as_str()));
        true
    }

    /// `body` with `level` written where `endpoint` carries effort. A body that is not a JSON
    /// object, or an endpoint without an effort field, comes back unchanged.
    #[must_use]
    pub fn apply_to_bytes(endpoint: &str, body: Bytes, level: EffortLevel) -> Bytes {
        let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
            return body;
        };
        if !Self::apply(endpoint, &mut value, level) {
            return body;
        }
        serde_json::to_vec(&value).map_or(body, Bytes::from)
    }

    /// The level the tier and role aim for before snapping to the model's published levels.
    fn objective(tier: SmartAutoTier, role: WorkRole, plan_backed: bool) -> EffortLevel {
        let scoped_implementation = role == WorkRole::Implementation && plan_backed;
        match tier {
            SmartAutoTier::Efficiency | SmartAutoTier::Fast => EffortLevel::Low,
            SmartAutoTier::Balance if scoped_implementation => EffortLevel::Low,
            SmartAutoTier::Balance => EffortLevel::Medium,
            SmartAutoTier::Intelligence if scoped_implementation => EffortLevel::Medium,
            SmartAutoTier::Intelligence => match role {
                WorkRole::Planning | WorkRole::Investigation | WorkRole::Implementation => {
                    EffortLevel::High
                }
                WorkRole::Validation | WorkRole::Conversation => EffortLevel::Medium,
            },
        }
    }

    /// The supported level nearest to `target`, ties going to the higher level.
    /// Never snaps to `none`, which switches reasoning off.
    fn snap(target: EffortLevel, supported: &[EffortLevel]) -> Option<EffortLevel> {
        supported
            .iter()
            .copied()
            .filter(|level| *level != EffortLevel::None)
            .min_by_key(|level| {
                (
                    (*level as u8).abs_diff(target as u8),
                    std::cmp::Reverse(*level),
                )
            })
    }

    fn field(endpoint: &str) -> Option<EffortField> {
        match endpoint {
            CHAT_ENDPOINT => Some(Self::CHAT_FIELD),
            RESPONSES_ENDPOINT => Some(Self::RESPONSES_FIELD),
            MESSAGES_ENDPOINT => Some(Self::MESSAGES_FIELD),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn published(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn every_level() -> Vec<String> {
        EffortLevel::ALL
            .iter()
            .map(|level| level.as_str().to_owned())
            .collect()
    }

    fn default_for(tier: SmartAutoTier, role: WorkRole, plan_backed: bool) -> Option<EffortLevel> {
        SmartAutoEffort::select(tier, role, plan_backed, None, &every_level())
    }

    #[test]
    fn efficiency_and_fast_aim_low_for_every_role() {
        let roles = [
            WorkRole::Planning,
            WorkRole::Investigation,
            WorkRole::Implementation,
            WorkRole::Validation,
            WorkRole::Conversation,
        ];
        for tier in [SmartAutoTier::Efficiency, SmartAutoTier::Fast] {
            for role in roles {
                assert_eq!(
                    default_for(tier, role, false),
                    Some(EffortLevel::Low),
                    "{tier:?} {role:?}"
                );
            }
        }
    }

    #[test]
    fn balance_aims_medium_and_low_for_plan_backed_implementation() {
        assert_eq!(
            default_for(SmartAutoTier::Balance, WorkRole::Planning, false),
            Some(EffortLevel::Medium)
        );
        assert_eq!(
            default_for(SmartAutoTier::Balance, WorkRole::Implementation, false),
            Some(EffortLevel::Medium)
        );
        assert_eq!(
            default_for(SmartAutoTier::Balance, WorkRole::Implementation, true),
            Some(EffortLevel::Low)
        );
    }

    #[test]
    fn intelligence_aims_high_for_planning_and_medium_for_review_and_chat() {
        for role in [
            WorkRole::Planning,
            WorkRole::Investigation,
            WorkRole::Implementation,
        ] {
            assert_eq!(
                default_for(SmartAutoTier::Intelligence, role, false),
                Some(EffortLevel::High),
                "{role:?}"
            );
        }
        for role in [WorkRole::Validation, WorkRole::Conversation] {
            assert_eq!(
                default_for(SmartAutoTier::Intelligence, role, false),
                Some(EffortLevel::Medium),
                "{role:?}"
            );
        }
        assert_eq!(
            default_for(SmartAutoTier::Intelligence, WorkRole::Implementation, true),
            Some(EffortLevel::Medium)
        );
    }

    #[test]
    fn supported_explicit_effort_wins_over_the_objective() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Efficiency,
            WorkRole::Conversation,
            false,
            Some(EffortLevel::Max),
            &published(&["low", "medium", "high", "max"]),
        );

        assert_eq!(selected, Some(EffortLevel::Max));
    }

    #[test]
    fn unsupported_explicit_effort_snaps_to_the_nearest_published_level() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Balance,
            WorkRole::Conversation,
            false,
            Some(EffortLevel::Xhigh),
            &published(&["low", "medium", "high"]),
        );

        assert_eq!(selected, Some(EffortLevel::High));
    }

    #[test]
    fn snaps_up_to_the_nearest_published_level() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Efficiency,
            WorkRole::Conversation,
            false,
            None,
            &published(&["medium", "high"]),
        );

        assert_eq!(selected, Some(EffortLevel::Medium));
    }

    #[test]
    fn snaps_down_to_the_nearest_published_level() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Intelligence,
            WorkRole::Planning,
            false,
            None,
            &published(&["minimal", "low", "medium"]),
        );

        assert_eq!(selected, Some(EffortLevel::Medium));
    }

    #[test]
    fn equal_distance_snaps_up() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Balance,
            WorkRole::Conversation,
            false,
            None,
            &published(&["low", "high"]),
        );

        assert_eq!(selected, Some(EffortLevel::High));
    }

    #[test]
    fn snapping_never_lands_on_none() {
        let low_target = |names: &[&str]| {
            SmartAutoEffort::select(
                SmartAutoTier::Fast,
                WorkRole::Conversation,
                false,
                None,
                &published(names),
            )
        };

        assert_eq!(
            low_target(&["none", "high", "max"]),
            Some(EffortLevel::High)
        );
        assert_eq!(low_target(&["none"]), None);
    }

    #[test]
    fn published_names_ignore_case_and_unknown_names() {
        let selected = SmartAutoEffort::select(
            SmartAutoTier::Balance,
            WorkRole::Conversation,
            false,
            None,
            &published(&["LOW", "turbo", "Medium"]),
        );

        assert_eq!(selected, Some(EffortLevel::Medium));
    }

    #[test]
    fn model_without_published_levels_gets_no_effort() {
        for requested in [None, Some(EffortLevel::High)] {
            let selected = SmartAutoEffort::select(
                SmartAutoTier::Intelligence,
                WorkRole::Planning,
                false,
                requested,
                &[],
            );
            assert_eq!(selected, None, "{requested:?}");
        }
        let unknown_only = SmartAutoEffort::select(
            SmartAutoTier::Balance,
            WorkRole::Conversation,
            false,
            None,
            &published(&["turbo"]),
        );
        assert_eq!(unknown_only, None);
    }

    #[test]
    fn requested_reads_each_body_shape() {
        let cases = [
            (CHAT_ENDPOINT, json!({"reasoning_effort": "High"})),
            (RESPONSES_ENDPOINT, json!({"reasoning": {"effort": "high"}})),
            (
                MESSAGES_ENDPOINT,
                json!({"output_config": {"effort": "high"}}),
            ),
        ];
        for (endpoint, body) in cases {
            assert_eq!(
                SmartAutoEffort::requested(endpoint, &body).unwrap(),
                Some(EffortLevel::High),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn requested_is_none_without_a_field_or_for_an_unknown_endpoint() {
        assert_eq!(
            SmartAutoEffort::requested(CHAT_ENDPOINT, &json!({"messages": []})).unwrap(),
            None
        );
        assert_eq!(
            SmartAutoEffort::requested("/embeddings", &json!({"reasoning_effort": "high"}))
                .unwrap(),
            None
        );
    }

    #[test]
    fn requested_rejects_an_unknown_level_name() {
        let error = SmartAutoEffort::requested(
            RESPONSES_ENDPOINT,
            &json!({"reasoning": {"effort": "turbo"}}),
        )
        .unwrap_err();

        assert!(matches!(error, AppError::BadRequest(_)), "{error:?}");
    }

    #[test]
    fn apply_writes_chat_reasoning_effort() {
        let mut body = json!({"model": "m", "reasoning_effort": "high"});

        assert!(SmartAutoEffort::apply(
            CHAT_ENDPOINT,
            &mut body,
            EffortLevel::Low
        ));

        assert_eq!(body, json!({"model": "m", "reasoning_effort": "low"}));
    }

    #[test]
    fn apply_writes_responses_reasoning_effort_and_keeps_the_summary() {
        let mut body = json!({"input": [], "reasoning": {"summary": "auto"}});

        assert!(SmartAutoEffort::apply(
            RESPONSES_ENDPOINT,
            &mut body,
            EffortLevel::Xhigh
        ));

        assert_eq!(
            body,
            json!({"input": [], "reasoning": {"summary": "auto", "effort": "xhigh"}})
        );
    }

    #[test]
    fn apply_writes_anthropic_output_config_effort() {
        let mut body = json!({"messages": [], "output_config": null});

        assert!(SmartAutoEffort::apply(
            MESSAGES_ENDPOINT,
            &mut body,
            EffortLevel::Max
        ));

        assert_eq!(
            body,
            json!({"messages": [], "output_config": {"effort": "max"}})
        );
    }

    #[test]
    fn apply_leaves_an_unknown_endpoint_or_non_object_body_unchanged() {
        let mut body = json!({"input": "hi"});
        assert!(!SmartAutoEffort::apply(
            "/embeddings",
            &mut body,
            EffortLevel::Low
        ));
        assert_eq!(body, json!({"input": "hi"}));

        let mut array = json!([]);
        assert!(!SmartAutoEffort::apply(
            CHAT_ENDPOINT,
            &mut array,
            EffortLevel::Low
        ));
        assert_eq!(array, json!([]));
    }

    #[test]
    fn apply_to_bytes_writes_the_level_and_keeps_sibling_keys() {
        let body = Bytes::from_static(br#"{"model":"m1","reasoning_effort":"low"}"#);

        let applied = SmartAutoEffort::apply_to_bytes(CHAT_ENDPOINT, body, EffortLevel::High);

        let value: Value = serde_json::from_slice(&applied).unwrap();
        assert_eq!(value["reasoning_effort"], "high");
        assert_eq!(value["model"], "m1");
    }

    #[test]
    fn apply_to_bytes_leaves_a_body_that_is_not_json_unchanged() {
        let body = Bytes::from_static(b"not json");

        let applied =
            SmartAutoEffort::apply_to_bytes(CHAT_ENDPOINT, body.clone(), EffortLevel::High);

        assert_eq!(applied, body);
    }
}
