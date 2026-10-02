//! Which client harness sent a request, told apart by its headers.

use axum::http::HeaderMap;

const MAX_SIGNATURE_CHARS: usize = 160;
/// Copilot CLI speaks through the stock `OpenAI/JS` SDK; its `x-initiator`
/// header is what separates it from any other program using that SDK.
const COPILOT_MARKER: &str = "[x-initiator]";
pub const UNKNOWN_HARNESS: &str = "unknown";
const COPILOT_HARNESS: &str = "copilot-cli";

/// Compact per-request client signature: the User-Agent plus a marker when
/// Copilot CLI's `x-initiator` header is present. `None` without either.
#[must_use]
pub fn client_signature(headers: &HeaderMap) -> Option<String> {
    let user_agent = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    let mut signature: String = user_agent.chars().take(MAX_SIGNATURE_CHARS).collect();
    if headers.contains_key("x-initiator") {
        if !signature.is_empty() {
            signature.push(' ');
        }
        signature.push_str(COPILOT_MARKER);
    }
    (!signature.is_empty()).then_some(signature)
}

/// True when the request came from Copilot CLI.
#[must_use]
pub fn is_copilot_cli(headers: &HeaderMap) -> bool {
    classify(client_signature(headers).as_deref()) == COPILOT_HARNESS
}

/// Harness name for a stored client signature.
#[must_use]
pub fn classify(signature: Option<&str>) -> String {
    let Some(signature) = signature.map(str::trim).filter(|s| !s.is_empty()) else {
        return UNKNOWN_HARNESS.to_string();
    };
    let lower = signature.to_ascii_lowercase();
    let known = if lower.starts_with("claude-cli/") {
        Some(if lower.contains("agent-sdk") {
            "claude-agent-sdk"
        } else if lower.contains("sdk-cli") {
            "claude-code-headless"
        } else {
            "claude-code"
        })
    } else if lower.contains(&COPILOT_MARKER.to_ascii_lowercase()) {
        Some(COPILOT_HARNESS)
    } else if lower.starts_with("opencode/") {
        Some("opencode")
    } else if lower.starts_with("codex") {
        Some("codex")
    } else if lower.contains("cursor") {
        Some("cursor")
    } else if lower.starts_with("openai/js") {
        Some("openai-js-sdk")
    } else if lower.starts_with("bun/") {
        Some("bun-app")
    } else if lower.starts_with("curl/") {
        Some("curl")
    } else if lower.starts_with("python") {
        Some("python")
    } else {
        None
    };
    known.map_or_else(
        || {
            lower
                .split(['/', ' '])
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(UNKNOWN_HARNESS)
                .to_string()
        },
        str::to_string,
    )
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn signature__copilot_marker_appended() {
        let copilot = headers(&[("user-agent", "OpenAI/JS 5.20.1"), ("x-initiator", "agent")]);
        assert_eq!(
            client_signature(&copilot).as_deref(),
            Some("OpenAI/JS 5.20.1 [x-initiator]")
        );
        assert_eq!(client_signature(&headers(&[])), None);
    }

    #[test]
    fn is_copilot_cli__needs_the_x_initiator_header() {
        assert!(is_copilot_cli(&headers(&[
            ("user-agent", "OpenAI/JS 5.20.1"),
            ("x-initiator", "user"),
        ])));
        assert!(!is_copilot_cli(&headers(&[("user-agent", "OpenAI/JS 5.20.1")])));
        assert!(!is_copilot_cli(&headers(&[])));
    }

    #[test]
    fn classify__known_harnesses() {
        let cases = [
            ("claude-cli/2.1.285 (external, cli)", "claude-code"),
            (
                "claude-cli/2.1.283 (external, sdk-cli)",
                "claude-code-headless",
            ),
            (
                "claude-cli/2.1.280 (external, sdk-ts, agent-sdk/0.3.280)",
                "claude-agent-sdk",
            ),
            ("OpenAI/JS 5.20.1 [x-initiator]", "copilot-cli"),
            ("OpenAI/JS 5.20.1", "openai-js-sdk"),
            (
                "opencode/1.18.29 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14",
                "opencode",
            ),
            (
                "codex_exec/0.149.0 (Mac OS 26.5.2; arm64) vscode/1.141.0-insider (codex_exec; 0.149.0)",
                "codex",
            ),
            ("Bun/1.4.0", "bun-app"),
            ("curl/8.7.1", "curl"),
            ("Python-urllib/3.13", "python"),
            ("SomeTool/2.0", "sometool"),
        ];
        for (signature, harness) in cases {
            assert_eq!(classify(Some(signature)), harness, "{signature}");
        }
        assert_eq!(classify(None), UNKNOWN_HARNESS);
    }
}
