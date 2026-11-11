use serde::{Deserialize, Serialize};

/// What is known about whether a completed Smart Auto attempt did its job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeCode {
    TestsPassed,
    TestsFailed,
    ReviewPassed,
    ReviewRejected,
    UserAccepted,
    UserCorrected,
}

/// Who reported an outcome. For one attempt, client wins over review, and review over heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSource {
    Heuristic,
    Review,
    Client,
}

impl OutcomeCode {
    pub const ALL: [OutcomeCode; 6] = [
        OutcomeCode::TestsPassed,
        OutcomeCode::TestsFailed,
        OutcomeCode::ReviewPassed,
        OutcomeCode::ReviewRejected,
        OutcomeCode::UserAccepted,
        OutcomeCode::UserCorrected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeCode::TestsPassed => "tests_passed",
            OutcomeCode::TestsFailed => "tests_failed",
            OutcomeCode::ReviewPassed => "review_passed",
            OutcomeCode::ReviewRejected => "review_rejected",
            OutcomeCode::UserAccepted => "user_accepted",
            OutcomeCode::UserCorrected => "user_corrected",
        }
    }

    pub fn parse(raw: &str) -> Option<OutcomeCode> {
        OutcomeCode::ALL
            .into_iter()
            .find(|code| code.as_str() == raw)
    }
}

impl OutcomeSource {
    pub const ALL: [OutcomeSource; 3] = [
        OutcomeSource::Heuristic,
        OutcomeSource::Review,
        OutcomeSource::Client,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeSource::Heuristic => "heuristic",
            OutcomeSource::Review => "review",
            OutcomeSource::Client => "client",
        }
    }

    pub fn parse(raw: &str) -> Option<OutcomeSource> {
        OutcomeSource::ALL
            .into_iter()
            .find(|source| source.as_str() == raw)
    }
}

/// One attempt's outcome as stored and served.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptOutcome {
    pub code: OutcomeCode,
    pub source: OutcomeSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_sources_round_trip_through_their_strings() {
        for code in OutcomeCode::ALL {
            assert_eq!(OutcomeCode::parse(code.as_str()), Some(code));
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::json!(code.as_str())
            );
        }
        for source in OutcomeSource::ALL {
            assert_eq!(OutcomeSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(OutcomeCode::parse("nope"), None);
    }
}
