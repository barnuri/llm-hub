use crate::schemas::model_context::strip_window_suffix;
use crate::schemas::model_directive::ModelDirective;

/// Finds a model directive in the visible user text: "use opus" at the start of a clause,
/// "switch to gpt-6.1-sol", "route to sonnet" or "model: opus". The last directive wins.
/// Pure and in memory, one pass over the words.
#[derive(Debug)]
pub struct SmartAutoDirective;

impl SmartAutoDirective {
    const USE_VERB: &str = "use";
    const SWITCH_VERBS: [&str; 2] = ["switch", "route"];
    const SWITCH_PREPOSITION: &str = "to";
    const MODEL_LABELS: [&str; 2] = ["model:", "model="];
    /// Words that may come before "use" and still leave it at the start of a clause.
    const LEADS: [&str; 6] = ["please", "pls", "and", "then", "now", "just"];
    const FILLERS: [&str; 3] = ["the", "a", "model"];
    const AUTO_NAMES: [&str; 2] = ["auto", "smart-auto"];
    const CLAUSE_ENDS: [char; 6] = ['.', '!', '?', ';', ':', ','];
    const EDGE_PUNCTUATION: [char; 12] =
        ['.', ',', '!', '?', ';', ':', '"', '\'', '`', '(', ')', '*'];
    const NAME_SEPARATOR: char = '-';
    const PATH_SEPARATOR: char = '/';
    const MAX_NAME_WORDS: usize = 2;
    const MIN_NAME_LEN: usize = 3;

    /// The last model directive in `visible_text`, which must be lowercase without system
    /// reminders. `None` when the text names no model in a directive.
    #[must_use]
    pub fn parse(visible_text: &str) -> Option<ModelDirective> {
        let words: Vec<&str> = visible_text.split_whitespace().collect();
        (0..words.len())
            .rev()
            .find_map(|index| Self::directive_at(&words, index))
    }

    /// Whether `name` from a directive names `qualified_id`: the whole qualified id, the bare
    /// model id, or a run of its `-` separated parts, such as `opus` in `claude-opus-5-5`.
    #[must_use]
    pub fn names_model(name: &str, qualified_id: &str) -> bool {
        let id = Self::normalize(strip_window_suffix(qualified_id).0);
        let name = Self::normalize(name);
        if name.contains(Self::PATH_SEPARATOR) {
            return id == name || id.ends_with(&format!("{}{name}", Self::PATH_SEPARATOR));
        }
        let bare = id.rsplit(Self::PATH_SEPARATOR).next().unwrap_or_default();
        bare.match_indices(&name).any(|(start, found)| {
            let end = start + found.len();
            let opens = start == 0 || bare[..start].ends_with(Self::NAME_SEPARATOR);
            let closes = end == bare.len() || bare[end..].starts_with(Self::NAME_SEPARATOR);
            opens && closes
        })
    }

    fn directive_at(words: &[&str], index: usize) -> Option<ModelDirective> {
        let word = Self::clean(words[index]);
        if let Some(name) = Self::MODEL_LABELS
            .iter()
            .find_map(|label| word.strip_prefix(label))
            .filter(|name| !name.is_empty())
        {
            return Self::directive_from(&[name]);
        }
        let names_from = if word == Self::USE_VERB && Self::starts_clause(words, index) {
            index + 1
        } else if Self::SWITCH_VERBS.contains(&word)
            && words
                .get(index + 1)
                .is_some_and(|next| Self::clean(next) == Self::SWITCH_PREPOSITION)
        {
            index + 2
        } else if word == "model" && words[index].ends_with(':') {
            index + 1
        } else {
            return None;
        };
        Self::directive_from(&Self::name_words(words, names_from))
    }

    /// Up to `MAX_NAME_WORDS` words after `from`, past fillers, stopping at a clause end.
    fn name_words<'a>(words: &[&'a str], from: usize) -> Vec<&'a str> {
        let mut names = Vec::with_capacity(Self::MAX_NAME_WORDS);
        for raw in words.iter().skip(from) {
            let word = Self::clean(raw);
            if names.is_empty() && Self::FILLERS.contains(&word) {
                continue;
            }
            names.push(word);
            if names.len() == Self::MAX_NAME_WORDS || raw.ends_with(Self::CLAUSE_ENDS) {
                break;
            }
        }
        names
    }

    /// The directive for `names`, or `None` when the first name is not a model-like token.
    fn directive_from(names: &[&str]) -> Option<ModelDirective> {
        let first = *names.first()?;
        if !Self::is_model_token(first) {
            return None;
        }
        if Self::AUTO_NAMES.contains(&first) {
            return Some(ModelDirective::Auto);
        }
        let mut candidates = Vec::with_capacity(names.len());
        if let [first, second] = names
            && Self::is_model_token(second)
        {
            candidates.push(format!("{first}{}{second}", Self::NAME_SEPARATOR));
        }
        candidates.push(first.to_owned());
        Some(ModelDirective::Model(candidates))
    }

    /// At least `MIN_NAME_LEN` characters, one letter, and only id characters, so pasted code
    /// such as `use crate::x;` is not a directive.
    fn is_model_token(word: &str) -> bool {
        word.len() >= Self::MIN_NAME_LEN
            && word.bytes().any(|byte| byte.is_ascii_alphabetic())
            && word
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._/[]".contains(&byte))
    }

    fn starts_clause(words: &[&str], index: usize) -> bool {
        let follows_end = |at: usize| at == 0 || words[at - 1].ends_with(Self::CLAUSE_ENDS);
        follows_end(index)
            || (index > 0
                && Self::LEADS.contains(&Self::clean(words[index - 1]))
                && follows_end(index - 1))
    }

    fn clean(word: &str) -> &str {
        word.trim_matches(Self::EDGE_PUNCTUATION)
    }

    /// Lowercase, with `.` and `_` read as the `-` separator, so `sonnet-5.5` names `sonnet-5-5`.
    fn normalize(id: &str) -> String {
        id.to_ascii_lowercase().replace(['.', '_'], "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(names: &[&str]) -> ModelDirective {
        ModelDirective::Model(names.iter().map(|name| (*name).to_owned()).collect())
    }

    #[test]
    fn use_at_the_start_names_a_model() {
        assert_eq!(
            SmartAutoDirective::parse("use opus"),
            Some(model(&["opus"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("use gpt-6.1-sol for this"),
            Some(model(&["gpt-6.1-sol-for", "gpt-6.1-sol"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("please use the model sonnet."),
            Some(model(&["sonnet"]))
        );
    }

    #[test]
    fn use_after_a_clause_end_or_a_lead_word_names_a_model() {
        assert_eq!(
            SmartAutoDirective::parse("fix the bug. use opus"),
            Some(model(&["opus"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("fix it, and use opus"),
            Some(model(&["opus"]))
        );
    }

    #[test]
    fn use_inside_a_clause_is_not_a_directive() {
        assert_eq!(SmartAutoDirective::parse("why does it use opus"), None);
        assert_eq!(
            SmartAutoDirective::parse("why does it pick only haiku sonnet and opus"),
            None
        );
    }

    #[test]
    fn switch_route_and_model_label_name_a_model() {
        assert_eq!(
            SmartAutoDirective::parse("now switch to claude opus"),
            Some(model(&["claude-opus", "claude"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("route to codex"),
            Some(model(&["codex"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("model: copilot/gpt-6.1-sol"),
            Some(model(&["copilot/gpt-6.1-sol"]))
        );
        assert_eq!(
            SmartAutoDirective::parse("do it with model:opus"),
            Some(model(&["opus"]))
        );
    }

    #[test]
    fn auto_releases_the_choice() {
        assert_eq!(
            SmartAutoDirective::parse("use auto"),
            Some(ModelDirective::Auto)
        );
        assert_eq!(
            SmartAutoDirective::parse("switch to smart-auto"),
            Some(ModelDirective::Auto)
        );
    }

    #[test]
    fn the_last_directive_wins() {
        assert_eq!(
            SmartAutoDirective::parse("use opus. no wait, use sonnet"),
            Some(model(&["sonnet"]))
        );
    }

    #[test]
    fn code_and_short_words_are_not_directives() {
        assert_eq!(SmartAutoDirective::parse("use crate::schemas::x;"), None);
        assert_eq!(SmartAutoDirective::parse("use it"), None);
        assert_eq!(SmartAutoDirective::parse("use 123"), None);
        assert_eq!(SmartAutoDirective::parse("use"), None);
        assert_eq!(SmartAutoDirective::parse("switch the tab"), None);
        assert_eq!(SmartAutoDirective::parse(""), None);
    }

    #[test]
    fn a_second_word_that_is_not_a_token_is_dropped() {
        assert_eq!(
            SmartAutoDirective::parse("use opus {x}"),
            Some(model(&["opus"]))
        );
    }

    #[test]
    fn names_model_matches_whole_parts_only() {
        assert!(SmartAutoDirective::names_model(
            "opus",
            "copilot/claude-opus-5-5"
        ));
        assert!(SmartAutoDirective::names_model(
            "sonnet-5.5",
            "copilot/claude-sonnet-5-5"
        ));
        assert!(SmartAutoDirective::names_model(
            "gpt-6.1-sol",
            "copilot/gpt-6.1-sol"
        ));
        assert!(!SmartAutoDirective::names_model(
            "gpt-6.1-sol",
            "copilot/gpt-6.1-solar"
        ));
        assert!(!SmartAutoDirective::names_model(
            "pus",
            "copilot/claude-opus-5-5"
        ));
        assert!(SmartAutoDirective::names_model(
            "opus",
            "copilot/claude-opus-5-5[1m]"
        ));
    }

    #[test]
    fn names_model_with_a_profile_matches_the_qualified_id() {
        assert!(SmartAutoDirective::names_model(
            "copilot/gpt-6.1-sol",
            "copilot/gpt-6.1-sol"
        ));
        assert!(SmartAutoDirective::names_model(
            "bedrock/claude-opus-5-5",
            "llmgw/bedrock/claude-opus-5-5"
        ));
        assert!(!SmartAutoDirective::names_model(
            "llmgw/gpt-6.1-sol",
            "copilot/gpt-6.1-sol"
        ));
    }
}
