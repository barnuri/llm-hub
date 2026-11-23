use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What raised a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// A rule proposes a flag change the operator can approve.
    Proposal,
    /// The prefill guard refused a request.
    Guard,
    /// A rule found something worth knowing that no flag change fixes.
    Advisory,
    /// A hook, script or extension sent its own notification.
    Custom,
}

/// How urgent a notification is. The order runs from `Info` up to `Critical`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Critical,
}

/// Where a notification stands. Notifications without an approve action use only `pending` and `dismissed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Pending,
    Approved,
    Dismissed,
    Applied,
    Failed,
}

/// A button the Notifications page offers for a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAction {
    Approve,
    Dismiss,
}

/// The flag change a proposal makes to a model's registry row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FlagDiff {
    /// Flags and values appended to the row, for example `--cache-type-k`, `q8_0`.
    #[serde(default)]
    pub add: Vec<String>,
    /// Flags the apply step deletes from the row, with their values, before it appends `add`.
    #[serde(default)]
    pub remove: Vec<String>,
    /// The new context size, when the proposal changes it.
    #[serde(default)]
    pub ctx: Option<u64>,
    /// The context size before the change, so the UI can show `old → new`.
    #[serde(default)]
    pub old_ctx: Option<u64>,
}

/// One notification from any source, as the Notifications page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Notification {
    /// A stable id. Local tuner ids hash profile, model, rule and diff, so a re-run keeps the id.
    pub id: String,
    pub kind: NotificationKind,
    /// Who sent it, for example `local_tuner`, `errors`, `health`, `api` or `ext:<name>`.
    pub source: String,
    pub severity: NotificationSeverity,
    /// A one-line summary.
    pub title: String,
    /// The main text, such as the facts behind a proposal.
    pub body: String,
    /// The profile the notification is about, when it is about one.
    pub profile: Option<String>,
    /// The model the notification is about, when it is about one.
    pub model: Option<String>,
    /// The rule that raised a proposal or advisory.
    pub rule: Option<String>,
    pub diff: Option<FlagDiff>,
    /// The buttons the page offers.
    pub actions: Vec<NotificationAction>,
    pub status: NotificationStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// The output tail of the apply command, once an apply has run.
    pub result: Option<String>,
    /// True when a mute rule with scope `all` matched, so the page hides it by default.
    #[serde(default)]
    pub muted: bool,
}

/// The glob fields a mute rule matches. A notification matches when every given field matches.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MuteMatch {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// What a mute rule silences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MuteScope {
    /// Hides the notification in the UI and skips every sink.
    All,
    /// Shows the notification in the UI but skips every sink.
    Sinks,
}

/// Where a mute rule came from. Only `ui` rules are saved in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MuteOrigin {
    /// `LLM_HUB_NOTIFY_MUTE`, read-only in the UI.
    Env,
    /// Created on the Notifications page or through the API.
    Ui,
}

/// A rule that mutes or snoozes the notifications it matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationMute {
    pub id: String,
    pub matcher: MuteMatch,
    /// When the snooze ends. `None` mutes forever.
    pub until_ms: Option<u64>,
    pub scope: MuteScope,
    pub note: Option<String>,
    pub origin: MuteOrigin,
    pub created_ms: u64,
}

impl Notification {
    /// The actions a `kind` offers: approve and dismiss for a proposal, dismiss for the rest.
    #[must_use]
    pub fn default_actions(kind: NotificationKind) -> Vec<NotificationAction> {
        match kind {
            NotificationKind::Proposal => {
                vec![NotificationAction::Approve, NotificationAction::Dismiss]
            }
            NotificationKind::Guard | NotificationKind::Advisory | NotificationKind::Custom => {
                vec![NotificationAction::Dismiss]
            }
        }
    }
}

impl NotificationKind {
    /// Every kind, in declaration order.
    pub const ALL: [NotificationKind; 4] = [
        NotificationKind::Proposal,
        NotificationKind::Guard,
        NotificationKind::Advisory,
        NotificationKind::Custom,
    ];

    /// The stored and serialized name of the kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationKind::Proposal => "proposal",
            NotificationKind::Guard => "guard",
            NotificationKind::Advisory => "advisory",
            NotificationKind::Custom => "custom",
        }
    }

    /// The kind named `raw`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<NotificationKind> {
        NotificationKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == raw)
    }
}

impl NotificationSeverity {
    /// Every severity, from the least to the most urgent.
    pub const ALL: [NotificationSeverity; 3] = [
        NotificationSeverity::Info,
        NotificationSeverity::Warning,
        NotificationSeverity::Critical,
    ];

    /// The stored and serialized name of the severity.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationSeverity::Info => "info",
            NotificationSeverity::Warning => "warning",
            NotificationSeverity::Critical => "critical",
        }
    }

    /// The severity named `raw`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<NotificationSeverity> {
        NotificationSeverity::ALL
            .into_iter()
            .find(|severity| severity.as_str() == raw)
    }
}

impl NotificationStatus {
    /// Every status, in declaration order.
    pub const ALL: [NotificationStatus; 5] = [
        NotificationStatus::Pending,
        NotificationStatus::Approved,
        NotificationStatus::Dismissed,
        NotificationStatus::Applied,
        NotificationStatus::Failed,
    ];

    /// The stored and serialized name of the status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationStatus::Pending => "pending",
            NotificationStatus::Approved => "approved",
            NotificationStatus::Dismissed => "dismissed",
            NotificationStatus::Applied => "applied",
            NotificationStatus::Failed => "failed",
        }
    }

    /// The status named `raw`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<NotificationStatus> {
        NotificationStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == raw)
    }
}

impl MuteScope {
    /// Every scope, in declaration order.
    pub const ALL: [MuteScope; 2] = [MuteScope::All, MuteScope::Sinks];

    /// The stored and serialized name of the scope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MuteScope::All => "all",
            MuteScope::Sinks => "sinks",
        }
    }

    /// The scope named `raw`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<MuteScope> {
        MuteScope::ALL
            .into_iter()
            .find(|scope| scope.as_str() == raw)
    }
}

impl MuteOrigin {
    /// Every origin, in declaration order.
    pub const ALL: [MuteOrigin; 2] = [MuteOrigin::Env, MuteOrigin::Ui];

    /// The stored and serialized name of the origin.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MuteOrigin::Env => "env",
            MuteOrigin::Ui => "ui",
        }
    }

    /// The origin named `raw`, or `None` for an unknown name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<MuteOrigin> {
        MuteOrigin::ALL
            .into_iter()
            .find(|origin| origin.as_str() == raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip_and_match_serde() {
        for kind in NotificationKind::ALL {
            assert_eq!(NotificationKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
        }
        assert_eq!(NotificationKind::parse("other"), None);
    }

    #[test]
    fn severity_names_round_trip_and_match_serde() {
        for severity in NotificationSeverity::ALL {
            assert_eq!(
                NotificationSeverity::parse(severity.as_str()),
                Some(severity)
            );
            assert_eq!(
                serde_json::to_value(severity).unwrap(),
                serde_json::json!(severity.as_str())
            );
        }
        assert_eq!(NotificationSeverity::parse("other"), None);
    }

    #[test]
    fn severity_orders_info_below_warning_below_critical() {
        assert!(NotificationSeverity::Info < NotificationSeverity::Warning);
        assert!(NotificationSeverity::Warning < NotificationSeverity::Critical);
    }

    #[test]
    fn status_names_round_trip_and_match_serde() {
        for status in NotificationStatus::ALL {
            assert_eq!(NotificationStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::json!(status.as_str())
            );
        }
        assert_eq!(NotificationStatus::parse("other"), None);
    }

    #[test]
    fn only_proposals_offer_approve() {
        assert_eq!(
            Notification::default_actions(NotificationKind::Proposal),
            vec![NotificationAction::Approve, NotificationAction::Dismiss]
        );
        for kind in [
            NotificationKind::Guard,
            NotificationKind::Advisory,
            NotificationKind::Custom,
        ] {
            assert_eq!(
                Notification::default_actions(kind),
                vec![NotificationAction::Dismiss]
            );
        }
    }

    #[test]
    fn flag_diff_fields_default_when_absent() {
        let diff: FlagDiff = serde_json::from_str("{}").unwrap();
        assert_eq!(diff, FlagDiff::default());
    }

    #[test]
    fn notification_serializes_snake_case_fields() {
        let notification = Notification {
            id: "abc".into(),
            kind: NotificationKind::Proposal,
            source: "local_tuner".into(),
            severity: NotificationSeverity::Warning,
            title: "Quantize the KV cache".into(),
            body: "cmd".into(),
            profile: Some("local".into()),
            model: Some("gemma".into()),
            rule: Some("kv_cache_q8".into()),
            diff: Some(FlagDiff {
                add: vec!["--cache-type-k".into(), "q8_0".into()],
                remove: Vec::new(),
                ctx: Some(32_768),
                old_ctx: None,
            }),
            actions: Notification::default_actions(NotificationKind::Proposal),
            status: NotificationStatus::Pending,
            created_ms: 1,
            updated_ms: 2,
            result: None,
            muted: false,
        };
        let value = serde_json::to_value(&notification).unwrap();
        assert_eq!(value["kind"], "proposal");
        assert_eq!(value["severity"], "warning");
        assert_eq!(value["status"], "pending");
        assert_eq!(value["actions"], serde_json::json!(["approve", "dismiss"]));
        assert_eq!(value["diff"]["ctx"], 32_768);
        let back: Notification = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, notification);
        let mut without_muted = value;
        without_muted.as_object_mut().unwrap().remove("muted");
        let back: Notification = serde_json::from_value(without_muted).unwrap();
        assert!(!back.muted);
    }

    #[test]
    fn mute_scope_and_origin_names_round_trip_and_match_serde() {
        for scope in MuteScope::ALL {
            assert_eq!(MuteScope::parse(scope.as_str()), Some(scope));
            assert_eq!(
                serde_json::to_value(scope).unwrap(),
                serde_json::json!(scope.as_str())
            );
        }
        for origin in MuteOrigin::ALL {
            assert_eq!(MuteOrigin::parse(origin.as_str()), Some(origin));
            assert_eq!(
                serde_json::to_value(origin).unwrap(),
                serde_json::json!(origin.as_str())
            );
        }
        assert_eq!(MuteScope::parse("other"), None);
        assert_eq!(MuteOrigin::parse("other"), None);
    }

    #[test]
    fn mute_match_fields_default_when_absent() {
        let matcher: MuteMatch = serde_json::from_str(r#"{"source":"errors"}"#).unwrap();
        assert_eq!(
            matcher,
            MuteMatch {
                source: Some("errors".into()),
                ..MuteMatch::default()
            }
        );
    }
}
