const WILDCARD: u8 = b'*';

/// True when `text` matches `glob`. `*` matches any run of characters, including `/`. Every other
/// byte matches itself.
pub fn glob_matches(glob: &str, text: &str) -> bool {
    let (glob, text) = (glob.as_bytes(), text.as_bytes());
    let (mut glob_at, mut text_at) = (0, 0);
    let mut last_star: Option<(usize, usize)> = None;
    while text_at < text.len() {
        match glob.get(glob_at) {
            Some(&WILDCARD) => {
                last_star = Some((glob_at, text_at));
                glob_at += 1;
            }
            Some(&byte) if byte == text[text_at] => {
                glob_at += 1;
                text_at += 1;
            }
            _ => {
                let Some((star_at, star_text_at)) = last_star else {
                    return false;
                };
                glob_at = star_at + 1;
                text_at = star_text_at + 1;
                last_star = Some((star_at, text_at));
            }
        }
    }
    glob[glob_at..].iter().all(|&byte| byte == WILDCARD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_prefixes_suffixes_middles_and_backtrack() {
        for (glob, text, expected) in [
            ("*", "", true),
            ("*", "anything", true),
            ("copilot/*", "copilot/gpt-5", true),
            ("copilot/*", "groq/x", false),
            ("*/claude-*", "copilot/claude-opus-4-5", true),
            ("/v1/responses", "/v1/responses", true),
            ("/v1/responses", "/v1/responses/x", false),
            ("a*b*c", "aXbYc", true),
            ("a*b*c", "aXcYb", false),
            ("ab*ba", "aba", false),
            ("*a*a*b", "aaab", true),
            ("gpt-6-sol", "gpt-6-sol", true),
            ("gpt-6-sol", "gpt-6-solar", false),
        ] {
            assert_eq!(glob_matches(glob, text), expected, "{glob} vs {text}");
        }
    }
}
