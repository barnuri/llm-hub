use axum::http::StatusCode;

/// What a request or attempt phase decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Go on with the next extension and then the request.
    Continue,
    /// Stop and answer the client with this status and message, in the client's API format.
    Reject {
        /// The HTTP status the client gets.
        status: StatusCode,
        /// The error text. The hub writes it in the client's API format.
        message: String,
    },
}

impl Flow {
    /// A rejection with `status` and `message`. A status that is not a 4xx or 5xx code becomes 400.
    #[must_use]
    pub fn reject(status: u16, message: impl Into<String>) -> Flow {
        let status = StatusCode::from_u16(status)
            .ok()
            .filter(|status| status.is_client_error() || status.is_server_error())
            .unwrap_or(StatusCode::BAD_REQUEST);
        Flow::Reject {
            status,
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
    fn an_invalid_or_non_error_status_becomes_bad_request() {
        for code in [42, 200, 302] {
            let Flow::Reject { status, .. } = Flow::reject(code, "x") else {
                panic!("expected a rejection");
            };
            assert_eq!(status, StatusCode::BAD_REQUEST, "{code}");
        }
    }
}
