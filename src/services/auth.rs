use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;

/// Auth for /v1/* and /api/*. Accepted credentials: the master key (env) or
/// any enabled hub api key (store). With neither configured, auth is
/// disabled (local-only tool) and any or no key is accepted.
pub async fn require_master_key(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let config = state.config();
    let hub_key_hashes = state.api_key_hashes();
    if config.master_key.is_none() && hub_key_hashes.is_empty() {
        return Ok(next.run(req).await);
    }
    let provided = extract_key(&req).ok_or(AppError::Unauthorized)?;
    let master_ok = config
        .master_key
        .as_deref()
        .is_some_and(|expected| keys_match(expected, &provided));
    let hub_key_ok = hub_key_hashes.contains(&crate::services::store::hash_key(&provided));
    if !master_ok && !hub_key_ok {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(req).await)
}

fn extract_key(req: &Request) -> Option<String> {
    let headers = req.headers();
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(token) = auth.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

/// Constant-time comparison over fixed-size digests so neither content
/// nor length of the configured key leaks through timing.
fn keys_match(expected: &str, provided: &str) -> bool {
    let expected_digest = Sha256::digest(expected.as_bytes());
    let provided_digest = Sha256::digest(provided.as_bytes());
    expected_digest.ct_eq(&provided_digest).into()
}

#[cfg(test)]
mod tests {
    use super::keys_match;

    #[test]
    fn matching_keys() {
        assert!(keys_match("secret", "secret"));
    }

    #[test]
    fn non_matching_keys() {
        assert!(!keys_match("secret", "wrong"));
        assert!(!keys_match("secret", ""));
        assert!(!keys_match("secret", "secret2"));
    }
}
