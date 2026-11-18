use axum::http::StatusCode;

/// What a request or attempt phase decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Go on with the next extension and then the request.
    Continue,
    /// Stop and answer the client with this status and message, in the client's API format.
    Reject { status: StatusCode, message: String },
}

impl Flow {
    /// A rejection with `status` and `message`. A status that is not a valid HTTP code becomes 400.
    #[must_use]
    pub fn reject(status: u16, message: impl Into<String>) -> Flow {
        Flow::Reject {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejection_keeps_its_status_and_message() {
        assert_eq!(
            Flow::reject(403, "no"),
            Flow::Reject {
                status: StatusCode::FORBIDDEN,
                message: "no".to_owned()
            }
        );
    }

    #[test]
    fn an_invalid_status_becomes_bad_request() {
        let Flow::Reject { status, .. } = Flow::reject(42, "x") else {
            panic!("expected a rejection");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
