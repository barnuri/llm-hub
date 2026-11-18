//! What a hook answers, parsed strictly. An unknown field or an unsupported operation makes the
//! call fail, so a hook that expects a feature the hub lacks never passes silently.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::ext::rules::{Action, header_name, header_value};

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum HookReply {
    Continue {},
    Reject {
        status: u16,
        message: String,
    },
    Patch {
        #[serde(default)]
        set_headers: BTreeMap<String, String>,
        #[serde(default)]
        remove_headers: Vec<String>,
        /// A JSON Patch (RFC 6902) with the `add`, `replace` and `remove` operations.
        #[serde(default)]
        body_patch: Vec<PatchOp>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchOp {
    op: PatchKind,
    path: String,
    value: Option<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchKind {
    Add,
    Replace,
    Remove,
}

/// A valid reply, ready to apply.
#[derive(Debug)]
pub(crate) enum Effect {
    Continue,
    Reject(StatusCode, String),
    Apply(Vec<Action>),
}

impl Effect {
    /// The effect as a short phrase for a log line.
    pub(crate) fn describe(&self) -> String {
        match self {
            Effect::Continue => "continue".to_owned(),
            Effect::Reject(status, _) => format!("reject({})", status.as_u16()),
            Effect::Apply(actions) => actions
                .iter()
                .map(Action::describe)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

impl HookReply {
    /// Checks the reply and turns it into the same actions a rule uses.
    ///
    /// # Errors
    /// A message that names what is wrong: a status below 400, a bad header or JSON Pointer, or a
    /// patch operation with a missing value.
    pub(crate) fn into_effect(self) -> Result<Effect, String> {
        match self {
            HookReply::Continue {} => Ok(Effect::Continue),
            HookReply::Reject { status, message } => {
                let status = StatusCode::from_u16(status)
                    .ok()
                    .filter(|status| status.is_client_error() || status.is_server_error())
                    .ok_or_else(|| format!("reject status {status} is not a 4xx or 5xx status"))?;
                Ok(Effect::Reject(status, message))
            }
            HookReply::Patch {
                set_headers,
                remove_headers,
                body_patch,
            } => {
                let mut actions = Vec::new();
                if !set_headers.is_empty() {
                    let pairs = set_headers
                        .iter()
                        .map(|(name, value)| Ok((header_name(name)?, header_value(value)?)))
                        .collect::<Result<_, String>>()?;
                    actions.push(Action::SetHeader(pairs));
                }
                if !remove_headers.is_empty() {
                    let names = remove_headers
                        .iter()
                        .map(|name| header_name(name))
                        .collect::<Result<_, _>>()?;
                    actions.push(Action::RemoveHeader(names));
                }
                for op in body_patch {
                    actions.push(op.into_action()?);
                }
                Ok(Effect::Apply(actions))
            }
        }
    }
}

impl PatchOp {
    fn into_action(self) -> Result<Action, String> {
        if !self.path.starts_with('/') || self.path.len() == 1 {
            return Err(format!(
                "`{}` is not a JSON Pointer such as /temperature",
                self.path
            ));
        }
        match (self.op, self.value) {
            (PatchKind::Add | PatchKind::Replace, Some(value)) => {
                Ok(Action::SetField(vec![(self.path, value)]))
            }
            (PatchKind::Add | PatchKind::Replace, None) => {
                Err(format!("the patch operation on {} has no value", self.path))
            }
            (PatchKind::Remove, _) => Ok(Action::RemoveField(vec![self.path])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(json: &str) -> Result<Effect, String> {
        serde_json::from_str::<HookReply>(json)
            .map_err(|e| e.to_string())
            .and_then(HookReply::into_effect)
    }

    #[test]
    fn continue_and_reject_replies_parse() {
        assert!(matches!(
            effect(r#"{"action":"continue"}"#),
            Ok(Effect::Continue)
        ));
        let reject = effect(r#"{"action":"reject","status":403,"message":"no"}"#).unwrap();
        assert!(matches!(&reject, Effect::Reject(StatusCode::FORBIDDEN, m) if m == "no"));
        assert_eq!(reject.describe(), "reject(403)");
        assert_eq!(Effect::Continue.describe(), "continue");
    }

    #[test]
    fn a_patch_reply_becomes_header_and_body_actions() {
        let patch = effect(
            r#"{"action":"patch","set_headers":{"x-a":"1"},"remove_headers":["x-b"],
                "body_patch":[{"op":"add","path":"/metadata/team","value":"blue"},
                              {"op":"replace","path":"/model","value":"a/m"},
                              {"op":"remove","path":"/temperature"}]}"#,
        )
        .unwrap();

        let Effect::Apply(actions) = &patch else {
            panic!("expected actions");
        };
        assert_eq!(actions.len(), 5);
        assert_eq!(
            patch.describe(),
            "set_header, remove_header, set_field, set_field, remove_field"
        );
    }

    #[test]
    fn an_empty_patch_is_valid_and_changes_nothing() {
        let Ok(Effect::Apply(actions)) = effect(r#"{"action":"patch"}"#) else {
            panic!("expected an empty patch");
        };
        assert!(actions.is_empty());
    }

    #[test]
    fn invalid_replies_are_errors() {
        for json in [
            r#"{"action":"stop"}"#,
            r#"{"action":"continue","extra":1}"#,
            r#"{"action":"reject","status":200,"message":"x"}"#,
            r#"{"action":"reject","status":99999,"message":"x"}"#,
            r#"{"action":"reject","status":403}"#,
            r#"{"action":"patch","set_headers":{"bad name":"1"}}"#,
            r#"{"action":"patch","set_headers":{"x-a":"bad\nvalue"}}"#,
            r#"{"action":"patch","remove_headers":["bad name"]}"#,
            r#"{"action":"patch","body_patch":[{"op":"move","path":"/a","from":"/b"}]}"#,
            r#"{"action":"patch","body_patch":[{"op":"add","path":"/a"}]}"#,
            r#"{"action":"patch","body_patch":[{"op":"remove","path":"a"}]}"#,
            r#"{"action":"patch","body_patch":[{"op":"remove","path":"/"}]}"#,
            r#"{"action":"patch","body_patch":[{"op":"add","path":"/a","value":1,"extra":2}]}"#,
            "not json",
        ] {
            assert!(effect(json).is_err(), "{json}");
        }
    }
}
