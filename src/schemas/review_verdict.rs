use serde::{Deserialize, Serialize};

use crate::schemas::app_error::AppError;

/// A reviewer's decision on a draft answer. Parsed strictly from JSON, so any other shape is an error.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// The draft is good as written.
    Accept,
    /// The draft can be fixed; each issue names one concrete defect.
    Repair { issues: Vec<String> },
    /// The draft is unusable; the reviewer names why.
    Reject { issues: Vec<String> },
}

impl ReviewVerdict {
    /// The most issues one verdict may name; more is a malformed verdict.
    pub const MAX_ISSUES: usize = 8;
    /// The longest issue kept, in characters; longer issues are cut at a character boundary.
    pub const MAX_ISSUE_CHARS: usize = 300;

    /// Parses one verdict object from the whole reply; prose, fences and out-of-bounds issues are errors.
    pub fn parse(reply: &str) -> Result<ReviewVerdict, AppError> {
        let verdict: ReviewVerdict = serde_json::from_str(reply.trim())
            .map_err(|e| AppError::BadRequest(format!("malformed review verdict: {e}")))?;
        verdict.bounded()
    }

    /// The issues a repair or reject names, empty for an accept.
    #[must_use]
    pub fn issues(&self) -> &[String] {
        match self {
            ReviewVerdict::Accept => &[],
            ReviewVerdict::Repair { issues } | ReviewVerdict::Reject { issues } => issues,
        }
    }

    /// Trims and cuts the issues, and rejects an empty or over-long list.
    fn bounded(self) -> Result<ReviewVerdict, AppError> {
        match self {
            ReviewVerdict::Accept => Ok(self),
            ReviewVerdict::Repair { issues } => Ok(ReviewVerdict::Repair {
                issues: Self::bounded_issues(issues)?,
            }),
            ReviewVerdict::Reject { issues } => Ok(ReviewVerdict::Reject {
                issues: Self::bounded_issues(issues)?,
            }),
        }
    }

    fn bounded_issues(issues: Vec<String>) -> Result<Vec<String>, AppError> {
        let named: Vec<String> = issues
            .into_iter()
            .map(|issue| issue.trim().to_owned())
            .filter(|issue| !issue.is_empty())
            .collect();
        if named.is_empty() {
            return Err(AppError::BadRequest(
                "malformed review verdict: a repair or reject must name at least one issue"
                    .to_owned(),
            ));
        }
        if named.len() > Self::MAX_ISSUES {
            return Err(AppError::BadRequest(format!(
                "malformed review verdict: {} issues exceed the limit of {}",
                named.len(),
                Self::MAX_ISSUES
            )));
        }
        Ok(named
            .into_iter()
            .map(|issue| issue.chars().take(Self::MAX_ISSUE_CHARS).collect())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accept_repair_and_reject() {
        assert_eq!(
            ReviewVerdict::parse(r#"{"verdict":"accept"}"#).unwrap(),
            ReviewVerdict::Accept
        );
        assert_eq!(
            ReviewVerdict::parse(r#"{"verdict":"repair","issues":["misses the edge case"]}"#)
                .unwrap(),
            ReviewVerdict::Repair {
                issues: vec!["misses the edge case".to_owned()]
            }
        );
        assert_eq!(
            ReviewVerdict::parse(r#"  {"verdict":"reject","issues":["wrong file"]}  "#).unwrap(),
            ReviewVerdict::Reject {
                issues: vec!["wrong file".to_owned()]
            }
        );
    }

    #[test]
    fn malformed_verdicts_are_errors() {
        for reply in [
            "",
            "accept",
            "```json\n{\"verdict\":\"accept\"}\n```",
            r#"{"verdict":"maybe"}"#,
            r#"{"verdict":"repair"}"#,
            r#"{"verdict":"repair","issues":[]}"#,
            r#"{"verdict":"reject","issues":["  "]}"#,
            r#"[{"verdict":"accept"}]"#,
        ] {
            assert!(ReviewVerdict::parse(reply).is_err(), "{reply}");
        }
    }

    #[test]
    fn too_many_issues_are_rejected_and_long_issues_are_cut() {
        let many = vec!["x"; ReviewVerdict::MAX_ISSUES + 1];
        let json = serde_json::json!({ "verdict": "repair", "issues": many }).to_string();
        assert!(ReviewVerdict::parse(&json).is_err());

        let long = "é".repeat(ReviewVerdict::MAX_ISSUE_CHARS + 50);
        let json = serde_json::json!({ "verdict": "repair", "issues": [long] }).to_string();
        let verdict = ReviewVerdict::parse(&json).unwrap();
        assert_eq!(
            verdict.issues()[0].chars().count(),
            ReviewVerdict::MAX_ISSUE_CHARS
        );
    }
}
