use std::error::Error;

/// Formats `error` with each cause after it, so a bare "error sending request"
/// shows the real reason (DNS, TLS, proxy, timeout) too.
#[must_use]
pub fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::error_chain;
    use std::fmt;

    #[derive(Debug)]
    struct Outer(Inner);
    #[derive(Debug)]
    struct Inner;

    impl fmt::Display for Outer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("error sending request")
        }
    }
    impl fmt::Display for Inner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("invalid peer certificate")
        }
    }
    impl std::error::Error for Inner {}
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn appends_every_cause() {
        assert_eq!(
            error_chain(&Outer(Inner)),
            "error sending request: invalid peer certificate"
        );
    }
}
