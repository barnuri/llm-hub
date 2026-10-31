use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::FutureExt;
use futures_util::future::BoxFuture;

use crate::services::gh_cli::{GhCli, GhOutcome};

/// Returns one scripted outcome and counts the calls, so tests never run gh.
#[derive(Debug)]
pub struct FakeGhCli {
    outcome: GhOutcome,
    calls: AtomicUsize,
}

impl FakeGhCli {
    pub fn new(outcome: GhOutcome) -> Self {
        Self {
            outcome,
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl GhCli for FakeGhCli {
    fn auth_token<'a>(
        &'a self,
        _gh_path: &'a str,
        _host: &'a str,
        _user: Option<&'a str>,
    ) -> BoxFuture<'a, GhOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(self.outcome.clone()).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_token_returns_scripted_outcome_and_counts_calls() {
        let gh = FakeGhCli::new(GhOutcome::Token("gho_test".to_string()));

        let first = gh.auth_token("gh", "github.com", None).await;
        let second = gh.auth_token("gh", "github.com", Some("octocat")).await;

        assert_eq!(first, GhOutcome::Token("gho_test".to_string()));
        assert_eq!(second, first);
        assert_eq!(gh.calls(), 2);
    }
}
